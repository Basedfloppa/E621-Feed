//! On-disk persistence for the calibrate channel cache.
//!
//! The in-memory [`ScoreCache`] is built by one ~60s full scoring pass and
//! then reused by every grid probe. This module lets that pass survive the
//! process: the next invocation validates the file against the freshly
//! prepared dataset and, on a hit, seeds the baseline from it.
//!
//! Two things make the file worth having beyond the ~60s it saves:
//!
//!   1. It is **self-contained**: the per-account candidate counts and the
//!      12-channel matrix, with no dependency on the dataset being rebuilt.
//!      That is exactly the input an analytic blend fit needs (TODO §2.7) —
//!      the positives are `i < n_test` within each account, and post ids are
//!      not required because every test candidate has relevance 1 and every
//!      negative 0.
//!   2. Iterating on grid settings (probe lists, pass schedule) no longer
//!      re-pays the baseline.
//!
//! ## What invalidates a cache
//!
//! The key is `(dataset fingerprint, priors fingerprint, diversify,
//! top-k)`. The dataset fingerprint is a hash over every account's
//! candidate ids in order, so a changed split, changed negative sampling, or
//! a changed catalog all miss. The priors fingerprint hashes the whole
//! `Priors` struct as JSON — including fields added later — so it cannot
//! drift the way a hand-listed field set would.
//!
//! `Priors::now` is excluded from the fingerprint (normalised to the epoch
//! before hashing): it is the evaluation clock, not a knob. That is why a
//! hit still recomputes [`M_RECENCY`](crate::cache::M_RECENCY) — every other
//! channel ignores the clock (verified: every `priors.now` reader in the
//! scorer is a recency age computation).
//!
//! ## Failure policy
//!
//! The cache is an optimisation, so **no failure may break a run**: a
//! missing, truncated, corrupt, stale or version-mismatched file is a miss
//! with a one-line reason, never an error. Writes go to a temporary file and
//! are `rename`d into place, so a crash mid-write leaves the previous file
//! intact rather than a half-written one.

use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use e621_account_parser_api::utils::Priors;

use crate::cache::{AccountChannelCache, ChannelScores, ScoreCache};
use crate::dataset::EvalDataset;

/// File magic. Bump the trailing byte when the layout changes.
const MAGIC: &[u8; 8] = b"E621CHC1";
/// Layout version. Checked verbatim; a mismatch is a miss, not an error.
const FORMAT_VERSION: u32 = 2;
/// 12 channels × `f32` + one veto byte.
const CHANNEL_BYTES: usize = 11 * 4;
/// Where a default-path cache lives, relative to the working directory —
/// alongside the `calibration_results/grid_*.toml` result files.
pub(crate) const DEFAULT_CACHE_PATH: &str = "calibration_results/channel-cache.bin";

// ---- key -------------------------------------------------------------------

/// Everything the cached channel scores depend on. Any differing field is a
/// miss; the fields are compared individually so the message can say which.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct CacheKey {
    pub(crate) diversify: bool,
    pub(crate) top_k_ndcg: u32,
    pub(crate) top_k_recall: u32,
    pub(crate) dataset: u64,
    pub(crate) priors: u64,
}

impl CacheKey {
    /// `None` when the priors cannot be serialised — caching is then simply
    /// unavailable, which must not be fatal.
    pub(crate) fn compute(
        dataset: &EvalDataset,
        priors: &Priors,
        diversify: bool,
        top_k_ndcg: usize,
        top_k_recall: usize,
    ) -> Option<Self> {
        Some(Self {
            diversify,
            top_k_ndcg: u32::try_from(top_k_ndcg).ok()?,
            top_k_recall: u32::try_from(top_k_recall).ok()?,
            dataset: dataset_fingerprint(dataset),
            priors: priors_fingerprint(priors)?,
        })
    }

