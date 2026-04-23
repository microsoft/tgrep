# Benchmarks

All benchmarks measure **search time only** — the trigram index is built before timing starts.
tgrep runs in client/server mode: `tgrep serve` runs in the background, and the `tgrep` client connects via TCP.

---

## chromium/chromium (494K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24805535539)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24805536677)

- **Repo**: [chromium/chromium](https://github.com/chromium/chromium) (494,093 files)
- **Queries**: 30 (mix of literals, multi-word, and regex)
- **Index build time**: ~112s (Linux), ~293s (Windows), ~497s (macOS)
- **Index size**: 2,478 MB (~2.5 GB)

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 873,795 | 29,126.5 |
| tgrep (client → serve) | 73,521 | 2,450.7 |

**tgrep is ~12x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 2,079,314 | 69,310.5 |
| tgrep (client → serve) | 87,352 | 2,911.7 |

**tgrep is ~24x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 83,876 | 2,795.9 |
| tgrep (client → serve) | 36,314 | 1,210.5 |

**tgrep is ~2.3x faster**

---

## mozilla/gecko-dev (388K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24805537755)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24805538770)

- **Repo**: [mozilla/gecko-dev](https://github.com/mozilla/gecko-dev) (387,841 files)
- **Queries**: 122 (mix of C++, JavaScript, and Python patterns)
- **Index build time**: ~92s (Linux), ~180s (Windows), ~257s (macOS)
- **Index size**: 1,938 MB (~1.9 GB)

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 2,301,586 | 18,865.5 |
| tgrep (client → serve) | 83,381 | 683.5 |

**tgrep is ~28x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 5,579,958 | 45,737.4 |
| tgrep (client → serve) | 62,027 | 508.4 |

**tgrep is ~90x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 243,421 | 1,995.3 |
| tgrep (client → serve) | 33,463 | 274.3 |

**tgrep is ~7x faster**

---

## torvalds/linux (94K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24805547294)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24805548476)

- **Repo**: [torvalds/linux](https://github.com/torvalds/linux) (93,931 files)
- **Queries**: 102 (mix of literals, multi-word, and regex)
- **Index build time**: ~41s (Linux), ~50s (Windows), ~144s (macOS)
- **Index size**: ~977 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 323,962 | 3,176.1 |
| tgrep (client → serve) | 75,612 | 741.3 |

**tgrep is ~4x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 343,684 | 3,369.5 |
| tgrep (client → serve) | 165,523 | 1,622.8 |

**tgrep is ~2x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 47,120 | 462.0 |
| tgrep (client → serve) | 49,511 | 485.4 |

**~1x (comparable — Linux I/O cache narrows the gap on this repo)**

---

## rust-lang/rust (59K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24805544808)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24805546001)

- **Repo**: [rust-lang/rust](https://github.com/rust-lang/rust) (59,267 files)
- **Queries**: 102 (mix of Rust patterns, macros, traits, and regex)
- **Index build time**: ~7s (Linux), ~11s (Windows/macOS)
- **Index size**: ~189 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 203,367 | 1,993.8 |
| tgrep (client → serve) | 19,362 | 189.8 |

**tgrep is ~10x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 38,985 | 382.2 |
| tgrep (client → serve) | 12,795 | 125.4 |

**tgrep is ~3x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 13,909 | 136.4 |
| tgrep (client → serve) | 9,709 | 95.2 |

**tgrep is ~1.4x faster**

---

## kubernetes/kubernetes (29K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24805542622)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24805543634)

- **Repo**: [kubernetes/kubernetes](https://github.com/kubernetes/kubernetes) (29,232 files)
- **Queries**: 97 (mix of Go patterns, Kubernetes API types, and regex)
- **Index build time**: ~9s (Linux/macOS), ~9s (Windows)
- **Index size**: ~203 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 95,187 | 981.3 |
| tgrep (client → serve) | 13,253 | 136.6 |

**tgrep is ~7x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 28,575 | 294.6 |
| tgrep (client → serve) | 8,711 | 89.8 |

**tgrep is ~3x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 15,659 | 161.4 |
| tgrep (client → serve) | 8,505 | 87.7 |

**tgrep is ~1.8x faster**

---

## golang/go (15K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24805540173)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24805541524)

- **Repo**: [golang/go](https://github.com/golang/go) (15,302 files)
- **Queries**: 103 (mix of Go stdlib patterns, testing, and regex)
- **Index build time**: ~4s (Linux), ~5s (macOS), ~6s (Windows)
- **Index size**: ~105 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 62,542 | 607.2 |
| tgrep (client → serve) | 7,500 | 72.8 |

**tgrep is ~8x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 12,250 | 118.9 |
| tgrep (client → serve) | 5,304 | 51.5 |

**tgrep is ~2.3x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 7,192 | 69.8 |
| tgrep (client → serve) | 3,816 | 37.0 |

**tgrep is ~1.9x faster**

---

## Key takeaways

- tgrep's advantage grows with repo size — the trigram index eliminates scanning files that can't match
- On the 494K-file Chromium repo, tgrep is up to **24x faster** (macOS) and **12x faster** (Windows)
- On the 388K-file gecko-dev repo, tgrep is up to **90x faster** (macOS) and **28x faster** (Windows)
- On the 94K-file Linux kernel, tgrep is **4x faster** on Windows, ~2x on macOS; on Linux x86_64 the two are comparable
- On the 59K-file Rust repo, tgrep is **10x faster** on Windows, ~3x on macOS, ~1.4x on Linux
- On the 29K-file Kubernetes repo, tgrep is **7x faster** on Windows, ~3x on macOS, ~1.8x on Linux
- On the 15K-file Go repo, tgrep is **8x faster** on Windows, ~2x on macOS/Linux
- On Windows, tgrep consistently shows the largest speedups (4–28x) due to slower Windows I/O
- On Linux, aggressive OS page caching often narrows the gap; tgrep is usually faster, but some repos (such as the Linux kernel on x86_64) can be comparable
- Index build is a one-time cost; the server watches for file changes and updates incrementally
