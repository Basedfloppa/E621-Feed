//! Offline bulk seed of the **global taxonomy** from the official e621
//! `db_exports` dumps.
//!
//! e621 publishes weekly full-dump CSVs at `https://e621.net/db_exports`.
//! Building the *posts* from a dump is not worth it: the dumps carry no media
//! URLs (only md5), so every thumbnail/sample would still have to be fetched
//! per-post by the media-hydrator — no faster than the normal incremental
//! prefetch. Instead this binary fills the parts that arrive complete:
//!
//! - `tags.csv.gz`            → the `tags` table (name + category→group);
//! - `tag_aliases.csv.gz`     → the `tag_aliases` table;
//! - `tag_implications.csv.gz`→ the `tag_implications` table.
//!
//! Posts and media then continue to accumulate through the usual small
//! incremental prefetch / `/process` / media-hydrator requests — the
//! deliberate "trickle" of fresh data, rather than one giant import.
//!
//! Data the dumps do NOT contain: users, favourites, votes, interactions —
//! the account layer stays API-only.
//!
//! Usage:
//!   cargo run --release --bin catalog-seed
//!     # download missing dumps into ./db_exports, then ingest all tables
//!   cargo run --release --bin catalog-seed -- --dir /srv/e621/db_exports
//!   cargo run --release --bin catalog-seed -- --skip-download
//!
//! All writes are idempotent upserts, so re-runs are safe.

use std::env;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use csv::StringRecord;
use flate2::read::GzDecoder;
use futures_util::StreamExt as _;
use tokio::io::AsyncWriteExt as _;

use e621_account_parser_api::{db, models};
use models::{TagAlias, TagImplication};

const EXPORTS_BASE: &str = "https://e621.net/db_exports";
const DEFAULT_DIR: &str = "db_exports";

const TAGS_CSV: &str = "tags.csv.gz";
const ALIASES_CSV: &str = "tag_aliases.csv.gz";
const IMPLICATIONS_CSV: &str = "tag_implications.csv.gz";

const RELATION_BATCH: usize = 5000;
const TAG_BATCH: usize = 50_000;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

struct Opts {
    dir: PathBuf,
    skip_download: bool,
}

impl Opts {
    fn parse() -> Self {
        let mut dir = PathBuf::from(DEFAULT_DIR);
        let mut skip_download = false;
        let mut args = env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--dir" => dir = PathBuf::from(args.next().expect("--dir needs a value")),
                "--skip-download" => skip_download = true,
                other => eprintln!("[catalog-seed] ignoring unknown arg: {other}"),
            }
        }
        Self { dir, skip_download }
    }
}

// ---------------------------------------------------------------------------
// Download (streamed to disk, never held in memory)
// ---------------------------------------------------------------------------

/// Shared HTTP client with the configured e621 User-Agent. e621 refuses
/// requests that carry a default/empty User-Agent (HTTP 403) — the same
/// `cfg().user_agent` the main API client uses.
fn seed_client() -> anyhow::Result<reqwest::Client> {
    let ua = models::cfg().user_agent.clone();
    reqwest::Client::builder()
        .user_agent(ua)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("failed to build e621 download client — check TLS/network config")
}

async fn download(client: &reqwest::Client, url: &str, dest: &Path) -> anyhow::Result<u64> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    anyhow::ensure!(
        resp.status().is_success(),
        "download {url}: HTTP {}",
        resp.status()
    );
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = resp.bytes_stream();
    let mut size: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read download body")?;
        size += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(size)
}

async fn ensure_download(
    client: &reqwest::Client,
    opts: &Opts,
    name: &str,
) -> anyhow::Result<PathBuf> {
    let path = opts.dir.join(name);
    if opts.skip_download {
        anyhow::ensure!(
            path.exists(),
            "{} missing and --skip-download was set",
            path.display()
        );
        return Ok(path);
    }
    if path.exists() {
        // A pre-existing file is treated as complete (if a download was
        // interrupted, delete the file and re-run).
        return Ok(path);
    }
    std::fs::create_dir_all(&opts.dir)?;
    let url = format!("{EXPORTS_BASE}/{name}");
    eprintln!("[catalog-seed] downloading {url} -> {}", path.display());
    let got = download(client, &url, &path).await?;
    eprintln!("[catalog-seed] download complete ({} bytes)", got);
    Ok(path)
}

// ---------------------------------------------------------------------------
// CSV helpers
// ---------------------------------------------------------------------------

/// Offset of `name` in `headers`, else None.
fn col_ix(headers: &StringRecord, name: &str) -> Option<usize> {
    headers.iter().position(|h| h == name)
}

fn opt(rec: &StringRecord, ix: Option<usize>) -> &str {
    ix.and_then(|i| rec.get(i)).unwrap_or("")
}

/// Optional non-empty string column (for timestamps / names).
fn dt_opt(rec: &StringRecord, ix: Option<usize>) -> Option<String> {
    let s = opt(rec, ix).trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Map an e621 tag `category` integer to the `group_type` string allowed by
/// the `tags` CHECK constraint. Unknown categories (2, 6, …) fall back to
/// `general` so the row is never rejected.
fn category_to_group(cat: &str) -> &'static str {
    match cat.trim() {
        "1" => "artist",
        "3" => "copyright",
        "4" => "character",
        "5" => "species",
        "7" => "meta",
        "8" => "lore",
        _ => "general", // 0 (general) and any unknown/legacy category
    }
}

