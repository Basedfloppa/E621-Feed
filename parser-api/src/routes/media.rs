//! Serve locally-stored original media (docs/offline-catalog.md).
//!
//! `GET /api/media/<post_id>?size=original` reads the post's entry from the
//! `media_entries` index and streams the bytes off disk. It is **read-only**:
//! it never fetches from e621 on demand — a post without a locally stored
//! original simply 404s. Original media is only placed here by the opt-in
//! in-server media worker or an explicit import.
//!
//! Responses are aggressively cacheable: a stored original is content-addressable
//! by post id (the file only changes when it is re-downloaded, which rewrites
//! the whole file), so every reply carries `Cache-Control: public,
//! max-age=31536000, immutable` and a strong `ETag` built from the file's
//! (mtime, size). Revalidation via `If-None-Match` answers `304 Not Modified`
//! without touching the DB LRU key or reading the file.
//!
//! Byte-range requests are supported (single ranges only, per RFC 7233): the
//! `206 Partial Content` body is streamed straight off disk via a seekable
//! window over the file — no whole-file buffering, so `<video>`/`<audio>`
//! seeking on multi-hundred-MB originals works without loading them into RAM.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use rocket::http::{ContentType, Header, Status};
use rocket::response::{self, Responder, Response};
use rocket::serde::json::Json;
use rocket::{Request, response::status::Custom};
use tokio::io::{AsyncRead, AsyncSeek, AsyncSeekExt as _, ReadBuf, SeekFrom};

use super::account::IfNoneMatch;
use e621_account_parser_api::{db, media_store, ratelimit::ClientIp};

/// Strong ETag derived from the file's (mtime, size). Both change only when
/// the original is re-downloaded, so the tag is stable across serves and
/// invalidates exactly when the content could actually change.
fn etag_for(mtime: u64, len: u64) -> String {
    format!("\"{mtime:x}-{len:x}\"")
}

/// Stored originals are content-addressed by post id: once written, the file
/// for a given post never changes unless it is fully re-downloaded, so an
/// immutable, year-long cache is safe (and saves the LAN from re-reading
/// every card on every page view).
const CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// Streams a file from disk with an inferred content type.
pub(crate) struct FileBody {
    file: tokio::fs::File,
    len: u64,
    ct: ContentType,
    etag: String,
}

/// `304 Not Modified` reply — same cache headers as the 200, no body.
pub(crate) struct NotModified {
    etag: String,
}

/// `206 Partial Content` reply: a seekable window over the file, streamed
/// without loading the whole file into memory.
pub(crate) struct RangeFileBody {
    stream: RangeStream,
    len: u64,
    ct: ContentType,
    etag: String,
    content_range: String,
}

/// `416 Range Not Satisfiable` — the requested range is entirely past EOF.
pub(crate) struct UnsatisfiableRange {
    total: u64,
}

/// What `serve_media` returns: bytes, a partial range, or a conditional
/// reply. Kept as a single type so the route's success/error shape stays
/// simple (`Result<MediaBody, RouteErr>`). The body variants are boxed to
/// keep the enum small (`tokio::fs::File` is ~320 bytes).
pub(crate) enum MediaBody {
    File(Box<FileBody>),
    Partial(Box<RangeFileBody>),
    NotModified(NotModified),
    Unsatisfiable(UnsatisfiableRange),
}

/// `AsyncRead + AsyncSeek` window over `[start, start+len)` of a file — what
/// Rocket's `sized_body` needs for a partial response (it requires a seekable
/// async reader so the content length is known up front).
struct RangeStream {
    file: tokio::fs::File,
    /// Absolute file offset of the range start.
    start: u64,
    /// Bytes of the range still to be read.
    remaining: u64,
    /// Total range length (for seek clamping).
    total: u64,
}

impl RangeStream {
    async fn open(path: &Path, start: u64, len: u64) -> io::Result<Self> {
        let mut file = tokio::fs::File::open(path).await?;
        file.seek(SeekFrom::Start(start)).await?;
        Ok(RangeStream {
            file,
            start,
            remaining: len,
            total: len,
        })
    }
}

