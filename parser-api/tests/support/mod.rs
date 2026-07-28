//! Shared integration-test setup.
//!
//! Each integration-test binary gets its own SQLite catalog file. This keeps
//! assertions independent of the developer's `database.db` and of unrelated
//! test binaries, while preserving the production DB layer and migrations.

use std::io::Write;
use std::sync::OnceLock;

use tempfile::NamedTempFile;

static TEST_CONFIG: OnceLock<NamedTempFile> = OnceLock::new();

/// Install a config pointing at a process-unique temporary SQLite database.
///
/// Must run before the first DB access in an integration-test binary: the DB
/// pool and dedicated writer connection intentionally cache their path.
pub fn install_isolated_db_config() {
    TEST_CONFIG.get_or_init(|| {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let example = std::fs::read_to_string(manifest_dir.join("config.example.toml"))
            .expect("read config.example.toml");
        let db_path = std::env::temp_dir().join(format!(
            "e621-account-parser-test-{}-{}.db",
            std::process::id(),
            module_path!().replace("::", "-")
        ));
        let db_path = db_path.to_string_lossy().replace('\\', "\\\\");
        let config = example.replacen(
            "db_path = \"database.db\"",
            &format!("db_path = \"{db_path}\""),
            1,
        );
        assert_ne!(config, example, "config.example.toml is missing db_path");

        let mut file = NamedTempFile::new().expect("create temporary test config");
        file.write_all(config.as_bytes())
            .expect("write temporary test config");
        file.flush().expect("flush temporary test config");
        e621_account_parser_api::models::reload_from(file.path())
            .expect("load isolated integration-test config");
        file
    });
}
