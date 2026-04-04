#!/usr/bin/env bash
#
# Benchmark tgrep vs ripgrep vs grep on Linux/macOS.
#
# Usage:
#   ./scripts/benchmark.sh                                    # full run
#   ./scripts/benchmark.sh --repo-path /src/myrepo --skip-build
#
set -euo pipefail

# ── Defaults ──
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

REPO_PATH=""
REPO_URL="https://github.com/torvalds/linux.git"
BENCH_DIR="/tmp/tgrep-bench"
TGREP_BIN=""
RESULTS_PATH=""
SKIP_BUILD=false

# ── Parse args ──
while [[ $# -gt 0 ]]; do
  case "$1" in
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
      echo "  --repo-path PATH   Existing repo to benchmark (skips cloning)"
      echo "  --repo-url  URL    URL to clone for benchmarking (default: torvalds/linux)"
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
  BENCH_REPO_DIR="$BENCH_DIR/linux"
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
"$TGREP_BIN" index "$BENCH_REPO_DIR" --index-path "$INDEX_PATH"

# ── 30 search patterns (mix of literals, multi-word, and regex) ──
QUERIES=(
  'mutex_lock'
  'printk'
  'EXPORT_SYMBOL'
  'kfree'
  'kmalloc'
  'BUG_ON'
  'pr_err'
  'unlikely'
  'IS_ERR'
  'container_of'
  'ARRAY_SIZE'
  '__init'
  'module_init'
  'platform_driver'
  'struct device'
  'struct file'
  'struct sk_buff'
  'struct task_struct'
  'struct page'
  'alloc_chrdev_region'
  'proc_create'
  'ioctl'
  'read'
  'TODO'
  'FIXME'
  'SPDX-License-Identifier'
  '^#include <linux/'
  '^#define\s+[A-Z_]+'
  'for_each_\w+'
  '#ifdef\s+CONFIG_'
)

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
if command -v rg >/dev/null 2>&1; then
  echo ""
  echo "==> Benchmarking ripgrep..."
  RG_START=$(now_ns)
  for pattern in "${QUERIES[@]}"; do
    rg -n "$pattern" "$BENCH_REPO_DIR" > /dev/null 2>&1 || true
  done
  RG_END=$(now_ns)
  RG_MS=$(( (RG_END - RG_START) / 1000000 ))
  echo "ripgrep: ${RG_MS}ms total"
else
  echo "ripgrep (rg) not found in PATH, skipping"
fi

# ── Write results ──
TGREP_AVG=$(awk "BEGIN { printf \"%.1f\", $TGREP_MS / $QUERY_COUNT }")
TIMESTAMP=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
REPO_NAME=$(basename "$BENCH_REPO_DIR")
PLATFORM="$(uname -s) $(uname -m)"

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
- **Scope**: search only (index built before timing)
- **tgrep mode**: client/server — \`tgrep serve\` runs in background, \`tgrep\` client connects via TCP

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
EOF

if [ "$RG_MS" -ge 0 ]; then
  RG_AVG=$(awk "BEGIN { printf \"%.1f\", $RG_MS / $QUERY_COUNT }")
  echo "| ripgrep | $RG_MS | $RG_AVG |" >> "$RESULTS_PATH"
fi
echo "| tgrep (client → serve) | $TGREP_MS | $TGREP_AVG |" >> "$RESULTS_PATH"

echo ""
cat "$RESULTS_PATH"
echo ""
echo "Results saved to $RESULTS_PATH"
