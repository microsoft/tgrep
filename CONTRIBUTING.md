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
Only one release-profile build is added to normal PR CI; the existing builds
share Cargo's checkout target directory and downloads where Cargo permits.

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
