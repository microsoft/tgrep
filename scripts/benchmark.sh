#!/usr/bin/env bash
#
# Benchmark tgrep vs ripgrep on Linux/macOS.
#
# Usage:
#   ./scripts/benchmark.sh                                    # full run
#   ./scripts/benchmark.sh --repo-path /src/myrepo --skip-build
#
set -euo pipefail

# ── Defaults ──
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

QUERIES_FILE=""
REPO_PATH=""
REPO_URL=""
BENCH_DIR="/tmp/tgrep-bench"
TGREP_BIN=""
RESULTS_PATH=""
SKIP_BUILD=false

# ── Parse args ──
while [[ $# -gt 0 ]]; do
  case "$1" in
    --queries)     QUERIES_FILE="$2"; shift 2 ;;
    --repo-path)   REPO_PATH="$2"; shift 2 ;;
    --repo-url)    REPO_URL="$2"; shift 2 ;;
    --bench-dir)   BENCH_DIR="$2"; shift 2 ;;
    --tgrep-bin)   TGREP_BIN="$2"; shift 2 ;;
    --results)     RESULTS_PATH="$2"; shift 2 ;;
    --skip-build)  SKIP_BUILD=true; shift ;;
    -h|--help)
      echo "Usage: $(basename "$0") [OPTIONS]"
      echo ""
      echo "Options:"
      echo "  --queries   PATH   JSON file with repo_url and queries array"
      echo "                     (default: scripts/benchmark-queries.json)"
      echo "  --repo-path PATH   Existing repo to benchmark (skips cloning)"
      echo "  --repo-url  URL    URL to clone (overrides value from queries JSON)"
      echo "  --bench-dir DIR    Working directory for artifacts (default: /tmp/tgrep-bench)"
      echo "  --tgrep-bin PATH   Path to tgrep binary (default: target/release/tgrep)"
      echo "  --results   PATH   Output path for results markdown"
      echo "  --skip-build       Skip building tgrep from source"
      exit 0
      ;;
    *) echo "Unknown option: $1 (use --help for usage)" >&2; exit 1 ;;
  esac
done

# ── Resolve paths ──
if [ -z "$QUERIES_FILE" ]; then
  QUERIES_FILE="$SCRIPT_DIR/benchmark-queries.json"
fi
if [ ! -f "$QUERIES_FILE" ]; then
  echo "error: queries file not found: $QUERIES_FILE" >&2
  exit 1
fi

# ── Load queries JSON ──
if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 is required to parse the queries JSON file" >&2
  exit 1
fi

if [ -z "$REPO_URL" ]; then
  REPO_URL=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['repo_url'])" "$QUERIES_FILE")
fi
if [ -z "$REPO_URL" ]; then
  echo "error: no repo_url specified (set in queries JSON or pass --repo-url)" >&2
  exit 1
fi

# Read queries into a shell array (compatible with bash 3 / macOS)
QUERIES=()
while IFS= read -r line; do
  QUERIES+=("$line")
