# Catalog hydration

The backend runs a background catalog hydrator independently of account
`/process` jobs. It repairs recommendation candidates that belong to no account
as well as stale posts imported by older versions.

## Schedule and load

- First pass: about 15 seconds after backend startup.
- Later passes: every 15 minutes after the preceding pass completes.
- Batch size: at most 50 posts.
- Upstream requests use `api::get_posts_by_ids`, so they share the global e621
  rate gate with Feed, Search, Digest, `/process`, and prefetch traffic.

## What is repaired

An active catalog post is selected when it has any incomplete upstream data:

- no usable `*.e621.net` original, preview, or sample URL;
- no `tags_posts` rows;
- missing `uploader_id`.

For returned posts, the hydrator upserts post/media metadata and saves all tag
relations/categories. If e621 does not return a requested ID, the catalog post
is removed. Posts already marked `is_deleted = 1` are purged before each scan;
foreign-key cascades remove related tag, account-link, session, and interaction
records.

## Logs

A repair pass logs its work, for example:

```text
[media-hydrator] purged 50 locally deleted posts
[media-hydrator] scanned 27 incomplete posts; e621 repaired 24, purged 3 absent posts
```

No log is emitted for an idle scan with no incomplete catalog posts.
