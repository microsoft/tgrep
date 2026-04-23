# Benchmarks

All benchmarks measure **search time only** — the trigram index is built before timing starts.
tgrep runs in client/server mode: `tgrep serve` runs in the background, and the `tgrep` client connects via TCP.

---

## chromium/chromium (494K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24811114641)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24811115740)

- **Repo**: [chromium/chromium](https://github.com/chromium/chromium) (494,120 files)
- **Queries**: 30 (mix of literals, multi-word, and regex)
- **Index build time**: ~97s (Linux), ~688s (Windows), ~540s (macOS)
- **Index size**: 2,478 MB (~2.5 GB)

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 848,569 | 28,285.6 |
| tgrep (client → serve) | 73,143 | 2,438.1 |

**tgrep is ~12x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 2,146,196 | 71,539.9 |
| tgrep (client → serve) | 91,716 | 3,057.2 |

**tgrep is ~23x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 83,530 | 2,784.3 |
| tgrep (client → serve) | 34,688 | 1,156.3 |

**tgrep is ~2.4x faster**

---

## mozilla/gecko-dev (388K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24811116791)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24811117829)

- **Repo**: [mozilla/gecko-dev](https://github.com/mozilla/gecko-dev) (387,841 files)
- **Queries**: 122 (mix of C++, JavaScript, and Python patterns)
- **Index build time**: ~77s (Linux), ~314s (Windows), ~267s (macOS)
- **Index size**: 1,938 MB (~1.9 GB)

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 2,048,034 | 16,787.2 |
| tgrep (client → serve) | 67,794 | 555.7 |

**tgrep is ~30x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 4,724,399 | 38,724.6 |
| tgrep (client → serve) | 72,273 | 592.4 |

**tgrep is ~65x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 212,783 | 1,744.1 |
| tgrep (client → serve) | 34,589 | 283.5 |

**tgrep is ~6x faster**

---

## torvalds/linux (94K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24811125540)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24811126603)

- **Repo**: [torvalds/linux](https://github.com/torvalds/linux) (93,931 files)
- **Queries**: 102 (mix of literals, multi-word, and regex)
- **Index build time**: ~37s (Linux), ~77s (Windows), ~90s (macOS)
- **Index size**: ~977 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 319,356 | 3,130.9 |
| tgrep (client → serve) | 77,352 | 758.4 |

**tgrep is ~4x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 123,740 | 1,213.1 |
| tgrep (client → serve) | 100,269 | 983.0 |

**tgrep is ~1.2x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 36,599 | 358.8 |
| tgrep (client → serve) | 49,765 | 487.9 |

**~1x (comparable — Linux I/O cache narrows the gap on this repo)**

---

## rust-lang/rust (59K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24811123386)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24811124502)

- **Repo**: [rust-lang/rust](https://github.com/rust-lang/rust) (59,293 files)
- **Queries**: 102 (mix of Rust patterns, macros, traits, and regex)
- **Index build time**: ~7s (Linux), ~8s (Windows), ~9s (macOS)
- **Index size**: ~189 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 137,992 | 1,352.9 |
| tgrep (client → serve) | 16,186 | 158.7 |

**tgrep is ~9x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 42,115 | 412.9 |
| tgrep (client → serve) | 10,941 | 107.3 |

**tgrep is ~4x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 19,216 | 188.4 |
| tgrep (client → serve) | 9,912 | 97.2 |

**tgrep is ~1.9x faster**

---

## kubernetes/kubernetes (29K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24811121029)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24811122255)

- **Repo**: [kubernetes/kubernetes](https://github.com/kubernetes/kubernetes) (29,242 files)
- **Queries**: 97 (mix of Go patterns, Kubernetes API types, and regex)
- **Index build time**: ~8s (Linux/macOS), ~8s (Windows)
- **Index size**: ~203 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 99,009 | 1,020.7 |
| tgrep (client → serve) | 13,362 | 137.8 |

**tgrep is ~7x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 24,786 | 255.5 |
| tgrep (client → serve) | 6,402 | 66.0 |

**tgrep is ~4x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 15,314 | 157.9 |
| tgrep (client → serve) | 8,130 | 83.8 |

**tgrep is ~1.9x faster**

---

## golang/go (15K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24811118882)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24811120023)

- **Repo**: [golang/go](https://github.com/golang/go) (15,302 files)
- **Queries**: 103 (mix of Go stdlib patterns, testing, and regex)
- **Index build time**: ~4s (Linux), ~3s (macOS), ~4s (Windows)
- **Index size**: ~105 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 45,996 | 446.6 |
| tgrep (client → serve) | 6,130 | 59.5 |

**tgrep is ~8x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 12,587 | 122.2 |
| tgrep (client → serve) | 2,893 | 28.1 |

**tgrep is ~4x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 7,276 | 70.6 |
| tgrep (client → serve) | 3,958 | 38.4 |

**tgrep is ~1.8x faster**

---

## Key takeaways

- tgrep's advantage grows with repo size — the trigram index eliminates scanning files that can't match
- On the 494K-file Chromium repo, tgrep is up to **23x faster** (macOS) and **12x faster** (Windows)
- On the 388K-file gecko-dev repo, tgrep is up to **65x faster** (macOS) and **30x faster** (Windows)
- On the 94K-file Linux kernel, tgrep is **4x faster** on Windows; on Linux/macOS the two are comparable
- On the 59K-file Rust repo, tgrep is **9x faster** on Windows, ~4x on macOS, ~2x on Linux
- On the 29K-file Kubernetes repo, tgrep is **7x faster** on Windows, ~4x on macOS, ~2x on Linux
- On the 15K-file Go repo, tgrep is **8x faster** on Windows, ~4x on macOS, ~2x on Linux
- On Windows, tgrep consistently shows the largest speedups (4–30x) due to slower Windows I/O
- On Linux, aggressive OS page caching often narrows the gap; tgrep is usually faster, but some repos (such as the Linux kernel on x86_64) can be comparable
- Index build is a one-time cost; the server watches for file changes and updates incrementally
