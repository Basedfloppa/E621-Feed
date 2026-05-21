#!/usr/bin/env bash
# Run CodeQL static-analysis checks across one or more Cargo projects.
#
# Creates a CodeQL database for each project (Rust extractor), runs the
# standard Rust security-and-correctness query suite, and produces a
# combined JSON report.
#
# Usage:
#   scripts/codeql-checks.sh [project-dir...]
#
# If no project directories are given the script looks for Cargo projects in
# the repository root — by default it checks `parser-api` and `parser-web`.
#
# Prerequisites:
#   - codeql CLI (must be on PATH)
#   - rust (cargo + rustc)
#
# Output layout:
#   <workspace>/target/static-checks/<UTC-timestamp>/
#     codeql-report.json       — aggregate report (machine-readable)
#     <project>/
#       codeql-database/       — CodeQL database (kept for re-analysis)
#       codeql-results.sarif   — SARIF output from the analysis
#       codeql-results.json    — Flattened finding list (machine-readable)
#       codeql.summary.json    — Per-project status
#
# Exit code is 0 unless prerequisites are missing. Individual analyses
# may fail without aborting the run.

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
command -v codeql >/dev/null 2>&1 || { echo "error: codeql CLI is required"    >&2; exit 2; }
command -v jq     >/dev/null 2>&1 || { echo "error: jq is required"           >&2; exit 2; }
command -v cargo  >/dev/null 2>&1 || { echo "error: cargo is required"        >&2; exit 2; }

# Verify Rust support in CodeQL
if ! codeql resolve languages 2>/dev/null | grep -q "^rust "; then
  echo "error: CodeQL Rust extractor not found" >&2
  exit 2
fi

# Check Rust query pack availability
RUST_QUERIES="codeql/rust-queries"
if ! codeql resolve packs 2>/dev/null | grep -q "$RUST_QUERIES"; then
  echo "error: $RUST_QUERIES pack not found" >&2
  exit 2
fi

TS="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="$REPO_ROOT/target/static-checks/$TS"
mkdir -p "$OUT"

log() { printf '[codeql-checks] %s\n' "$*" >&2; }

# --- Per-project output directory helper -----------------------------------
project_out() {
  local proj="$1"
  local dir="$OUT/$(basename "$proj")"
  mkdir -p "$dir"
  printf '%s' "$dir"
}

# ---------------------------------------------------------------------------
#  Per-project check dispatcher
# ---------------------------------------------------------------------------
for PROJ in "${PROJECTS[@]}"; do
  log "═══ project: $(basename "$PROJ") ═══"
  base="$(project_out "$PROJ")"
  name="$(basename "$PROJ")"

  # --- 1. Create CodeQL database -------------------------------------------
  DB_DIR="$base/codeql-database"
  rm -rf "$DB_DIR" 2>/dev/null || true

  log "create database $name"
t0=$(date +%s)
db_rc=0
  codeql database create \
    --language=rust \
    --source-root="$PROJ" \
    --overwrite \
    "$DB_DIR" \
    > "${base}/codeql-db-create.log" 2>&1 || db_rc=$?
t1=$(date +%s)

  if [[ $db_rc -ne 0 ]]; then
    log "FAIL  $name/database (exit=$db_rc)"
    jq -n \
      --arg proj  "$name" \
      --arg tool  "codeql-database" \
      --arg status "exit_${db_rc}" \
      --argjson dur "$((t1-t0))" \
      '{project:$proj, tool:$tool, status:$status, duration_seconds:$dur}' \
      > "${base}/codeql.summary.json"
    continue
  fi

  # --- 2. Run queries ------------------------------------------------------
  log "run   queries $name"
  SARIF_OUT="$base/codeql-results.sarif"
t0=$(date +%s)
q_rc=0

  codeql database analyze \
    "$DB_DIR" \
    "${RUST_QUERIES}" \
    --format=sarif-latest \
    --output="$SARIF_OUT" \
    --sarif-add-snippets \
    --threads=0 \
    > "${base}/codeql-analyze.log" 2>&1 || q_rc=$?