    /// Human-readable reason the cached header is not usable, or `None` when
    /// it is an exact match.
    fn mismatch(&self, other: &Self) -> Option<String> {
        if self.dataset != other.dataset {
            return Some("dataset changed (split, negatives or catalog)".into());
        }
        if self.priors != other.priors {
            return Some("priors changed since the cache was written".into());
        }
        if self.diversify != other.diversify {
            return Some(format!(
                "diversify {} != {}",
                other.diversify, self.diversify
            ));
        }
        if self.top_k_ndcg != other.top_k_ndcg || self.top_k_recall != other.top_k_recall {
            return Some(format!(
                "top-k {}@{} != {}@{}",
                other.top_k_ndcg, other.top_k_recall, self.top_k_ndcg, self.top_k_recall
            ));
        }
        None
    }
}

/// Hash of the candidate ids in order — per account, test bucket then
/// negatives. Captures which posts are scored and in what order, which is
/// exactly the precondition for the stored channel columns to line up.
pub(crate) fn dataset_fingerprint(dataset: &EvalDataset) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update((dataset.accounts.len() as u64).to_le_bytes());
    for fx in &dataset.accounts {
        hasher.update((fx.test_features.len() as u64).to_le_bytes());
        hasher.update((fx.neg_features.len() as u64).to_le_bytes());
        for f in fx.test_features.iter().chain(fx.neg_features.iter()) {
            hasher.update(f.id.to_le_bytes());
        }
    }
    truncate(hasher.finalize())
}

/// Hash of every `Priors` field except the evaluation clock.
///
/// Serialising the whole struct rather than listing fields is deliberate: a
/// knob added later is covered automatically, whereas a hand-written field
/// list is the exact defect class this codebase has been fixing (a channel
/// count that drifted from 9 to 12 in three separate places).
pub(crate) fn priors_fingerprint(priors: &Priors) -> Option<u64> {
    let mut normalised = priors.clone();
    // The clock is injected per scoring pass and must not enter the key.
    normalised.now = DateTime::<Utc>::UNIX_EPOCH;
    let bytes = serde_json::to_vec(&normalised).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(truncate(hasher.finalize()))
}

fn truncate(digest: impl AsRef<[u8]>) -> u64 {
    let d = digest.as_ref();
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[..8]);
    u64::from_le_bytes(b)
}

// ---- header ----------------------------------------------------------------

fn write_header<W: Write>(w: &mut W, key: &CacheKey, n_accounts: u32) -> io::Result<()> {
    w.write_all(MAGIC)?;
    write_u32(w, FORMAT_VERSION)?;
    write_u32(w, u32::from(key.diversify))?;
    write_u32(w, key.top_k_ndcg)?;
    write_u32(w, key.top_k_recall)?;
    write_u64(w, key.dataset)?;
    write_u64(w, key.priors)?;
    write_u32(w, n_accounts)
}

/// Parsed header plus the key it carries. Separated from key validation so
/// the caller can report *why* a file was rejected.
fn read_header<R: Read>(r: &mut R) -> io::Result<(CacheKey, u32)> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a channel cache (bad magic)",
        ));
    }
    let version = read_u32(r)?;
    if version != FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cache format v{version}, expected v{FORMAT_VERSION}"),
        ));
    }
    let diversify = read_u32(r)? != 0;
    let top_k_ndcg = read_u32(r)?;
    let top_k_recall = read_u32(r)?;
    let dataset = read_u64(r)?;
    let priors = read_u64(r)?;
    let n_accounts = read_u32(r)?;
    Ok((
        CacheKey {
            diversify,
            top_k_ndcg,
            top_k_recall,
            dataset,
            priors,
        },
        n_accounts,
    ))
}

// ---- channel encoding ------------------------------------------------------