impl AsyncRead for RangeStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.remaining == 0 {
            return Poll::Ready(Ok(()));
        }
        let max = buf.remaining().min(self.remaining as usize);
        let mut limited = buf.take(max);
        match Pin::new(&mut self.file).poll_read(cx, &mut limited) {
            Poll::Ready(Ok(())) => {
                let n = limited.filled().len();
                // `ReadBuf::take` hands the inner reader a fresh sub-buffer;
                // propagate its initialized region back to the parent before
                // advancing (same dance tokio's `Take` does).
                unsafe { buf.assume_init(n) };
                buf.advance(n);
                self.remaining -= n as u64;
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl AsyncSeek for RangeStream {
    fn start_seek(mut self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let current_rel = self.total - self.remaining;
        let rel = match position {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(e) => self.total as i64 + e,
            SeekFrom::Current(c) => current_rel as i64 + c,
        };
        let rel = rel.clamp(0, self.total as i64) as u64;
        self.remaining = self.total - rel;
        let abs = self.start + rel;
        Pin::new(&mut self.file).start_seek(SeekFrom::Start(abs))
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Pin::new(&mut self.file).poll_complete(cx)
    }
}

fn content_type_for(path: &Path) -> ContentType {
    let ext: Option<String> = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("jpg" | "jpeg") => ContentType::JPEG,
        Some("png") => ContentType::PNG,
        Some("gif") => ContentType::GIF,
        Some("webp") => ContentType::WEBP,
        Some("webm") => ContentType::new("video", "webm"),
        Some("mp4") => ContentType::new("video", "mp4"),
        Some("mp3") => ContentType::new("audio", "mpeg"),
        Some("flac") => ContentType::new("audio", "flac"),
        Some("ogg") => ContentType::new("audio", "ogg"),
        Some("zip") => ContentType::new("application", "zip"),
        Some("swf") => ContentType::new("application", "x-shockwave-flash"),
        Some("txt") => ContentType::Plain,
        _ => ContentType::new("application", "octet-stream"),
    }
}

impl<'r> Responder<'r, 'static> for FileBody {
    fn respond_to(self, _req: &'r Request<'_>) -> response::Result<'static> {
        Response::build()
            .status(Status::Ok)
            .header(self.ct)
            .header(Header::new("Accept-Ranges", "bytes"))
            .header(Header::new("Cache-Control", CACHE_CONTROL))
            .header(Header::new("ETag", self.etag))
            .sized_body(self.len as usize, self.file)
            .ok()
    }
}

impl<'r> Responder<'r, 'static> for RangeFileBody {
    fn respond_to(self, _req: &'r Request<'_>) -> response::Result<'static> {
        Response::build()
            .status(Status::PartialContent)
            .header(self.ct)
            .header(Header::new("Accept-Ranges", "bytes"))
            .header(Header::new("Cache-Control", CACHE_CONTROL))
            .header(Header::new("ETag", self.etag))
            .header(Header::new("Content-Range", self.content_range))
            .sized_body(self.len as usize, self.stream)
            .ok()
    }
}

impl<'r> Responder<'r, 'static> for NotModified {
    fn respond_to(self, _req: &'r Request<'_>) -> response::Result<'static> {
        // A 304 must not carry a body (RFC 7232 §4.1).
        Response::build()
            .status(Status::NotModified)
            .header(Header::new("Accept-Ranges", "bytes"))
            .header(Header::new("Cache-Control", CACHE_CONTROL))
            .header(Header::new("ETag", self.etag))
            .ok()
    }
}

impl<'r> Responder<'r, 'static> for UnsatisfiableRange {
    fn respond_to(self, _req: &'r Request<'_>) -> response::Result<'static> {
        Response::build()
            .status(Status::RangeNotSatisfiable)
            .header(Header::new("Accept-Ranges", "bytes"))
            .header(Header::new(
                "Content-Range",
                format!("bytes */{}", self.total),
            ))
            .ok()
    }
}

