# Benchmarks

All benchmarks measure **search time only** — the trigram index is built before timing starts.
tgrep runs in client/server mode: `tgrep serve` runs in the background, and the `tgrep` client connects via TCP.

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

## rust-lang/rust (~50K files)

- **Repo**: [rust-lang/rust](https://github.com/rust-lang/rust) (~50K files)
- **Queries**: 100 (mix of Rust patterns, macros, traits, and regex)
- **Status**: Pending — run the `Benchmark Rust` workflows to populate results

---

## kubernetes/kubernetes (~20K files)

- **Repo**: [kubernetes/kubernetes](https://github.com/kubernetes/kubernetes) (~20K files)
- **Queries**: 96 (mix of Go patterns, Kubernetes API types, and regex)
- **Status**: Pending — run the `Benchmark Kubernetes` workflows to populate results

---

## golang/go (~10K files)

- **Repo**: [golang/go](https://github.com/golang/go) (~10K files)
- **Queries**: 100 (mix of Go stdlib patterns, testing, and regex)
- **Status**: Pending — run the `Benchmark Go` workflows to populate results

---

## Key takeaways

- tgrep's advantage grows with repo size — the trigram index eliminates scanning files that can't match
- On the 388K-file gecko-dev repo, tgrep is up to **98x faster** (macOS) and **25x faster** (Windows)
- On the 493K-file Chromium repo, tgrep is up to **21x faster** (macOS) and **10x faster** (Windows)
- On Linux, aggressive OS page caching narrows the gap, but tgrep is still **2.6–4x faster** on large repos
- On smaller repos (93K files), the gap narrows because OS page cache helps ripgrep's brute-force approach
- Index build is a one-time cost; the server watches for file changes and updates incrementally
