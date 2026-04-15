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