impl<'r> Responder<'r, 'static> for MediaBody {
    fn respond_to(self, req: &'r Request<'_>) -> response::Result<'static> {
        match self {
            MediaBody::File(f) => f.respond_to(req),
            MediaBody::Partial(p) => p.respond_to(req),
            MediaBody::NotModified(nm) => nm.respond_to(req),
            MediaBody::Unsatisfiable(u) => u.respond_to(req),
        }
    }
}

type RouteErr = Custom<Json<serde_json::Value>>;

fn not_found(detail: &str) -> RouteErr {
    Custom(
        Status::NotFound,
        Json(serde_json::json!({ "error": "not_found", "detail": detail })),
    )
}

fn unavailable(detail: &str) -> RouteErr {
    Custom(
        Status::UnprocessableEntity,
        Json(serde_json::json!({ "error": "media_unavailable", "detail": detail })),
    )
}

/// Does `If-None-Match` say our ETag is still fresh? Handles `*` and a bare
/// exact match (weak `W/` prefixes are stripped — fine for this content).
fn if_none_match_is_fresh(inm: Option<&str>, etag: &str) -> bool {
    let Some(inm) = inm else {
        return false;
    };
    let expected = etag.trim_start_matches("W/");
    inm.split(',')
        .map(str::trim)
        .map(|t| t.trim_start_matches("W/"))
        .any(|t| t == "*" || t == expected)
}

/// Result of parsing one `Range: bytes=…` header.
enum RangeParse {
    /// No usable range (absent, unknown unit, multi-range, malformed) — the
    /// request is served as a full 200 (RFC 7233 §3.1).
    Ignore,
    /// Satisfiable inclusive range `(start, end)` — serve 206.
    Satisfiable(u64, u64),
    /// Syntactically valid but entirely past EOF — serve 416.
    Unsatisfiable,
}

/// Parse a single byte range from a `Range` header. Only `bytes=` single
/// ranges are served; everything else is ignored per RFC 7233.
fn parse_range(header: &str, total: u64) -> RangeParse {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeParse::Ignore;
    };
    let spec = spec.trim();
    if spec.is_empty() || spec.contains(',') {
        // Multi-range / empty: we serve only single ranges.
        return RangeParse::Ignore;
    }
    let Some((start_s, end_s)) = spec.split_once('-') else {
        return RangeParse::Ignore;
    };
    let start_s = start_s.trim();
    let end_s = end_s.trim();
    let last = total.saturating_sub(1);
    let (start, end) = match (start_s.is_empty(), end_s.is_empty()) {
        (false, false) => match (start_s.parse::<u64>(), end_s.parse::<u64>()) {
            (Ok(s), Ok(e)) => (s, e.min(last)),
            _ => return RangeParse::Ignore,
        },
        // `bytes=start-` → open-ended.
        (false, true) => match start_s.parse::<u64>() {
            Ok(s) => (s, last),
            Err(_) => return RangeParse::Ignore,
        },
        // `bytes=-N` → last N bytes; `-0` is unsatisfiable.
        (true, false) => match end_s.parse::<u64>() {
            Ok(0) => return RangeParse::Unsatisfiable,
            Ok(n) => (total.saturating_sub(n), last),
            Err(_) => return RangeParse::Ignore,
        },
        (true, true) => return RangeParse::Ignore,
    };
    if start > end {
        RangeParse::Unsatisfiable
    } else {
        RangeParse::Satisfiable(start, end)
    }
}

