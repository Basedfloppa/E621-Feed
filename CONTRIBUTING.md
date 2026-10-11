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

CI is the authoritative gate: `.github/workflows/ci.yml` runs `fmt`, Clippy, both
test suites and the WASM build on every push and pull request, and
`.github/workflows/audit.yml` runs `cargo audit` and `cargo deny check` on a
schedule and whenever a manifest or lock file changes. The versioned local hook
mirrors those checks so a failure surfaces before the push, but it is opt-in —
enable it after cloning:

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

## Browser and HTTP smoke checks

`scripts/e2e-smoke.mjs` and `scripts/browser-check.mjs` cover what unit tests
cannot see: the HTTP contract of the embedded SPA, and whether a real browser can
boot the built bundle. Both are plain Node (>= 22, for the global `WebSocket`)
with no dependencies to install, print every check they ran, and exit non-zero if
any of them failed.

```bash
# embedded SPA, static assets (including their SRI hashes) and the owner cookie
node scripts/e2e-smoke.mjs --base-url http://127.0.0.1:8181

# does the bundle actually boot: CSP, WASM MIME, Yew mount, console errors
node scripts/browser-check.mjs --url http://127.0.0.1:8181
```

`browser-check.mjs` looks for Chrome in `$CHROME_PATH`, the Playwright cache and
`$PATH`, and skips with exit code 0 when it finds none, so it can be called on a
runner without a browser. Two things are worth knowing before reading its output:

- Run it against a **release** API binary. A debug build mounts a `rocket_cors`
  fairing that diverts any non-allowlisted `Origin` to `/cors/403`, which the SPA
  fallback then serves as `200 OK index.html`; because Trunk marks the bundle
  references `crossorigin`, the browser sends an `Origin` even for a same-origin
  load, so the app cannot boot straight from a debug `:8181`.
- `/api/health` answers 503 whenever the upstream e621 probe fails or its rate
  limit trips. That is the probe endpoint reporting, not the server being down.

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
- `cargo-deny` reads the `deny.toml` checked in at the repository root and is
  invoked once per crate with an explicit `--config`. Without the tool installed
  the step reports `status: "skipped"` instead of failing, so a green report does
  not by itself mean the license and advisory policy was checked.
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