done < <(python3 -c "
import json, sys
data = json.load(open(sys.argv[1]))
for q in data['queries']:
    print(q)
" "$QUERIES_FILE")

if [ ${#QUERIES[@]} -eq 0 ]; then
  echo "error: no queries found in $QUERIES_FILE" >&2
  exit 1
fi

if [ -z "$TGREP_BIN" ]; then
  TGREP_BIN="$REPO_ROOT/target/release/tgrep"
fi
if [ -z "$RESULTS_PATH" ]; then
  RESULTS_PATH="$BENCH_DIR/benchmark-results.md"
fi

INDEX_PATH="$BENCH_DIR/tgrep-index"

if [ -n "$REPO_PATH" ]; then
  BENCH_REPO_DIR="$REPO_PATH"
else
  # Derive clone directory name from repo URL (e.g. linux, chromium)
  CLONE_NAME=$(basename "$REPO_URL" .git)
  BENCH_REPO_DIR="$BENCH_DIR/$CLONE_NAME"
fi

# GNU date supports %N for nanoseconds; macOS/BSD does not
now_ns() {
  local ns
  ns=$(date +%s%N 2>/dev/null)
  if [[ "$ns" == *N* ]]; then
    python3 -c "import time; print(int(time.time() * 1e9))"
  else
    echo "$ns"
  fi
}

# ── Build ──
if [ "$SKIP_BUILD" = false ]; then
  echo "==> Building tgrep (release)..."
  (cd "$REPO_ROOT" && cargo build --release)
fi

if [ ! -x "$TGREP_BIN" ]; then
  echo "error: tgrep binary not found at $TGREP_BIN" >&2
  echo "run 'make release' first or pass --tgrep-bin" >&2
  exit 1
fi

# ── Clone benchmark repo ──
if [ -z "$REPO_PATH" ]; then
  if [ ! -d "$BENCH_REPO_DIR" ]; then
    echo "==> Cloning $REPO_URL (shallow)..."
    mkdir -p "$BENCH_DIR"
    git clone --depth 1 "$REPO_URL" "$BENCH_REPO_DIR"
  else
    echo "==> Using existing repo at $BENCH_REPO_DIR"
  fi
fi

if [ ! -d "$BENCH_REPO_DIR" ]; then
  echo "error: benchmark repo not found at $BENCH_REPO_DIR" >&2
  exit 1
fi

# ── Count files ──
FILE_COUNT=$(git -C "$BENCH_REPO_DIR" ls-files | wc -l | tr -d ' ')

# ── Build index ──
echo "==> Building tgrep index..."
INDEX_START=$(now_ns)
"$TGREP_BIN" index "$BENCH_REPO_DIR" --index-path "$INDEX_PATH"
INDEX_END=$(now_ns)
INDEX_MS=$(( (INDEX_END - INDEX_START) / 1000000 ))
echo "Index built in ${INDEX_MS}ms"

QUERY_COUNT=${#QUERIES[@]}
echo "==> Running $QUERY_COUNT queries against $FILE_COUNT files"

# ── Start tgrep serve ──
echo "==> Starting tgrep serve..."

LOCKFILE="$INDEX_PATH/serve.json"
rm -f "$LOCKFILE"

"$TGREP_BIN" serve "$BENCH_REPO_DIR" --index-path "$INDEX_PATH" --no-watch > /dev/null 2>&1 &
SERVE_PID=$!

cleanup_serve() {
  kill "$SERVE_PID" 2>/dev/null || true
  wait "$SERVE_PID" 2>/dev/null || true
  return 0
}
trap 'cleanup_serve' EXIT

echo "Waiting for tgrep serve (pid $SERVE_PID)..."
READY=false
for i in $(seq 1 60); do
  if [ -f "$LOCKFILE" ]; then
    PORT=$(grep -o '"port":[0-9]*' "$LOCKFILE" | grep -o '[0-9]*')
    echo "tgrep serve ready on port $PORT"
    READY=true
    break
  fi
  sleep 1
done

if [ "$READY" = false ]; then
  echo "ERROR: tgrep serve failed to start within 60s" >&2
  exit 1
fi

# ── Benchmark: tgrep (client → serve) ──
echo ""
log_mem() {
  if [ -f /proc/meminfo ]; then
    awk '/MemTotal|MemAvailable|MemFree|SwapTotal|SwapFree/ {printf "  %s %s\n", $1, $2" "$3}' /proc/meminfo
  elif command -v vm_stat >/dev/null 2>&1; then
    vm_stat | head -10
  fi
}

echo "==> Memory before tgrep benchmark:"
log_mem

echo "==> Benchmarking tgrep (client -> serve)..."
TGREP_START=$(now_ns)
QIDX=0
for pattern in "${QUERIES[@]}"; do
  QIDX=$((QIDX + 1))
  echo "  [$QIDX/$QUERY_COUNT] $pattern"
  "$TGREP_BIN" "$pattern" "$BENCH_REPO_DIR" --index-path "$INDEX_PATH" > /dev/null 2>&1 || true
done
TGREP_END=$(now_ns)
TGREP_MS=$(( (TGREP_END - TGREP_START) / 1000000 ))
echo "tgrep: ${TGREP_MS}ms total"

echo "==> Memory after tgrep benchmark:"
log_mem

# ── Stop serve ──
echo "==> Stopping tgrep serve..."
trap - EXIT
cleanup_serve

# ── Benchmark: ripgrep ──
RG_MS=-1
RG_TIMEOUTS=0
RG_TIMEOUT_SEC=120
if command -v rg >/dev/null 2>&1; then
  # Detect timeout command (macOS needs gtimeout from coreutils)
  TIMEOUT_CMD=""
  if command -v timeout >/dev/null 2>&1; then
    TIMEOUT_CMD="timeout"
  elif command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT_CMD="gtimeout"
  fi

  echo ""
  echo "==> Benchmarking ripgrep (${RG_TIMEOUT_SEC}s timeout per query)..."
  RG_START=$(now_ns)
  QIDX=0
  for pattern in "${QUERIES[@]}"; do
    QIDX=$((QIDX + 1))
    echo "  [$QIDX/$QUERY_COUNT] $pattern"
    if [ -n "$TIMEOUT_CMD" ]; then
      rc=0
      $TIMEOUT_CMD "$RG_TIMEOUT_SEC" rg -n "$pattern" "$BENCH_REPO_DIR" > /dev/null 2>&1 || rc=$?
      if [ $rc -eq 124 ]; then
        echo "    ⚠ timed out (${RG_TIMEOUT_SEC}s)"
        RG_TIMEOUTS=$((RG_TIMEOUTS + 1))
      fi
    else
      rg -n "$pattern" "$BENCH_REPO_DIR" > /dev/null 2>&1 || true
    fi
  done
  RG_END=$(now_ns)
  RG_MS=$(( (RG_END - RG_START) / 1000000 ))
  echo "ripgrep: ${RG_MS}ms total"
  if [ $RG_TIMEOUTS -gt 0 ]; then
    echo "ripgrep: $RG_TIMEOUTS/$QUERY_COUNT queries timed out (${RG_TIMEOUT_SEC}s limit)"
  fi
else
  echo "ripgrep (rg) not found in PATH, skipping"
fi

# ── Write results ──
TGREP_AVG=$(awk "BEGIN { printf \"%.1f\", $TGREP_MS / $QUERY_COUNT }")
TIMESTAMP=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
REPO_NAME=$(basename "$BENCH_REPO_DIR")
PLATFORM="$(uname -s) $(uname -m)"

# Calculate index size
INDEX_SIZE_BYTES=0
for f in "$INDEX_PATH"/index.bin "$INDEX_PATH"/lookup.bin "$INDEX_PATH"/files.bin "$INDEX_PATH"/meta.json; do
  if [ -f "$f" ]; then
    INDEX_SIZE_BYTES=$(( INDEX_SIZE_BYTES + $(wc -c < "$f" | tr -d ' ') ))
  fi
done
if [ "$INDEX_SIZE_BYTES" -ge 1048576 ]; then
  INDEX_SIZE=$(awk "BEGIN { printf \"%.1f MB\", $INDEX_SIZE_BYTES / 1048576 }")
elif [ "$INDEX_SIZE_BYTES" -ge 1024 ]; then
  INDEX_SIZE=$(awk "BEGIN { printf \"%.1f KB\", $INDEX_SIZE_BYTES / 1024 }")
else
  INDEX_SIZE="${INDEX_SIZE_BYTES} B"
fi

RESULTS_DIR="$(dirname "$RESULTS_PATH")"
if [ -n "$RESULTS_DIR" ] && [ "$RESULTS_DIR" != "." ]; then
  mkdir -p "$RESULTS_DIR"
fi

cat > "$RESULTS_PATH" <<EOF
# Benchmark: ${QUERY_COUNT}-query search on repo: $REPO_NAME

- **Repo**: $BENCH_REPO_DIR
- **Files**: $FILE_COUNT
- **Queries**: $QUERY_COUNT
- **Date**: $TIMESTAMP
- **Platform**: $PLATFORM
- **Index build time**: ${INDEX_MS}ms
- **Index size**: $INDEX_SIZE
- **Scope**: search only (index built before timing)
- **tgrep mode**: client/server — \`tgrep serve\` runs in background, \`tgrep\` client connects via TCP

| Tool | Total (ms) | Avg per query (ms) | Timeouts (${RG_TIMEOUT_SEC}s) |
| --- | ---: | ---: | ---: |
EOF

if [ "$RG_MS" -ge 0 ]; then
  RG_AVG=$(awk "BEGIN { printf \"%.1f\", $RG_MS / $QUERY_COUNT }")
  echo "| ripgrep | $RG_MS | $RG_AVG | $RG_TIMEOUTS |" >> "$RESULTS_PATH"
fi
echo "| tgrep (client → serve) | $TGREP_MS | $TGREP_AVG | - |" >> "$RESULTS_PATH"

echo ""
cat "$RESULTS_PATH"
echo ""
echo "Results saved to $RESULTS_PATH"