/// Serve one locally-stored original file.
///
/// Deliberately unauthenticated (so plain `<img>`/`<video>` tags work against
/// the local proxy), but per-IP rate-limited. The media index is shared across
/// accounts, so anyone with network access to this server can read stored
/// originals — documented tradeoff for the offline-serve feature.
#[get("/media/<post_id>?<size>")]
pub(crate) async fn serve_media(
    post_id: i64,
    size: Option<String>,
    client_ip: ClientIp,
    if_none_match: IfNoneMatch,
    range: RangeHeader,
) -> Result<MediaBody, RouteErr> {
    // Rate-limit even this read-only route: every hit does a DB index lookup
    // and (on success) a touch-write, so bound it per IP. Raised to
    // 240/min × burst 480 so a full catalog grid (50 cards) plus pagination
    // can load without tripping the bucket; still bounded against scraping.
    if e621_account_parser_api::ratelimit::check(&format!("media:{}", client_ip.0), 240, 480)
        .is_err()
    {
        return Err(unavailable("rate limited"));
    }
    // Only originals are stored; anything else requested is a client mistake.
    if matches!(size.as_deref(), Some(s) if s != "original" && !s.is_empty()) {
        let s = size.unwrap_or_default();
        return Err(not_found(&format!(
            "media size '{s}' is never stored locally"
        )));
    }
    if post_id <= 0 {
        return Err(not_found("invalid post id"));
    }
    let entry = match db::get_media_entry(post_id) {
        Ok(Some(e)) => e,
        Ok(None) => return Err(not_found("no locally-stored original for this post")),
        Err(e) => return Err(unavailable(&format!("media index error: {e}"))),
    };
    let (rel_path, _mtime) = entry;
    let path = match media_store::stored_path(post_id, &rel_path) {
        Some(p) => p,
        None => return Err(not_found("indexed file missing from disk")),
    };
    // Stat BEFORE the conditional check: the ETag is (file_mtime, size), and
    // the DB `media_entries.mtime` LRU key is intentionally NOT used because
    // it is bumped on every serve (which would churn the ETag and defeat
    // caching).
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return Err(unavailable(&format!("unable to stat media file: {e}"))),
    };
    let len = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if len == 0 {
        return Err(not_found("stored media file is empty"));
    }
    let etag = etag_for(mtime, len);

    // Conditional GET: client already has this exact file → 304, no body, no
    // LRU write, no disk read. `If-None-Match` takes precedence over `Range`
    // (RFC 7232 §6), so this check runs first.
    if if_none_match_is_fresh(if_none_match.header_value(), &etag) {
        return Ok(MediaBody::NotModified(NotModified { etag }));
    }

    // Byte-range request → 206 Partial Content streamed off disk.
    if let Some(range_hdr) = range.header_value() {
        match parse_range(range_hdr, len) {
            RangeParse::Satisfiable(start, end) => {
                let range_len = end - start + 1;
                let stream = match RangeStream::open(&path, start, range_len).await {
                    Ok(s) => s,
                    Err(e) => return Err(unavailable(&format!("unable to open media file: {e}"))),
                };
                // Bytes served → the file is clearly in use.
                let _ = db::touch_media_entry(post_id, chrono::Utc::now().timestamp());
                return Ok(MediaBody::Partial(Box::new(RangeFileBody {
                    stream,
                    len: range_len,
                    ct: content_type_for(&path),
                    etag,
                    content_range: format!("bytes {start}-{end}/{len}"),
                })));
            }
            RangeParse::Unsatisfiable => {
                return Ok(MediaBody::Unsatisfiable(UnsatisfiableRange { total: len }));
            }
            RangeParse::Ignore => {} // fall through to a full 200
        }
    }

    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => return Err(unavailable(&format!("unable to open media file: {e}"))),
    };
    // Refresh LRU so frequently-viewed posts are evicted last (bytes served,
    // so the file is clearly in use).
    let _ = db::touch_media_entry(post_id, chrono::Utc::now().timestamp());
    Ok(MediaBody::File(Box::new(FileBody {
        file,
        len,
        ct: content_type_for(&path),
        etag,
    })))
}

/// Raw `Range` request header, if present (same pattern as `IfNoneMatch`).
pub(crate) struct RangeHeader(Option<String>);

impl RangeHeader {
    fn header_value(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

#[rocket::async_trait]
impl<'r> rocket::request::FromRequest<'r> for RangeHeader {
    type Error = std::convert::Infallible;
    async fn from_request(req: &'r Request<'_>) -> rocket::request::Outcome<Self, Self::Error> {
        rocket::request::Outcome::Success(RangeHeader(
            req.headers()
                .get_one("Range")
                .map(std::string::ToString::to_string),
        ))
    }
}
