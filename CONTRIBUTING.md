## Prerequisites

- Stable Rust and Cargo
- Node.js and npm for the WASM frontend
- `cargo-audit` for dependency changes:

  ```bash
  cargo install cargo-audit --locked
  ```

For frontend development, install JavaScript dependencies once:

```bash
cd parser-web
npm install
```

## Enable the pre-commit hook

This repository has no remote quality gate: the only GitHub workflow
(`.github/workflows/docker-publish.yml`) builds and publishes the image, and runs
nothing else — no `fmt`, `clippy`, tests, audit or WASM build. Enable the
versioned local hook after cloning:

```bash
git config core.hooksPath .githooks
```

The hook runs only for the Rust crate affected by staged files:

| Staged change | Checks |
|---|---|
| `parser-api` Rust source/tests | format, Clippy, API tests |
| `parser-web` Rust source/tests | format, Clippy, frontend tests |
| A crate's `Cargo.toml`, `Cargo.lock`, or `.cargo/audit.toml` | the checks above plus `cargo audit` |

For each affected crate it runs:

```bash
cargo fmt --manifest-path <crate>/Cargo.toml --all -- --check
cargo clippy --manifest-path <crate>/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path <crate>/Cargo.toml -- --test-threads=1
```

The API integration suite uses a shared SQLite database, so tests deliberately run with one test thread. Do not remove this flag without first making the suite database-isolated.

Use `git commit --no-verify` only for an emergency revert; follow up by restoring a passing quality gate.

## Run checks manually

Run these before opening a PR or when the hook reports a failure:

```bash
# API
cargo fmt --manifest-path parser-api/Cargo.toml --all -- --check
cargo clippy --manifest-path parser-api/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path parser-api/Cargo.toml -- --test-threads=1
(cd parser-api && cargo audit --deny warnings)

# WASM frontend
cargo fmt --manifest-path parser-web/Cargo.toml --all -- --check
cargo clippy --manifest-path parser-web/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path parser-web/Cargo.toml -- --test-threads=1
(cd parser-web && cargo audit --deny warnings)
```

To apply Rust formatting, omit `-- --check` from the `cargo fmt` command. For machine-applicable lint fixes, use `cargo clippy --fix --allow-dirty --all-targets`, then inspect the diff and rerun the full checks.

## Static checks (optional tooling)

`static-checks.sh` runs clippy (default lints as errors, `perf`/`nursery` as
information), `cargo audit`, `cargo-deny`, `cargo-outdated`, `cargo-bloat`,
`cargo-machete` and `cargo-geiger` per crate, then writes a JSON report to
`target/static-checks/<UTC-timestamp>/report.json`. Five of those tools are not
bundled with Cargo:

```bash
cargo install --locked --root "$PWD/target/tools" \
  cargo-machete cargo-bloat cargo-outdated cargo-deny cargo-geiger
export PATH="$PWD/target/tools/bin:$PATH"
./static-checks.sh
```

Three things worth knowing before fighting the tooling:

- `--locked` is required for `cargo-outdated` and `cargo-geiger`: without it
  Cargo resolves a `cargo-util` that needs rustc 1.97, newer than the toolchain
  used here.
- `cargo-deny` stays a no-op (`status: "skipped"`) until a `deny.toml` exists —
  there is none today, so installing the tool alone does not turn the step on.
- If `~/.cargo` is not writable, point `CARGO_HOME` at a writable directory
  first (`CARGO_HOME="$PWD/target/cargo-home"`), otherwise every `cargo install`
  fails even though the registry is reachable.

## Dependency advisories

`cargo audit` reads its configuration from the current crate directory. Run it from `parser-api/` or `parser-web/`, not with `--manifest-path`.

Fix or update a vulnerable dependency whenever possible. If an advisory is transitive and cannot currently be resolved, add a narrowly scoped entry to that crate's `.cargo/audit.toml` containing:

1. the RustSec advisory ID;
2. the dependency path and why an upgrade is blocked;
3. why the vulnerable code path is not reachable in this application.

Revisit ignored advisories whenever their parent dependency is upgraded.

## Scope and style

- Keep backend changes in `parser-api/` and frontend changes in `parser-web/`.
- Format all Rust changes with `rustfmt`; the hook treats warnings as errors.
- Add or update focused tests when changing behavior, especially scoring, ingestion, database, and route code.
- Do not commit generated frontend CSS (`parser-web/src/tailwind-output.css`) or local databases.
