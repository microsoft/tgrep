# Fuzz Testing

tgrep uses [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) with
[libFuzzer](https://llvm.org/docs/LibFuzzer.html) for fuzz testing.

## Setup

```bash
cargo install cargo-fuzz
```

## Targets

| Target | What it fuzzes |
|--------|---------------|
| `fuzz_trigram` | Trigram extraction, mask generation, binary detection |
| `fuzz_query` | Regex → query plan decomposition (arbitrary patterns) |
| `fuzz_ondisk` | On-disk format encode/decode roundtrips |

## Running

```bash
cd fuzz

# Run a specific target
cargo fuzz run fuzz_trigram
cargo fuzz run fuzz_query
cargo fuzz run fuzz_ondisk

# Run with a time limit (e.g., 60 seconds)
cargo fuzz run fuzz_trigram -- -max_total_time=60

# List all targets
cargo fuzz list
```

## Notes

- Requires a nightly Rust toolchain: `rustup install nightly`
- `cargo fuzz` automatically uses nightly when invoked
- Crash inputs are saved to `fuzz/artifacts/<target>/`
- Corpus is saved to `fuzz/corpus/<target>/` for incremental fuzzing
