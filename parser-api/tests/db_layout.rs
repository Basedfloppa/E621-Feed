//! Static guard: where the database lives and what compose mounts are one
//! decision, not two.
//!
//! SQLite creates `-wal` and `-shm` **beside** the database file, so the
//! database's path decides whether those two files survive `docker compose
//! down` / `up --build`, which recreate the container and its writable layer:
//!
//! * file mount (`./database.db:/app/database.db`) + `db_path = "database.db"`
//!   → the journal lives inside the container and is discarded with it. That
//!   rolls back every transaction since the last checkpoint, and a kill in the
//!   middle of a checkpoint malforms the file (2026-10-13).
//! * directory mount (`./data:/app/data`) + `db_path = "data/database.db"`
//!   → the journal sits on the host and survives.
//!
//! Either half alone is worse than the bug it fixes: `data/` mounted while
//! `db_path` still says `database.db` starts the app against a fresh, empty
//! database inside the container. Nothing at runtime complains, which is why
//! this is checked statically.

use std::path::{Path, PathBuf};

fn repo_file(rel: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The compose header explains the *old* file mount by quoting it, so a plain
/// `contains` would match the documentation instead of the configuration. Only
/// lines that compose would actually parse count.
fn active_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter(|line| {
        let trimmed = line.trim();
        !trimmed.is_empty() && !trimmed.starts_with('#')
    })
}

/// `db_path = "data/database.db"` → `data/database.db`, quotes stripped.
fn declared_db_path(toml: &str) -> String {
    for line in toml.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("db_path") else {
            continue;
        };
        let Some((_, value)) = rest.split_once('=') else {
            continue;
        };
        return value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
    }
    panic!("no db_path in the config");
}

#[test]
fn the_database_lives_in_the_mounted_directory() {
    let db_path = declared_db_path(&repo_file("config.example.toml"));
    let parent = Path::new(&db_path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or_default();
    assert_eq!(
        parent, "data",
        "config.example.toml points db_path at {db_path:?}, but docker-compose \
         mounts ./data as the directory that keeps -wal/-shm on the host. A \
         database outside data/ gets its journal in the container's writable \
         layer, which `docker compose down` throws away."
    );
}

#[test]
fn compose_mounts_the_data_directory_instead_of_the_database_file() {
    let compose = repo_file("docker-compose.yml");
    let lines: Vec<&str> = active_lines(&compose).collect();

    assert!(
        !lines.iter().any(|line| line.contains("./database.db:")),
        "docker-compose.yml still mounts the database as a FILE. SQLite keeps \
         -wal/-shm beside it, inside the container's writable layer, and \
         `docker compose down` / `up --build` discards them."
    );

    // Every service must carry the directory mount: a service without it reads
    // and writes its own database in the container layer, and loses it on the
    // next recreate.
    let services = lines
        .iter()
        .filter(|line| line.contains("container_name:"))
        .count();
    let mounts = lines
        .iter()
        .filter(|line| line.contains("./data:/app/data"))
        .count();
    assert!(services > 0, "no services found in docker-compose.yml");
    assert_eq!(
        services, mounts,
        "{services} service(s) but {mounts} `./data:/app/data` mount(s): every \
         service that opens the database needs the same mount, or it silently \
         uses a database that dies with the container."
    );
}

/// `db_path` moves with the mount, so the pair cannot drift in the one
/// direction the tests above allow (both changed, but to different names).
#[test]
fn the_example_config_and_compose_agree_on_the_filename() {
    let db_path = declared_db_path(&repo_file("config.example.toml"));
    let file = Path::new(&db_path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or_default();
    assert_eq!(
        file, "database.db",
        "the docker docs and the backup instructions name data/database.db; \
         got {db_path:?}"
    );
}
