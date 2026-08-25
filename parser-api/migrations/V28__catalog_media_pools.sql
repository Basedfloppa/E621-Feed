-- Local catalog & offline serving (docs/offline-catalog.md).
--
-- Two additions:
--   1. `pools` / `pool_posts` — persist pool membership so the post-viewer
--      pool navigation works from local data without hitting e621, and pool
--      membership is available for future scoring.
--   2. `media_entries` — an INDEX of original media files stored on the system
--      disk under the hardcoded `media/` folder. The DB deliberately does NOT
--      hold media bytes; it only maps a post → its locally-stored original
--      file (relative path, size, last-modified LRU key). Serving reads the
--      file off disk.
--
-- All tables are additive and empty until the corresponding opt-in catalog
-- feature is enabled, so existing deployments are unaffected.

-- Pool membership (local pool navigation + future scoring).
CREATE TABLE pools (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT ''
) STRICT;

CREATE TABLE pool_posts (
    pool_id  INTEGER NOT NULL REFERENCES pools(id) ON DELETE CASCADE,
    post_id  INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    PRIMARY KEY (pool_id, post_id)
) STRICT;
CREATE INDEX idx_pool_posts_post ON pool_posts(post_id);

-- Index of original-media files stored on disk (NOT the blob store).
CREATE TABLE media_entries (
    post_id  INTEGER PRIMARY KEY REFERENCES posts(id) ON DELETE CASCADE,
    rel_path TEXT    NOT NULL,   -- relative to the media folder (media/)
    bytes    INTEGER NOT NULL,
    mtime    INTEGER NOT NULL,   -- epoch seconds; LRU-eviction key
    url_md5  TEXT    NOT NULL    -- md5(file_url) provenance / change detection
) STRICT;
CREATE INDEX idx_media_mtime ON media_entries(mtime);
