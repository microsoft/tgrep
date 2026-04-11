# Benchmarks

All benchmarks measure **search time only** — the trigram index is built before timing starts.
tgrep runs in client/server mode: `tgrep serve` runs in the background, and the `tgrep` client connects via TCP.

---

## chromium/chromium (493K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24171177161)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24171159517)

- **Repo**: [chromium/chromium](https://github.com/chromium/chromium) (492,734 files)
- **Queries**: 30 (mix of literals, multi-word, and regex)
- **Index build time**: ~106s (Linux), ~312s (Windows), ~668s (macOS)
- **Index size**: 2,470 MB (~2.4 GB)

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 770,353 | 25,678.4 |
| tgrep (client → serve) | 75,473 | 2,515.8 |

**tgrep is ~10x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 1,825,058 | 60,835.3 |
| tgrep (client → serve) | 88,415 | 2,947.2 |

**tgrep is ~21x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 95,974 | 3,199.1 |
| tgrep (client → serve) | 37,483 | 1,249.4 |

**tgrep is ~2.6x faster**

---

## mozilla/gecko-dev (388K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24166560879)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24166565600)

- **Repo**: [mozilla/gecko-dev](https://github.com/mozilla/gecko-dev) (387,841 files)
- **Queries**: 122 (mix of C++, JavaScript, and Python patterns)
- **Index build time**: ~84s (Linux), ~239s (Windows), ~292s (macOS)
- **Index size**: 1,938 MB (~1.9 GB)

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 1,840,166 | 15,083.3 |
| tgrep (client → serve) | 73,847 | 605.3 |

**tgrep is ~25x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 5,191,142 | 42,550.3 |
| tgrep (client → serve) | 53,166 | 435.8 |

**tgrep is ~98x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 158,795 | 1,301.6 |
| tgrep (client → serve) | 39,119 | 320.6 |

**tgrep is ~4x faster**

---

## torvalds/linux (93K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/23984630704)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/23984627799)

- **Repo**: [torvalds/linux](https://github.com/torvalds/linux) (93,023 files)
- **Queries**: 102 (mix of literals, multi-word, and regex)
- **Index build time**: ~61s (Linux/macOS), ~110s (Windows)
- **Index size**: ~969 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 531,054 | 5,206.4 |
| tgrep (client → serve) | 130,938 | 1,283.7 |

**tgrep is ~4x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 93,843 | 920.0 |
| tgrep (client → serve) | 63,896 | 626.4 |

**tgrep is ~1.5x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 103,194 | 1,011.7 |
| tgrep (client → serve) | 128,157 | 1,256.4 |

**tgrep is ~0.8x (ripgrep faster on small repos with Linux I/O cache)**

---

## rust-lang/rust (59K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24288075733)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24288075258)

- **Repo**: [rust-lang/rust](https://github.com/rust-lang/rust) (58,949 files)
- **Queries**: 102 (mix of Rust patterns, macros, traits, and regex)
- **Index build time**: ~8s (Linux), ~10s (Windows), ~13s (macOS)
- **Index size**: ~188 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 184,124 | 1,805.1 |
| tgrep (client → serve) | 23,359 | 229.0 |

**tgrep is ~8x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 42,080 | 412.5 |
| tgrep (client → serve) | 26,214 | 257.0 |

**tgrep is ~1.6x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 18,851 | 184.8 |
| tgrep (client → serve) | 10,658 | 104.5 |

**tgrep is ~1.8x faster**

---

## kubernetes/kubernetes (29K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24288076754)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24288076157)

- **Repo**: [kubernetes/kubernetes](https://github.com/kubernetes/kubernetes) (29,226 files)
- **Queries**: 97 (mix of Go patterns, Kubernetes API types, and regex)
- **Index build time**: ~8s (Linux), ~11s (Windows/macOS)
- **Index size**: ~203 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 144,506 | 1,489.8 |
| tgrep (client → serve) | 13,886 | 143.2 |

**tgrep is ~10x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 35,611 | 367.1 |
| tgrep (client → serve) | 14,135 | 145.7 |

**tgrep is ~2.5x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 15,457 | 159.4 |
| tgrep (client → serve) | 7,907 | 81.5 |

**tgrep is ~2x faster**

---

## golang/go (15K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/24288077547)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/24288077162)

- **Repo**: [golang/go](https://github.com/golang/go) (15,267 files)
- **Queries**: 103 (mix of Go stdlib patterns, testing, and regex)
- **Index build time**: ~4s (Linux/macOS), ~5s (Windows)
- **Index size**: ~105 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 63,254 | 614.1 |
| tgrep (client → serve) | 6,683 | 64.9 |

**tgrep is ~9.5x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 12,144 | 117.9 |
| tgrep (client → serve) | 2,935 | 28.5 |

**tgrep is ~4x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 7,043 | 68.4 |
| tgrep (client → serve) | 3,551 | 34.5 |

**tgrep is ~2x faster**

---

## Key takeaways

- tgrep's advantage grows with repo size — the trigram index eliminates scanning files that can't match
- On the 493K-file Chromium repo, tgrep is up to **21x faster** (macOS) and **10x faster** (Windows)
- On the 388K-file gecko-dev repo, tgrep is up to **98x faster** (macOS) and **25x faster** (Windows)
- On the 93K-file Linux kernel, tgrep is **4x faster** on Windows, ~1.5x on macOS, but Linux x86_64 is a case where ripgrep can be faster
- On the 59K-file Rust repo, tgrep is **8x faster** on Windows, ~1.6–1.8x on macOS/Linux
- On the 29K-file Kubernetes repo, tgrep is **10x faster** on Windows, ~2–2.5x on macOS/Linux
- On the 15K-file Go repo, tgrep is **9.5x faster** on Windows, ~2–4x on macOS/Linux
- On Windows, tgrep consistently shows the largest speedups (8–25x) due to slower Windows I/O
- On Linux, aggressive OS page caching often narrows the gap; tgrep is usually faster, but some repos (such as the Linux kernel on x86_64) can favor ripgrep
- Index build is a one-time cost; the server watches for file changes and updates incrementally
