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

## chromium/chromium (492K files)

[Benchmark Run (macOS)](https://github.com/microsoft/tgrep/actions/runs/23994708937)

- **Repo**: [chromium/chromium](https://github.com/chromium/chromium) (491,632 files)
- **Queries**: 121 (mix of literals, multi-word, and regex)
- **Index build time**: ~498s (~8 min)
- **Index size**: 2,467 MB (~2.4 GB)

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 5,653,497 | 46,723.1 |
| tgrep (client → serve) | 162,404 | 1,342.2 |

**tgrep is ~35x faster**

---

## Key takeaways

- tgrep's advantage grows with repo size — the trigram index eliminates scanning files that can't match
- On the 492K-file Chromium repo, ripgrep spends ~47s per query scanning every file; tgrep averages ~1.3s
- On smaller repos (93K files), the gap narrows because OS page cache helps ripgrep's brute-force approach
- Index build is a one-time cost; the server watches for file changes and updates incrementally
