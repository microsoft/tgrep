# Contributing to tgrep

Thanks for your interest in contributing! Here's how to get started.

## Development Setup

1. Install [Rust](https://rustup.rs/) (1.85+ required for edition 2024)
2. Clone and build:
   ```bash
   git clone https://github.com/microsoft/tgrep.git
   cd tgrep
   cargo build
   ```

## Workflow

```bash
# Run all checks (fmt + clippy + test)
make check
make test

# Or run individually:
cargo test

# Check formatting
cargo fmt --all --check

# Run clippy lints
cargo clippy --all-targets

# Build release binary
cargo build --release
```

## Recurring release qualification

The CI workflow runs on PRs and pushes to `main`, weekly on Monday at 08:00 UTC,
and manually with **Actions > CI > Run workflow**. Its native Linux, macOS and
Windows matrix is fail-fast disabled and bounded to 40 minutes per test job.
Existing Rust and Python agent integration coverage is retained. Workspace
builds/tests are locked; the separately excluded `vendor/ignore` manifest runs
its upstream unit/integration tests **and doctests** on each OS at version
0.4.25. Do not format or upgrade vendored code to satisfy unrelated checks;
see its [provenance and distribution contract](vendor/ignore/PATCHES.md).
Generic root-level `scripts/test_*.py` discovery also runs on each OS alongside
the explicit nested qualification/agent suites when those files exist. Older
checkouts with no standalone tests skip that step (Python 3.14 exits 5 for an
empty inventory); newly landed script tests are included without workflow edits.

Linux additionally validates both complete Cargo dependency graphs and checks
all four fuzz binaries with `--locked`. Exactly one `ignore`, resolving directly
from `tgrep-core` to `vendor/ignore`, is required in each graph. This is a stable
Cargo compile check using the host C++ compiler, not a sanitizer fuzz campaign;
the separate nightly weekly/manual Fuzz workflow remains unchanged.

An installed-release smoke runs on Linux for each PR/push and on **all three
OSes weekly/manually**. It runs the documented
`cargo install --path tgrep-cli --locked --root <private-temporary-root>`, then
uses that exact installed executable for ordinary index/server search and file
listing, shared attach/readiness/refresh, scan parity, stale-daemon fallback over
a deliberately stale ordinary index, restart, detach and worktree deletion while
the daemon remains alive. Both indexed and scan-control backend diagnostics are
required: matching results alone cannot qualify a broken indexed backend or a
`--no-index` regression. No runtime integration, user-home installation, signing
or release publication occurs.
Installed qualification adds one release-profile build on normal PR CI; the
managed measurement matrix separately builds its release test on each native
OS. Builds share Cargo's checkout target directory and downloads where Cargo permits.

To reproduce in an isolated source checkout with Cargo, Git, Python 3.11+ and
a C++ compiler available:

```bash
python -B -m unittest discover -s scripts/qualification -p 'test_*.py' -v
# When root-level scripts/test_*.py files exist:
python -B -m unittest discover -s scripts -p 'test_*.py' -v
cargo test --manifest-path vendor/ignore/Cargo.toml --locked
cargo build --locked --workspace
cargo test --locked --workspace
python -B scripts/qualification/qualify.py dependencies # Linux fuzz compile check
python -B scripts/qualification/qualify.py installed
git diff --exit-code -- Cargo.lock fuzz/Cargo.lock vendor/ignore/Cargo.lock
```

The helper uses private temporary Git fixtures, configuration, external shared
storage, logs and installation roots. Commands/readiness have deadlines; owned
processes are stopped and reaped before temporary roots are removed, including
on failure. Windows children start suspended, join an owned kill-on-close Job
Object, then resume; descendants remain owned even after their parent exits.
Unix children use private process groups. Windows temporary cleanup reports and
retries briefly retained executable images for at most five seconds; persistent
cleanup errors fail qualification. Cargo's normal cache is not removed.
All three lockfiles must remain
byte-for-byte unchanged, even on helper failure. For native Unix invalid-byte
regressions, run the Rust suite on native Linux/macOS filesystems; under WSL use
a native checkout and `TMPDIR`, not a Windows-mounted `/mnt/...` directory.
These checks qualify checkout-based distribution, not crates.io packaging or
the separate cross-build/signing pipelines.

Managed catalogs pin `rusqlite` to upstream Git revision
`2a71e35d94b2a02f7dd4a0c4cfd59c339370e747`, which bundles SQLite 3.53.3.
SQLite 3.53.2 can retain native WAL read locks after readers close when a
canonical Windows DOS-device path is mistaken for a UNC path. The upstream
correction preserves canonical path and identity checks; a separate native
integration test forces overlapping read-lock acquisition and requires
reclamation after both readers close. Core and CLI test dependencies share this
revision, including when core is consumed outside this workspace.

Both root and fuzz lockfiles record the immutable Git source. A fresh locked
checkout build, including the compliant internal pipeline, must retrieve that
revision and its bundled sources from GitHub or an approved pre-populated Cargo
Git cache; registry-feed access alone is insufficient. Configure that retrieval
within the pipeline's existing dependency/feed policy. This does not change
feed, signing or publication policy, and ordinary CI or checkout-install results
do not qualify that separate pipeline.

## Managed lifecycle qualification

The `installed` qualification also exercises managed v2 through the public CLI
of the normally installed release binary, without test hooks. It covers
`owner-hold`, exact-generation reuse with no blob reads, independent private
overlays, indexed RPC/CLI parity with forced scans, migration, exact receipt
replay and explicit detach/owner release. Ordinary and default-v1 installed
coverage remains enabled.

The managed lifecycle suite uses actual CLI/RPC clients, temporary Git
repositories, native filesystem/owner locks and the production owned-child
supervisor. Enable `managed-test-hooks` for deterministic preparation,
publication, retirement, deletion and recovery barriers/failures. The hook RPCs
are not compiled into normal binaries. Do not replace a barrier with a whole-test
retry, count scan fallback as indexed success, or disable a platform's lifetime
coverage.

Arm asynchronous operation hooks with `pause-token` or `error-token` and the
operation's persisted scope/sequence/token before submitting it. Admission binds
the hook to the exact operation ID before background dispatch can observe it;
global hooks can capture unrelated reconciliation or housekeeping instead.
Keep unscoped hooks only for automatic work whose token is not known in advance.

Compatibility tests require **original**, separately built v1 CLI and core
reader executables. They are not current binaries with a version label changed.
In a new scratch directory, prepare the pinned control sources:

```bash
git fetch --no-tags --depth=1 origin 1120aca41dd192ae61bdd996e9886cb354a78a27
git archive --format=zip --output="$SCRATCH/legacy.zip" 1120aca41dd192ae61bdd996e9886cb354a78a27
python -B scripts/qualification/prepare_legacy.py "$SCRATCH/legacy.zip" "$SCRATCH/legacy-source"
cargo build --locked --manifest-path "$SCRATCH/legacy-source/Cargo.toml" -p tgrep-cli --target-dir "$SCRATCH/legacy-target"
cargo build --locked --manifest-path "$SCRATCH/legacy-source/Cargo.toml" -p tgrep-core --example legacy_reader --target-dir "$SCRATCH/legacy-target"
export TGREP_V1_BINARY="$SCRATCH/legacy-target/debug/tgrep"
export TGREP_V1_READER="$SCRATCH/legacy-target/debug/examples/legacy_reader"
```

Set `SCRATCH` to an existing absolute caller-owned directory first; the helper
requires a new `legacy-source` destination, validates the Git archive's pinned
commit and bounded safe entries, and adds only the standalone old-core probe.
On Windows use the `.exe` suffix and PowerShell `$env:TGREP_V1_BINARY` /
`$env:TGREP_V1_READER` assignments, or run the shell example in Git Bash.
The native CI matrix supplies these controls automatically.

Run targeted coverage before the full workspace:

```bash
cargo test --locked -p tgrep-core --features managed-test-hooks managed:: -- --test-threads=2
cargo test --locked -p tgrep-core --features managed-test-hooks --test managed_lifecycle -- --test-threads=2
cargo test --locked -p tgrep-cli --features managed-test-hooks --test shared_daemon managed:: -- --test-threads=2
cargo test --locked --workspace
cargo test --locked --workspace --features tgrep-cli/managed-test-hooks -- --test-threads=2
cargo clippy --locked --workspace --all-targets --features tgrep-cli/managed-test-hooks -- -D warnings
```

Some ignored tests are subprocess entry points, not standalone cases. Do not
run the entire suite with `--ignored`: their supervisors construct their
environment and own cleanup. The explicitly named measurement below is a
separate deliberate ignored test.

### Native lifecycle measurements

`managed::performance::bounded_lifecycle_queries_and_measurement_shape` is the
small deterministic functional gate. Its injected pauses and debug timings are
**not** performance results. For a deliberate unpaused release measurement,
first build on stable source, then reserve a quiet host window with no concurrent
heavy builds or benchmarks:

```bash
cargo test --locked --release -p tgrep-cli --features managed-test-hooks \
  --test shared_daemon --no-run
export TGREP_MANAGED_PERFORMANCE_REPORT="$SCRATCH/managed-native.json"
cargo test --locked --release -p tgrep-cli --features managed-test-hooks \
  --test shared_daemon managed::performance::native_managed_lifecycle_measurement \
  -- --ignored --exact --test-threads=1 --nocapture
```

The report path must be absolute and new. PowerShell can set it with
`$env:TGREP_MANAGED_PERFORMANCE_REPORT = 'C:\scratch\managed-native.json'`.
Run on native Windows, Linux and macOS filesystems; WSL requires a native Linux
source/build/fixture tree, not DrvFS. The report records source-input and binary
fingerprints, toolchain, filesystem, query sample counts/distributions,
read/extraction counters, sealed publication sizes, storage/reservation samples
and platform-labeled process memory. A snapshot without Git history records that
fact rather than inventing a commit.

The fixture is bounded to two repositories, four initial views and one successor
view, with 256 generated 4096-byte files per repository. It verifies shared-v2
routing and actual CLI/scan parity while exercising reuse, migration, collection
and warm initialization. All children are owned and reaped. Report files remain
for review; scratch data is not installed into a user's cache.
See [measurement interpretation](SHARED_INDEX_BENCHMARKS.md#managed-lifecycle-measurements).

Native CI selects the CLI and test harness from the build's Cargo JSON records,
runs those exact executables, and retains both with their selection/SHA-256
manifest alongside the report. These are qualification artifacts, not signed
releases; no executable is selected by globbing an existing target directory.

## Pre-commit Hook

Install the git hook to auto-check formatting and lints before each commit:

```bash
make hooks
```

## Project Structure

- **tgrep-core** — Library: trigram index, on-disk format, walker, query planner
- **tgrep-cli** — Binary: CLI parsing, search output, TCP server, file watcher

## Pull Requests

1. Fork the repo and create a feature branch
2. Make your changes with tests if applicable
3. Ensure `cargo fmt`, `cargo clippy`, and `cargo test` all pass
4. Submit a PR with a clear description of the change

## Reporting Issues

Please include:
- OS and Rust version (`rustc --version`)
- Steps to reproduce
- Expected vs actual behavior
- Relevant log output (run with `tgrep serve` to see `[trace]` logs)

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