t1=$(date +%s)

  if [[ $q_rc -ne 0 ]] || [[ ! -s "$SARIF_OUT" ]]; then
    log "FAIL  $name/queries (exit=$q_rc)"
    jq -n \
      --arg proj  "$name" \
      --arg tool  "codeql-queries" \
      --arg status "exit_${q_rc}" \
      --argjson dur "$((t1-t0))" \
      '{project:$proj, tool:$tool, status:$status, duration_seconds:$dur}' \
      > "${base}/codeql.summary.json"
    continue
  fi

  # --- 3. Summarise results ------------------------------------------------
  log "summarise $name"

  # Flatten SARIF into a JSON array of findings (one per result).
  jq -c '
    .runs[].results[]? |
    {
      rule_id:      .ruleId,
      message:      (.message.text // ""),
      severity:     (.properties."problem.severity" // "warning"),
      path:         (.locations[0].physicalLocation.artifactLocation.uri // ""),
      start_line:   (.locations[0].physicalLocation.region.startLine // null),
      start_column: (.locations[0].physicalLocation.region.startColumn // null),
      end_line:     (.locations[0].physicalLocation.region.endLine // null),
      end_column:   (.locations[0].physicalLocation.region.endColumn // null)
    }
  ' "$SARIF_OUT" 2>/dev/null > "${base}/codeql-results.json"

  # Count by severity
  findings_total=$(jq -s "length" "${base}/codeql-results.json" 2>/dev/null || echo 0)
  errors_count=$(jq -s '[.[] | select(.severity == "error")] | length' "${base}/codeql-results.json" 2>/dev/null || echo 0)
  warnings_count=$(jq -s '[.[] | select(.severity == "warning")] | length' "${base}/codeql-results.json" 2>/dev/null || echo 0)

  jq -n \
    --arg proj   "$name" \
    --arg tool   "codeql" \
    --arg status "ok" \
    --argjson dur "$((t1-t0))" \
    --arg sarif  "$SARIF_OUT" \
    --argjson findings "$findings_total" \
    --argjson errors "$errors_count" \
    --argjson warnings "$warnings_count" \
    '{
      project:$proj, tool:$tool, status:$status, duration_seconds:$dur,
      sarif:$sarif,
      total_findings:$findings, errors:$errors, warnings:$warnings
    }' > "${base}/codeql.summary.json"
done

# ---------------------------------------------------------------------------
#  Assemble master report
# ---------------------------------------------------------------------------
commit="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
branch="$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
codeql_v="$(codeql --version 2>/dev/null | head -1 || echo unknown)"

build_project_entry() {
  local proj="$1"
  local name; name="$(basename "$proj")"
  local base="$OUT/$name"
  local sf="$base/codeql.summary.json"
  if [[ -f "$sf" ]]; then
    jq -s '{codeql: .[0]}' "$sf"
  else
    jq -n '{codeql: {status:"no_data"}}'
  fi
}

PROJECTS_JSON="{}"
for PROJ in "${PROJECTS[@]}"; do
  name="$(basename "$PROJ")"
  pc=$(build_project_entry "$PROJ")
  PROJECTS_JSON=$(jq --arg n "$name" --argjson c "$pc" '.[$n] = $c' <<<"$PROJECTS_JSON")
done

jq -n \
  --arg ts       "$TS" \
  --arg repo     "$REPO_ROOT" \
  --arg commit   "$commit" \
  --arg branch   "$branch" \
  --arg codeql   "$codeql_v" \
  --argjson projects "$PROJECTS_JSON" \
  '{
    timestamp: $ts,
    repo:      $repo,
    commit:    $commit,
    branch:    $branch,
    codeql:    $codeql,
    projects:  $projects
  }' > "$OUT/codeql-report.json"

# ---------------------------------------------------------------------------
#  Brief stderr summary
# ---------------------------------------------------------------------------
log "summary:"
jq -r '
  .projects | to_entries[] |
  "  project: \(.key)",
  (.value | to_entries[] |
    "    \(.key): status=\(.value.status)"
    + (if .value.total_findings != null then "  findings=\(.value.total_findings)"   else "" end)
    + (if .value.errors        != null then "  errors=\(.value.errors)"             else "" end)
    + (if .value.warnings      != null then "  warnings=\(.value.warnings)"         else "" end)
    + (if .value.duration_seconds != null then "  (\(.value.duration_seconds)s)"    else "" end)
    + (if .value.sarif         != null then "  sarif=\(.value.sarif)"               else "" end)
  )
' "$OUT/codeql-report.json" >&2

log "report: $OUT/codeql-report.json"
echo "$OUT/codeql-report.json"