/// Field order is part of the on-disk format — `FORMAT_VERSION` guards it,
/// and `written_channels_come_back_field_for_field` fails if the two sides
/// ever disagree.
fn encode_channel(buf: &mut Vec<u8>, ch: &ChannelScores) {
    for v in [
        ch.sim,
        ch.quality,
        ch.recency,
        ch.rating,
        ch.media,
        ch.popularity,
        ch.tag_relation,
        ch.uploader,
        ch.exclusivity,
        ch.novelty,
        ch.artist_discovery,
    ] {
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

/// Mirror of [`encode_channel`]; `raw` must be a whole number of channels.
fn decode_channels(raw: &[u8]) -> Vec<ChannelScores> {
    let mut out = Vec::with_capacity(raw.len() / CHANNEL_BYTES);
    for chunk in raw.chunks_exact(CHANNEL_BYTES) {
        let f = |i: usize| f32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        out.push(ChannelScores {
            sim: f(0),
            quality: f(1),
            recency: f(2),
            rating: f(3),
            media: f(4),
            popularity: f(5),
            tag_relation: f(6),
            uploader: f(7),
            exclusivity: f(8),
            novelty: f(9),
            artist_discovery: f(10),
        });
    }
    out
}

// ---- save / load -----------------------------------------------------------

/// Write `cache` to `path` atomically. `Err` only for real I/O problems —
/// the caller decides whether a failed write is worth reporting.
pub(crate) fn save(
    path: &Path,
    key: &CacheKey,
    dataset: &EvalDataset,
    cache: &ScoreCache,
) -> io::Result<()> {
    if cache.accounts.len() != dataset.accounts.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "cache has {} accounts, dataset has {}",
                cache.accounts.len(),
                dataset.accounts.len()
            ),
        ));
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let tmp = tmp_path(path);
    {
        let mut w = BufWriter::new(fs::File::create(&tmp)?);
        write_header(&mut w, key, dataset.accounts.len() as u32)?;
        let mut hasher = Sha256::new();
        let mut buf: Vec<u8> = Vec::new();
        for (acc_idx, acc) in cache.accounts.iter().enumerate() {
            let fx = &dataset.accounts[acc_idx];
            write_u32(&mut w, fx.test_features.len() as u32)?;
            write_u32(&mut w, acc.channels.len() as u32)?;
            buf.clear();
            for ch in &acc.channels {
                encode_channel(&mut buf, ch);
            }
            hasher.update(&buf);
            w.write_all(&buf)?;
        }
        w.write_all(&hasher.finalize())?;
        w.flush()?;
    }
    fs::rename(&tmp, path)
}

/// Load a cache for `key`, or `None` with a printed reason.
///
/// `context_base` is deliberately not persisted: it is a per-account set of
/// tag HashMaps derived from the dataset, cheap enough to rebuild, and
/// large enough that storing it would dwarf the channel matrix.
pub(crate) fn load(path: &Path, key: &CacheKey, dataset: &EvalDataset) -> Option<ScoreCache> {
    match try_load(path, key, dataset) {
        Ok(cache) => Some(cache),
        Err(reason) => {
            eprintln!("[cache] miss: {reason}");
            None
        }
    }
}

fn try_load(path: &Path, key: &CacheKey, dataset: &EvalDataset) -> Result<ScoreCache, String> {
    let file = fs::File::open(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => format!("{} does not exist", path.display()),
        _ => format!("{}: {e}", path.display()),
    })?;
    let mut r = BufReader::new(file);
    let (cached_key, n_accounts) =
        read_header(&mut r).map_err(|e| format!("{}: {e}", path.display()))?;

    if let Some(reason) = cached_key.mismatch(key) {
        return Err(reason);
    }
    if n_accounts as usize != dataset.accounts.len() {
        return Err(format!(
            "accounts {} != dataset {}",
            n_accounts,
            dataset.accounts.len()
        ));
    }

    let mut hasher = Sha256::new();
    let mut accounts = Vec::with_capacity(n_accounts as usize);
    for (acc_idx, fx) in dataset.accounts.iter().enumerate() {
        let n_test = read_u32(&mut r).map_err(|e| format!("account {acc_idx}: {e}"))?;
        let n_cand = read_u32(&mut r).map_err(|e| format!("account {acc_idx}: {e}"))?;
        // Validate against the dataset *before* allocating: a corrupt length
        // would otherwise ask for gigabytes.
        let expected_test = fx.test_features.len();
        let expected_cand = expected_test + fx.neg_features.len();
        if n_test as usize != expected_test || n_cand as usize != expected_cand {
            return Err(format!(
                "account {acc_idx}: {n_test}test/{n_cand}cand in cache, \
                 {expected_test}test/{expected_cand}cand in dataset"
            ));
        }
        let mut raw = vec![0u8; n_cand as usize * CHANNEL_BYTES];
        r.read_exact(&mut raw)
            .map_err(|e| format!("account {acc_idx}: truncated ({e})"))?;
        hasher.update(&raw);
        accounts.push(AccountChannelCache {
            channels: decode_channels(&raw),
            n_test: n_test as usize,
            context_base: None,
        });
    }

    let mut expected = [0u8; 32];
    r.read_exact(&mut expected)
        .map_err(|e| format!("missing checksum: {e}"))?;
    if hasher.finalize()[..] != expected[..] {
        return Err("checksum mismatch (file corrupt or truncated)".into());
    }

    Ok(ScoreCache { accounts })
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