// ---------------------------------------------------------------------------
// Tags ingest
// ---------------------------------------------------------------------------

fn ingest_tags(path: &Path) -> anyhow::Result<u64> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let gz = GzDecoder::new(BufReader::new(file));
    let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_reader(gz);
    let headers = rdr.headers()?.clone();
    let name_ix = col_ix(&headers, "name");
    let cat_ix = col_ix(&headers, "category");

    let mut buf: Vec<(String, String)> = Vec::with_capacity(TAG_BATCH);
    let mut inserted: u64 = 0;

    for result in rdr.records() {
        let rec = result?;
        let name = opt(&rec, name_ix).to_string();
        let category = opt(&rec, cat_ix);
        if name.is_empty() {
            continue;
        }
        buf.push((name, category_to_group(category).to_string()));
        if buf.len() >= TAG_BATCH {
            db::upsert_catalog_tags(&buf).map_err(|e| anyhow::anyhow!("upsert tags: {e}"))?;
            inserted += buf.len() as u64;
            buf.clear();
        }
    }
    if !buf.is_empty() {
        db::upsert_catalog_tags(&buf).map_err(|e| anyhow::anyhow!("upsert tags (final): {e}"))?;
        inserted += buf.len() as u64;
    }
    Ok(inserted)
}

// ---------------------------------------------------------------------------
// Alias / implication ingest
// ---------------------------------------------------------------------------

fn ingest_tag_relations(path: &Path, table_alias: bool) -> anyhow::Result<u64> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let gz = GzDecoder::new(BufReader::new(file));
    let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_reader(gz);
    let headers = rdr.headers()?.clone();
    let ix = |n: &str| col_ix(&headers, n);

    let mut buf: Vec<TagAlias> = Vec::new();
    let mut imp_buf: Vec<TagImplication> = Vec::new();
    let mut inserted: u64 = 0;

    for result in rdr.records() {
        let rec = result?;
        if table_alias {
            let e = TagAlias {
                id: opt(&rec, ix("id")).trim().parse().unwrap_or(0),
                antecedent_name: opt(&rec, ix("antecedent_name")).to_string(),
                consequent_name: opt(&rec, ix("consequent_name")).to_string(),
                status: opt(&rec, ix("status")).to_string(),
                created_at: dt_opt(&rec, ix("created_at")),
                updated_at: dt_opt(&rec, ix("updated_at")),
            };
            buf.push(e);
            if buf.len() >= RELATION_BATCH {
                db::save_tag_aliases(&buf).map_err(|e| anyhow::anyhow!("save tag aliases: {e}"))?;
                inserted += buf.len() as u64;
                buf.clear();
            }
        } else {
            let e = TagImplication {
                id: opt(&rec, ix("id")).trim().parse().unwrap_or(0),
                antecedent_name: opt(&rec, ix("antecedent_name")).to_string(),
                consequent_name: opt(&rec, ix("consequent_name")).to_string(),
                status: opt(&rec, ix("status")).to_string(),
                created_at: dt_opt(&rec, ix("created_at")),
                updated_at: dt_opt(&rec, ix("updated_at")),
            };
            imp_buf.push(e);
            if imp_buf.len() >= RELATION_BATCH {
                db::save_tag_implications(&imp_buf)
                    .map_err(|e| anyhow::anyhow!("save tag implications: {e}"))?;
                inserted += imp_buf.len() as u64;
                imp_buf.clear();
            }
        }
    }
    if !buf.is_empty() {
        db::save_tag_aliases(&buf).map_err(|e| anyhow::anyhow!("save tag aliases (final): {e}"))?;
        inserted += buf.len() as u64;
    }
    if !imp_buf.is_empty() {
        db::save_tag_implications(&imp_buf)
            .map_err(|e| anyhow::anyhow!("save tag implications (final): {e}"))?;
        inserted += imp_buf.len() as u64;
    }
    Ok(inserted)
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();

    let path = models::default_path()?;
    models::reload_from(&path)?;
    db::ensure_sqlite().map_err(|e| anyhow::anyhow!("migrate: {e}"))?;

    let client = seed_client()?;

    let tags_path = ensure_download(&client, &opts, TAGS_CSV).await?;
    let aliases_path = ensure_download(&client, &opts, ALIASES_CSV).await?;
    let implications_path = ensure_download(&client, &opts, IMPLICATIONS_CSV).await?;

    eprintln!("[catalog-seed] ingesting tags from {}", tags_path.display());
    let n = ingest_tags(&tags_path)?;
    eprintln!("[catalog-seed] tags: {n}");

    eprintln!(
        "[catalog-seed] ingesting tag aliases from {}",
        aliases_path.display()
    );
    let n = ingest_tag_relations(&aliases_path, true)?;
    eprintln!("[catalog-seed] tag aliases: {n}");

    eprintln!(
        "[catalog-seed] ingesting tag implications from {}",
        implications_path.display()
    );
    let n = ingest_tag_relations(&implications_path, false)?;
    eprintln!("[catalog-seed] tag implications: {n}");

    eprintln!("[catalog-seed] done.");
    Ok(())
}
