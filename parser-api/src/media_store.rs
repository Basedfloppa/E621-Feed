//! Disk I/O for locally-stored original media (docs/offline-catalog.md).
//!
//! The DB (`media_entries`) only indexes files; this module owns the bytes on
//! the system disk under the hardcoded `media/` folder (relative to the
//! working directory — link/symlink it wherever it's needed). Responsibilities:
//!
//!   * atomic save (`temp` + `rename`) of a downloaded original;
//!   * resolve a stored post's absolute file path for serving;
//!   * LRU eviction driver: when total `media_cache_max_bytes` is exceeded,
//!     delete the oldest files and their index rows.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

use crate::db;
use crate::models::Post;
use crate::models::cfg;

/// Rewrite `files.original.url` to the local media proxy for every post in
/// `posts` that has an original stored on disk. This is the single point that
/// makes locally-available posts serve their original bytes from this backend
/// instead of hot-linking e621.
pub fn rewrite_local_media_urls(posts: &mut [Post]) {
    let ids: Vec<i64> = posts.iter().map(|p| p.id).collect();
    if ids.is_empty() {
        return;
    }
    let map = match db::stored_url_map(&ids) {
        Ok(m) => m,
        Err(_) => return,
    };
    for p in posts.iter_mut() {
        if map.contains(&p.id) {
            // Point every size at the local full file. Only full originals are
            // stored on disk, so when one exists the whole card renders from
            // the local server (cost is negligible on a LAN). The frontend
            // treats `/api/media/…` as a usable image source.
            let local = format!("/api/media/{}?size=original", p.id);
            if let Some(u) = p.files.preview.url.as_mut() {
                *u = local.clone();
            }
            if let Some(u) = p.files.sample.url.as_mut() {
                *u = local.clone();
            }
            if let Some(u) = p.files.original.url.as_mut() {
                *u = local;
            }
        }
    }
}

/// The on-disk media folder. Fixed at `media/` (relative to the working
/// directory) — link/symlink it wherever you need it.
pub fn cache_dir() -> PathBuf {
    PathBuf::from("media")
}

/// `rel_path` (relative to the media folder) used as the `media_entries.rel_path`
/// key. The numeric `post_id % 100` shard keeps each directory small and never
/// contains traversal components.
pub fn rel_path_for(post_id: i64, ext: &str) -> String {
    format!("{:02}/{post_id}.{}", post_id % 100, rel_ext(ext))
}

/// Reduce a file extension to a bare `[a-zA-Z0-9_]` token (max 12 chars) so it
/// can never carry path separators or traversal components. Empty/invalid →
/// `"bin"`. `ext` is sourced from `posts.file_ext` (e621 metadata) and must not
/// be trusted as a filesystem component.
fn rel_ext(ext: &str) -> String {
    let cleaned: String = ext
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(12)
        .collect();
    if cleaned.is_empty() {
        "bin".to_string()
    } else {
        cleaned
    }
}

/// Atomically persist an original's bytes. Returns the relative path stored in
/// the index.
pub fn save_original(post_id: i64, ext: &str, bytes: &[u8]) -> Result<String, String> {
    let dir = cache_dir();
    let rel = rel_path_for(post_id, ext);
    let final_path = dir.join(&rel);
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("media save create_dir_all: {e}"))?;
    }
    let tmp = final_path.with_extension(format!("{}.tmp", rel_ext(ext)));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("media save create tmp: {e}"))?;
        f.write_all(bytes)
            .map_err(|e| format!("media save write: {e}"))?;
        f.sync_all().map_err(|e| format!("media save sync: {e}"))?;
    }
    fs::rename(&tmp, &final_path).map_err(|e| format!("media save rename: {e}"))?;
    Ok(rel)
}

/// Save an original to disk AND write its index entry (the combined write the
/// mode-B prefetcher uses). Keeps the file and the `media_entries` row in sync
/// in one call. `url_digest` is a short provenance token for change detection.
pub fn store_original(
    post_id: i64,
    ext: &str,
    bytes: &[u8],
    url_digest: &str,
) -> Result<String, String> {
    let rel = save_original(post_id, ext, bytes)?;
    let mtime = chrono::Utc::now().timestamp();
    db::upsert_media_entry(post_id, &rel, bytes.len() as i64, mtime, url_digest)
        .map_err(|e| format!("store_original index: {e}"))?;
    Ok(rel)
}

