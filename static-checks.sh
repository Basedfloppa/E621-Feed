#!/usr/bin/env bash
# Run Rust static-analysis checks across one or more Cargo projects and
# aggregate per-tool output into a single machine-readable JSON report.
#
# Each tool is run per-project so the report captures results for both
# `parser-api` (server) and `parser-web` (WASM frontend).
#
# Usage:
#   scripts/static-checks.sh [project-dir...]
#
# If no project directories are given the script looks for Cargo projects in
# the repository root — by default it checks `parser-api` and `parser-web`.
# Pass explicit paths to run against a different set.
#
# Examples:
#   ./scripts/static-checks.sh
#   ./scripts/static-checks.sh parser-api
#   ./scripts/static-checks.sh /abs/path/to/project-a /abs/path/to/project-b
#
# Output layout:
#   <workspace>/target/static-checks/<UTC-timestamp>/
#     report.json            — aggregate report (machine-readable)
#     <project>/<tool>.raw   — stdout of each tool per project
#     <project>/<tool>.err   — stderr of each tool per project
#     <project>/<tool>.summary.json — per-project per-tool status
#
# Exit code is always 0 unless prerequisites are missing; individual tools may
# fail without aborting the run (each tool's exit code is recorded in JSON).

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")" && pwd)"

# --- Default project list (relative to repo root) --------------------------
DEFAULT_PROJECTS=()
for guess in parser-api parser-web; do
  [[ -f "$REPO_ROOT/$guess/Cargo.toml" ]] && DEFAULT_PROJECTS+=("$REPO_ROOT/$guess")
done