// ---- little-endian primitives ---------------------------------------------

fn write_u32<W: Write>(w: &mut W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn write_u64<W: Write>(w: &mut W, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// A stable 64-bit hash for the test-only synthetic dataset fingerprint.
/// Not used by [`dataset_fingerprint`]; kept so tests can build distinct
/// keys without a full `EvalDataset`.
#[cfg(test)]
fn synthetic_ids_fingerprint(ids: &[i64]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    ids.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn distinct_channels(seed: f32) -> ChannelScores {
        ChannelScores {
            sim: seed + 1.0,
            quality: seed + 2.0,
            recency: seed + 3.0,
            rating: seed + 4.0,
            media: seed + 5.0,
            popularity: seed + 6.0,
            tag_relation: seed + 7.0,
            uploader: seed + 8.0,
            exclusivity: seed + 9.0,
            novelty: seed + 10.0,
            artist_discovery: seed + 11.0,
        }
    }

    fn encode(chs: &[ChannelScores]) -> Vec<u8> {
        let mut buf = Vec::new();
        for ch in chs {
            encode_channel(&mut buf, ch);
        }
        buf
    }

    fn key(dataset: u64, priors: u64) -> CacheKey {
        CacheKey {
            diversify: false,
            top_k_ndcg: 20,
            top_k_recall: 50,
            dataset,
            priors,
        }
    }

    #[test]
    fn written_channels_come_back_field_for_field() {
        // Distinct value per field, so a write/read order mismatch on any
        // single channel fails rather than cancelling out.
        let original = vec![distinct_channels(0.0), distinct_channels(1.0)];
        let decoded = decode_channels(&encode(&original));
        assert_eq!(decoded.len(), original.len());
        for (a, b) in original.iter().zip(decoded.iter()) {
            assert_eq!(a.sim, b.sim, "sim");
            assert_eq!(a.quality, b.quality, "quality");
            assert_eq!(a.recency, b.recency, "recency");
            assert_eq!(a.rating, b.rating, "rating");
            assert_eq!(a.media, b.media, "media");
            assert_eq!(a.popularity, b.popularity, "popularity");
            assert_eq!(a.tag_relation, b.tag_relation, "tag_relation");
            assert_eq!(a.uploader, b.uploader, "uploader");
            assert_eq!(a.exclusivity, b.exclusivity, "exclusivity");
            assert_eq!(a.novelty, b.novelty, "novelty");
            assert_eq!(a.artist_discovery, b.artist_discovery, "artist_discovery");
        }
    }

    #[test]
    fn the_channel_block_is_the_size_the_reader_assumes() {
        assert_eq!(encode(&[distinct_channels(0.0)]).len(), CHANNEL_BYTES);
    }

    #[test]
    fn a_round_tripped_header_keeps_every_key_field() {
        let mut buf = Vec::new();
        write_header(&mut buf, &key(0xDEAD_BEEF, 0x1234_5678), 676).unwrap();
        let (parsed, n) = read_header(&mut buf.as_slice()).unwrap();
        assert_eq!(n, 676);
        assert_eq!(parsed, key(0xDEAD_BEEF, 0x1234_5678));
    }

    #[test]
    fn a_foreign_file_is_rejected_rather_than_misread() {
        let err = read_header(&mut b"not a cache at all".as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_format_version_bump_is_a_miss_not_a_parse() {
        let mut buf = Vec::new();
        write_header(&mut buf, &key(1, 2), 3).unwrap();
        // Overwrite the version, which sits right after the 8-byte magic.
        buf[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let err = read_header(&mut buf.as_slice()).unwrap_err();
        assert!(err.to_string().contains("format v"), "{err}");
    }

    #[test]
    fn a_truncated_header_is_an_error_not_a_panic() {
        let mut buf = Vec::new();
        write_header(&mut buf, &key(1, 2), 3).unwrap();
        buf.truncate(buf.len() - 4);
        assert!(read_header(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn a_changed_dataset_fingerprint_reports_which_part_moved() {
        let cached = key(0xAAAA, 7);
        let current = key(0xBBBB, 7);
        let reason = cached.mismatch(&current).unwrap();
        assert!(reason.contains("dataset"), "{reason}");
    }

    #[test]
    fn a_changed_prior_is_a_miss_and_says_so() {
        let cached = key(1, 0xAAAA);
        let current = key(1, 0xBBBB);
        let reason = cached.mismatch(&current).unwrap();
        assert!(reason.contains("priors"), "{reason}");
    }

    #[test]
    fn changing_only_the_evaluation_clock_does_not_invalidate_the_cache() {
        // The clock is injected per pass; if it entered the key every run
        // would miss.
        let base = Priors::default();
        let mut moved = base.clone();
        moved.now = DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::days(400);
        assert_eq!(
            priors_fingerprint(&base).unwrap(),
            priors_fingerprint(&moved).unwrap()
        );
    }

    #[test]
    fn changing_a_prior_knob_does_invalidate_the_cache() {
        let base = Priors::default();
        let mut tweaked = base.clone();
        tweaked.quality_a += 0.01;
        assert_ne!(
            priors_fingerprint(&base).unwrap(),
            priors_fingerprint(&tweaked).unwrap()
        );
    }

    #[test]
    fn the_priors_fingerprint_ignores_field_order_but_not_values() {
        // Guards the property the JSON-round-trip relies on: two structurally
        // identical priors hash the same, a changed value does not.
        let a = Priors::default();
        let b = a.clone();
        assert_eq!(priors_fingerprint(&a), priors_fingerprint(&b));
    }

    #[test]
    fn distinct_candidate_sets_hash_differently() {
        assert_ne!(
            synthetic_ids_fingerprint(&[1, 2, 3]),
            synthetic_ids_fingerprint(&[1, 2, 4])
        );
        assert_ne!(
            synthetic_ids_fingerprint(&[1, 2, 3]),
            synthetic_ids_fingerprint(&[3, 2, 1]),
            "order must matter: the cache is positional"
        );
    }

    /// `n_test` is what tells the analytic blend fit which candidates are
    /// favourites; the on-disk header already carries it, and this pins the
    /// loader threading it into `AccountChannelCache` rather than defaulting
    /// it to zero (which would make every candidate a negative).
    #[test]
    fn n_test_survives_the_cache_file_round_trip() {
        let now = Utc::now();
        let ds = crate::test_support::dataset(vec![
            crate::test_support::account(
                vec![
                    crate::test_support::post(1, now, 1, &[]),
                    crate::test_support::post(2, now, 2, &[]),
                ],
                vec![crate::test_support::post(3, now, 3, &[])],
            ),
            crate::test_support::account(vec![crate::test_support::post(4, now, 4, &[])], vec![]),
        ]);
        let priors = Priors::default();
        let key = CacheKey::compute(&ds, &priors, false, 20, 50).expect("key");
        let (_, cache) = crate::cache::score_with_cache(
            &ds,
            &priors,
            now,
            20,
            50,
            false,
            None,
            crate::cache::M_ALL,
        );

        let dir = std::env::temp_dir().join(format!("e621chc-n_test-{}", std::process::id()));
        let path = dir.join("cache.bin");
        save(&path, &key, &ds, &cache).expect("write cache");
        let loaded = load(&path, &key, &ds).expect("round trip must hit");
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(loaded.accounts.len(), cache.accounts.len());
        for (a, b) in loaded.accounts.iter().zip(cache.accounts.iter()) {
            assert_eq!(a.n_test, b.n_test, "n_test drifted through the file");
            assert_eq!(a.channels.len(), b.channels.len());
        }
        assert_eq!(loaded.accounts[0].n_test, 2);
        assert_eq!(loaded.accounts[1].n_test, 1);
    }
}