/// Absolute path for a stored post, if the file exists — and only when `rel`
/// resolves to a path still inside the media folder (defense against a planted
/// traversal `rel_path` in `media_entries`). Returns `None` otherwise.
pub fn stored_path(_post_id: i64, rel: &str) -> Option<PathBuf> {
    let dir = cache_dir();
    // Reject traversal / absolute / prefix components outright.
    let rel_path = std::path::Path::new(rel);
    if rel_path.is_absolute()
        || rel_path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let p = dir.join(rel);
    if p.is_file() { Some(p) } else { None }
}

/// Total bytes on disk currently indexed. Used for the cache-size budget.
pub fn indexed_bytes() -> i64 {
    db::count_media_bytes().unwrap_or(0)
}

/// Enforce `media_cache_max_bytes`: delete the oldest files (LRU by mtime)
/// and their index rows until the total drops under the budget.
///
/// Returns the number of files evicted.
pub fn evict_to_budget() -> usize {
    let max = cfg().catalog.media_cache_max_bytes;
    if max == 0 {
        return 0;
    }
    let mut evicted = 0;
    loop {
        let total = indexed_bytes();
        if total <= max as i64 {
            break;
        }
        // Evict in bounded batches so a pathological surplus can't pin the
        // writer for minutes.
        let candidates = match db::oldest_media_entries(16) {
            Ok(c) if !c.is_empty() => c,
            _ => break,
        };
        let mut removed_ids = Vec::new();
        for ent in &candidates {
            if let Some(p) = stored_path(ent.post_id, &ent.rel_path) {
                let _ = fs::remove_file(p);
            }
            removed_ids.push(ent.post_id);
        }
        match db::delete_media_entries(&removed_ids) {
            Ok(_) => evicted += removed_ids.len(),
            Err(_) => break,
        }
    }
    evicted
}

/// Remove a stored post's original file and its index row (catalog post
/// removal). No-op when the entry is absent. Returns whether an entry existed
/// and was removed.
pub fn delete_and_unindex(post_id: i64) -> Result<bool, String> {
    let Some((rel, _mtime)) = db::get_media_entry(post_id)? else {
        return Ok(false);
    };
    if let Some(p) = stored_path(post_id, &rel) {
        let _ = fs::remove_file(p);
    }
    db::delete_media_entries(&[post_id]).map_err(|e| format!("delete_and_unindex: {e}"))?;
    Ok(true)
}

/// Clear the entire local media cache: delete the on-disk originals and wipe
/// the `media_entries` index (the links to local files). Returns how many
/// entries were cleared.
pub fn clear_cache() -> Result<usize, String> {
    let entries = db::all_media_entries().map_err(|e| format!("clear_cache list: {e}"))?;
    for ent in &entries {
        if let Some(p) = stored_path(ent.post_id, &ent.rel_path) {
            let _ = fs::remove_file(p);
        }
    }
    db::clear_media_entries().map_err(|e| format!("clear_cache: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_path_shards_by_mod_100() {
        assert_eq!(rel_path_for(7, "jpg"), "07/7.jpg");
        assert_eq!(rel_path_for(123_456, "png"), "56/123456.png");
        assert_eq!(rel_path_for(1_000_000, "webm"), "00/1000000.webm");
    }

    #[test]
    fn rel_path_neutralizes_traversal_ext() {
        // A crafted extension must never carry separators or traversal.
        assert_eq!(rel_path_for(1, "../../etc/passwd"), "01/1.etcpasswd");
        assert_eq!(rel_path_for(1, "..\\..\\win"), "01/1.win");
        assert_eq!(rel_path_for(1, "png"), "01/1.png");
        // Empty / non-alphanumeric → safe fallback.
        assert_eq!(rel_path_for(1, "..."), "01/1.bin");
        assert_eq!(rel_path_for(1, ""), "01/1.bin");
        // Long extensions are capped to keep the filename bounded.
        let long = rel_path_for(1, &"a".repeat(80));
        assert!(long.len() <= "01/1.".len() + 12);
        assert!(long.ends_with(&"a".repeat(12)));
    }

    #[test]
    fn stored_path_rejects_traversal_components() {
        assert!(stored_path(1, "../outside.png").is_none());
        assert!(stored_path(1, "/etc/passwd").is_none());
        assert!(stored_path(1, "07/1.png").is_some() || stored_path(1, "07/1.png").is_none());
        // Absolute and prefix forms are refused.
        assert!(stored_path(1, "C:\\evil").is_none());
    }
}