if [[ ${#DEFAULT_PROJECTS[@]} -eq 0 ]]; then
  echo "error: no Cargo projects found under $REPO_ROOT" >&2
  exit 2
fi

# --- Parse arguments -------------------------------------------------------
PROJECTS=()
if [[ $# -gt 0 ]]; then
  for p in "$@"; do
    # Resolve relative to CWD or to REPO_ROOT
    if [[ -f "$p/Cargo.toml" ]]; then
      PROJECTS+=("$(cd "$p" && pwd)")
    elif [[ -f "$REPO_ROOT/$p/Cargo.toml" ]]; then
      PROJECTS+=("$(cd "$REPO_ROOT/$p" && pwd)")
    else
      echo "error: $p has no Cargo.toml" >&2
      exit 2
    fi
  done
else
  PROJECTS=("${DEFAULT_PROJECTS[@]}")
fi

# --- Prerequisites ---------------------------------------------------------
command -v jq    >/dev/null 2>&1 || { echo "error: jq is required"    >&2; exit 2; }
command -v cargo >/dev/null 2>&1 || { echo "error: cargo is required" >&2; exit 2; }

TS="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="$REPO_ROOT/target/static-checks/$TS"
mkdir -p "$OUT"

log() { printf '[static-checks] %s\n' "$*" >&2; }

# --- Per-project output directory helper -----------------------------------
project_out() {
  local proj="$1" tool="$2"
  local dir="$OUT/$(basename "$proj")"
  mkdir -p "$dir"
  printf '%s' "$dir/$tool"
}

# --- Tool runner -----------------------------------------------------------
# run_check TOOL_NAME PROJ PROBE_BIN -- cmd args...
#
# TOOL_NAME  — short name used in the report (e.g. clippy, audit).
# PROJ       — absolute path to the project being checked.
# PROBE_BIN  — what we look up via `command -v` to detect installation.
#              For cargo subcommands the probe is `cargo-<sub>`; for first-party
#              cargo features the probe is `cargo` itself.
run_check() {
  local name="$1" proj="$2" probe="$3"; shift 3

  if ! command -v "$probe" >/dev/null 2>&1; then
    jq -n --arg p "$probe" '{status:"not_installed", probe:$p}' \
      > "$(project_out "$proj" "$name").summary.json"
    log "skip   $(basename "$proj")/$name  (missing: $probe)"
    return
  fi

  log "run    $(basename "$proj")/$name"
  local base="$(project_out "$proj" "$name")"
  local t0; t0=$(date +%s)
  local rc=0
  # If the command is prefixed with 'cd@', change to the project directory first.
  # This is needed for tools that do not support --manifest-path.
  if [[ "$1" == "cd@" ]]; then
    shift
    (cd "$proj" && "$@") > "${base}.raw" 2> "${base}.err" || rc=$?
  else
    "$@" > "${base}.raw" 2> "${base}.err" || rc=$?
  fi
  local t1; t1=$(date +%s)

  local status; [[ $rc -eq 0 ]] && status=ok || status="exit_${rc}"

  jq -n \
    --arg status  "$status" \
    --argjson rc  "$rc" \
    --argjson dur "$((t1-t0))" \
    --arg raw     "${base}.raw" \
    --arg err     "${base}.err" \
    --arg proj    "$(basename "$proj")" \
    --arg tool    "$name" \
    '{project:$proj, tool:$tool, status:$status, exit_code:$rc, duration_seconds:$dur, raw:$raw, err:$err}' \
    > "${base}.summary.json"
}

# Disable colour for every child process so .raw captures stay plain text.
export NO_COLOR=1
export CARGO_TERM_COLOR=never

# ---------------------------------------------------------------------------
#  Per-project check dispatcher
# ---------------------------------------------------------------------------
for PROJ in "${PROJECTS[@]}"; do
  log "═══ project: $(basename "$PROJ") ═══"

  # --- 1. clippy -----------------------------------------------------------
  run_check clippy "$PROJ" cargo \
    cargo clippy --manifest-path "$PROJ/Cargo.toml" --all-targets --message-format=json \
      -- -W clippy::perf -W clippy::nursery

  # --- 2. cargo-audit (run once at repo root) ------------------------------
  # audit is not project-scoped; it reads Cargo.lock. Run only for the first
  # project to avoid redundant identical scans.
  if [[ "$PROJ" == "${PROJECTS[0]}" ]]; then
    run_check audit "$PROJ" cargo-audit \
      cargo audit --file "$PROJ/Cargo.lock" --json
  fi

  # --- 3. cargo-deny (also repo-scoped, run once) --------------------------
  if [[ "$PROJ" == "${PROJECTS[0]}" ]]; then
    if [[ -f "$REPO_ROOT/deny.toml" ]]; then
      run_check deny "$PROJ" cargo-deny \
        cargo deny --manifest-path "$PROJ/Cargo.toml" check
    else
      jq -n '{status:"skipped", reason:"no deny.toml config"}' \
        > "$(project_out "$PROJ" deny).summary.json"
      log "skip   $(basename "$PROJ")/deny (no deny.toml config)"
    fi
  fi

  # --- 4. cargo-outdated ---------------------------------------------------
  # Run per-project since each has its own dependencies.
  run_check outdated "$PROJ" cargo-outdated \
    cargo outdated --manifest-path "$PROJ/Cargo.toml" --format json

  # --- 5. cargo-bloat (only native targets; skip WASM frontend) ------------
  if [[ "$(basename "$PROJ")" != "parser-web" ]]; then
    run_check bloat "$PROJ" cargo-bloat \
      cd@ cargo bloat --release --crates -n 30 --message-format json
  else
    jq -n --arg proj "$(basename "$PROJ")" '{project:$proj, status:"skipped", reason:"WASM target"}' \
      > "$(project_out "$PROJ" bloat).summary.json"
    log "skip   $(basename "$PROJ")/bloat (WASM target — no native release binary)"
  fi

  # --- 6. cargo-machete ----------------------------------------------------
  run_check machete "$PROJ" cargo-machete \
    cargo machete "$PROJ"

  # --- 7. cargo-geiger (only native targets; skip WASM) --------------------
  if [[ "$(basename "$PROJ")" != "parser-web" ]]; then
    run_check geiger "$PROJ" cargo-geiger \
      cargo geiger --manifest-path "$PROJ/Cargo.toml" --output-format Json --quiet
  else
    jq -n --arg proj "$(basename "$PROJ")" '{project:$proj, status:"skipped", reason:"WASM target"}' \
      > "$(project_out "$PROJ" geiger).summary.json"
    log "skip   $(basename "$PROJ")/geiger (WASM target)"
  fi
done

# ---------------------------------------------------------------------------
#  Post-processing: extract findings from raw outputs
# ---------------------------------------------------------------------------

# === clippy lint-frequency summary (per project) ===========================
for PROJ in "${PROJECTS[@]}"; do
  base="$(project_out "$PROJ" clippy)"
  if [[ -s "${base}.raw" ]]; then
    jq -s '
      map(
        select(.reason == "compiler-message") |
        .message |
        select(.code != null and (.spans | length) > 0) |
        {lint: .code.code, level: .level}
      ) |
      group_by(.lint) |
      map({lint: .[0].lint, level: .[0].level, count: length}) |
      sort_by(-.count)
    ' "${base}.raw" > "${base}.lints.json" 2>/dev/null \
      || echo '[]' > "${base}.lints.json"

    jq --slurpfile lints "${base}.lints.json" '
      . + {
        by_lint:        $lints[0],
        distinct_lints: ($lints[0] | length),
        total_findings: ($lints[0] | map(.count) | add // 0)
      }
    ' "${base}.summary.json" > "${base}.tmp" && mv "${base}.tmp" "${base}.summary.json"
  fi
done

# === outdated findings (per project) ======================================
for PROJ in "${PROJECTS[@]}"; do
  base="$(project_out "$PROJ" outdated)"
  if [[ -s "${base}.raw" ]]; then
    jq -s '
      map(
        {crate: .crate_name, deps: [.dependencies[] | select(
          (.compat != null and .compat != "" and .compat != "Removed" and .compat != "---")
          or (.latest != null and .latest != "" and .latest != "Removed" and .latest != "---")
        )]}
      ) |
      map(. + {update_count: (.deps | length)})
    ' "${base}.raw" > "${base}.findings.json" 2>/dev/null \
      || echo '[]' > "${base}.findings.json"

    jq -s '
      [.[].dependencies[]] |
      {
        compat_updates: [.[] | select(.compat != null and .compat != "" and .compat != "Removed" and .compat != "---")],
        major_updates:  [.[] | select(
          (.compat == null or .compat == "" or .compat == "---" or .compat == "Removed")
          and (.latest != null and .latest != "" and .latest != "Removed" and .latest != "---")
        )]
      } |
      {compat_count: (.compat_updates | length), major_count: (.major_updates | length)}
    ' "${base}.raw" > "${base}.classification.json" 2>/dev/null \
      || echo '{}' > "${base}.classification.json"

    jq --slurpfile f "${base}.findings.json" \
       --slurpfile c "${base}.classification.json" '
      . + {
        by_crate:            $f[0],
        total_outdated:      ($f[0] | map(.update_count) | add // 0),
        compat_updates:      $c[0].compat_count,
        major_updates:       $c[0].major_count,
        affected_crates:     [$f[0][] | select(.update_count > 0) | .crate]
      }
    ' "${base}.summary.json" > "${base}.tmp" && mv "${base}.tmp" "${base}.summary.json"
  fi
done

# === bloat crate-size breakdown (per project) ==============================
for PROJ in "${PROJECTS[@]}"; do
  base="$(project_out "$PROJ" bloat)"
  if [[ -s "${base}.raw" ]]; then
    jq '{
      file_size_bytes:      .["file-size"],
      text_section_bytes:   .["text-section-size"],
      total_crates:         (.crates | length),
      top_crates:           [.crates[] | {name, size}] | sort_by(-.size)
    }' "${base}.raw" > "${base}.findings.json" 2>/dev/null \
      || echo '{}' > "${base}.findings.json"

    jq --slurpfile f "${base}.findings.json" '. + $f[0]' \
      "${base}.summary.json" > "${base}.tmp" && mv "${base}.tmp" "${base}.summary.json"
  fi
done

# === machete unused-deps extraction (per project) ==========================
for PROJ in "${PROJECTS[@]}"; do
  base="$(project_out "$PROJ" machete)"
  if [[ -s "${base}.raw" ]]; then
    sed -E 's/\x1B\[[0-9;]*[mGKHJ]//g' "${base}.raw" \
      | awk '
          /^cargo-machete (found|finished|did)/ { next }
          /^If you believe cargo-machete/        { exit }
          /^$/                                    { next }
          /^[A-Za-z0-9_.-]+ -- .*Cargo\.toml:?$/ {
            crate = $1
            next
          }
          crate != "" && /^[[:space:]]+[A-Za-z0-9_-]+[[:space:]]*$/ {
            dep = $1
            printf "%s\t%s\n", crate, dep
          }
        ' \
      | jq -Rn '
          [inputs
            | select(length > 0)
            | split("\t")
            | {crate: .[0], dep: .[1]}]
          | group_by(.crate)
          | map({crate: .[0].crate, unused: map(.dep)})
        ' > "${base}.findings.json" 2>/dev/null \
      || echo '[]' > "${base}.findings.json"

    jq --slurpfile f "${base}.findings.json" '
      . + {
        unused_by_crate: $f[0],
        total_unused:    ($f[0] | map(.unused | length) | add // 0),
        affected_crates: ($f[0] | length)
      }
    ' "${base}.summary.json" > "${base}.tmp" && mv "${base}.tmp" "${base}.summary.json"
  fi
done

# === audit findings (single run) ==========================================
# Attach audit results to the first project's output directory.
AUDIT_PROJ="${PROJECTS[0]}"
AUDIT_BASE="$(project_out "$AUDIT_PROJ" audit)"
if [[ -s "${AUDIT_BASE}.raw" ]]; then
  jq '{
    vulnerabilities: (.vulnerabilities.count // 0),
    warnings:        ((.warnings // {} | to_entries | map(.value | length) | add) // 0)
  }' "${AUDIT_BASE}.raw" > "${AUDIT_BASE}.findings.json" 2>/dev/null \
    || echo '{}' > "${AUDIT_BASE}.findings.json"

  jq --slurpfile f "${AUDIT_BASE}.findings.json" '. + $f[0]' \
    "${AUDIT_BASE}.summary.json" > "${AUDIT_BASE}.tmp" && mv "${AUDIT_BASE}.tmp" "${AUDIT_BASE}.summary.json"
fi

# === geiger unsafe metrics (per project) ===================================
for PROJ in "${PROJECTS[@]}"; do
  base="$(project_out "$PROJ" geiger)"
  if [[ -s "${base}.raw" ]]; then
    jq '{
      unsafe_count: (.unsafe_count // 0),
      safe_count:   (.safe_count // 0),
      total_crates: ([.crates[]?.entry?] | length // 0)
    }' "${base}.raw" > "${base}.findings.json" 2>/dev/null \
      || echo '{}' > "${base}.findings.json"

    jq --slurpfile f "${base}.findings.json" '. + $f[0]' \
      "${base}.summary.json" > "${base}.tmp" && mv "${base}.tmp" "${base}.summary.json"
  fi
done

# ---------------------------------------------------------------------------
#  Assemble master report
# ---------------------------------------------------------------------------
commit="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
branch="$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
rustc_v="$(rustc --version 2>/dev/null || echo unknown)"

# Build a JSON object keyed by project, each containing its check summaries.
build_project_checks() {
  local proj="$1" name; name="$(basename "$proj")"
  local dir="$OUT/$name"
  local checks_json
  checks_json=$(jq -n '{}')

  for tool in clippy audit deny outdated bloat machete geiger; do
    local sf="$dir/$tool.summary.json"
    if [[ -f "$sf" ]]; then
      checks_json=$(jq --arg t "$tool" --slurpfile s "$sf" '.[$t] = $s[0]' <<<"$checks_json")
    else
      checks_json=$(jq --arg t "$tool" '.[$t] = {status:"no_data"}' <<<"$checks_json")
    fi
  done

  printf '%s' "$checks_json"
}

PROJECTS_JSON="{}"
for PROJ in "${PROJECTS[@]}"; do
  name="$(basename "$PROJ")"
  pc=$(build_project_checks "$PROJ")
  PROJECTS_JSON=$(jq --arg n "$name" --argjson c "$pc" '.[$n] = $c' <<<"$PROJECTS_JSON")
done

jq -n \
  --arg ts     "$TS" \
  --arg repo   "$REPO_ROOT" \
  --arg commit "$commit" \
  --arg branch "$branch" \
  --arg rustc  "$rustc_v" \
  --argjson projects "$PROJECTS_JSON" \
  '{
    timestamp: $ts,
    repo:      $repo,
    commit:    $commit,
    branch:    $branch,
    rustc:     $rustc,
    projects:  $projects
  }' > "$OUT/report.json"

# ---------------------------------------------------------------------------
#  Brief stderr summary
# ---------------------------------------------------------------------------
log "summary:"
jq -r '
  .projects | to_entries[] |
  "  project: \(.key)",
  (.value | to_entries[] |
    "    \(.key | . + (" " * (10 - length))): status=\(.value.status)"
    + (if .value.total_findings    != null then "  findings=\(.value.total_findings)"    else "" end)
    + (if .value.vulnerabilities   != null then "  vulns=\(.value.vulnerabilities)"      else "" end)
    + (if .value.warnings          != null then "  warns=\(.value.warnings)"             else "" end)
    + (if .value.total_unused      != null then "  unused=\(.value.total_unused)"        else "" end)
    + (if .value.total_outdated    != null then "  outdated=\(.value.total_outdated)"    else "" end)
    + (if .value.compat_updates    != null then "  compat=\(.value.compat_updates)"      else "" end)
    + (if .value.major_updates     != null then "  major=\(.value.major_updates)"        else "" end)
    + (if .value.total_crates      != null then "  bloat_crates=\(.value.total_crates)"  else "" end)
    + (if .value.unsafe_count      != null then "  unsafe=\(.value.unsafe_count)"        else "" end)
    + (if .value.reason            != null then "  reason=\(.value.reason)"              else "" end)
    + "  (\(.value.duration_seconds // 0)s)"
  )
' "$OUT/report.json" >&2

log "report: $OUT/report.json"
echo "$OUT/report.json"
