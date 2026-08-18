# Benchmarks

The large-repo benchmarks measure **search time only** — the trigram index is built before timing starts.
tgrep runs in client/server mode: `tgrep serve` runs in the background, and the `tgrep` client connects via TCP.

The core benchmark suite also includes Criterion microbenchmarks for index building,
query execution, and trigram extraction. These are useful for tracking lower-level
performance changes that may not show up in end-to-end search latency.

---

## Core Criterion benchmarks

Local Criterion run on Windows from the `perf-benchmarks` branch. The short
measurement windows below are intended for PR validation; use larger sample sizes
and measurement windows for publication-quality comparisons.

```powershell
cargo bench -p tgrep-core --bench trigram_extraction -- --sample-size 30 --warm-up-time 1 --measurement-time 2
cargo bench -p tgrep-core --bench query_execution -- --sample-size 30 --warm-up-time 1 --measurement-time 2
cargo bench -p tgrep-core --bench index_build -- --sample-size 10 --warm-up-time 0.5 --measurement-time 1
cargo bench -p tgrep-core --bench index_build -- --peak-memory 5000
cargo bench -p tgrep-core --bench index_build -- --peak-memory-high-diversity 1000
```

### Index build

| Case | Mean | Throughput | Peak working set |
| --- | ---: | ---: | ---: |
| 100 files | 29.611ms | 1.6490 MiB/s | - |
| 500 files | 100.77ms | 2.4228 MiB/s | - |
| 2,000 files | 322.78ms | 3.0255 MiB/s | - |
| 5,000 files | 776.27ms | 3.1450 MiB/s | 16.47 MiB |
| 1,000 high-diversity files | 372.62ms | 2.6208 MiB/s | 43.68 MiB |

The high-diversity case stresses the number of distinct trigrams and posting-list
serialization. The flat sorted-posting writer reduced this case from roughly
1.47s and 98.74 MiB peak working set to roughly 0.37s and 43.68 MiB in local runs.

### Index build strategies

`tgrep index --index-strategy=external` replaces the single in-heap posting
vector with an external merge sort: postings fill a fixed-size arena that spills
sorted, delta+varint encoded segments to disk, which are then k-way merged
directly into `index.bin` / `lookup.bin`.

The peak-memory probe runs both strategies back to back on the same fixture.
Each of these runs uses a deliberately small 4 MB arena so bench-scale fixtures
spill; the shipped default is 64 MB.

```powershell
cargo bench -p tgrep-core --bench index_build -- --peak-memory-high-diversity 16000
```

**Peak working set** (high-diversity fixture, Windows, local run):

| Files | `memory` | `external` | Reduction |
| ---: | ---: | ---: | ---: |
| 2,000 | 66.93 MiB | 32.85 MiB | −51% |
| 4,000 | 88.98 MiB | 34.29 MiB | −61% |
| 8,000 | 185.21 MiB | 37.92 MiB | −80% |
| 16,000 | 342.98 MiB | 50.30 MiB | −85% |

The `memory` column grows roughly linearly with file count while `external`
stays close to flat (+17 MiB across an 8x increase in files), which is the
point: peak heap becomes a function of the arena budget rather than of repo
size. The remaining growth in the `external` column is the file table and walk
results, not postings.

**Real-world scale: the Linux kernel.** 94,634 indexed files / 446,892 trigrams
/ 990 MiB index (`C:\repos\linux`, v7.2-rc7, warm page cache, peak working set
sampled from the child process):

| Strategy | Spill segments | Peak working set | Build |
| --- | ---: | ---: | ---: |
| `memory` | - | 2,803 - 3,855 MiB | ~23 s |
| `external --index-buffer 256` | 8 | 430.6 MiB | ~24 s |
| `external` (64 MiB default) | 31 | 151 - 159 MiB | ~23 s |
| `external --index-buffer 16` | 122 | 109.6 MiB | ~23 s |
| `external --index-buffer 4` | 487 | 102.0 MiB | ~24 s |
| `external --index-buffer 1` | 1,946 | 98.9 MiB | ~29 s |

At the default budget that is a **20x reduction in peak memory for no
measurable time cost** — build time is flat within run-to-run noise for any
budget at or above 4 MiB, and only degrades at 1,946-segment fan-in.

Two things worth noting beyond the headline. First, the `memory` figure is a
*range* because `Vec` growth doubles: peak lands wherever the final reallocation
happens to fall, and during that realloc both the old and new buffers are
resident. It varied by over a gigabyte across three identical runs. The
`external` column varied by 8 MiB. Bounded memory is also *predictable* memory,
which matters more than the average when the question is whether a machine can
finish the build at all.

Second, peak decreases monotonically as the budget shrinks, which is the whole
point of the knob — but that property had to be fixed, not assumed. See below.

All configurations produce identical results: 15 literal and regex queries over
190,000+ matches returned byte-identical output against the `memory` index,
including at 1,946-segment fan-in.

**Why the merge shares one read budget.** The first implementation gave every
open segment a fixed 256 KiB read-ahead buffer, which made merge memory
`segments * 256 KiB` — unbounded in fan-in. Since a smaller arena spills *more*
segments, asking for less memory delivered more of it, and peak traced a U:

| Arena | Segments | Peak, fixed buffers | Peak, shared budget |
| ---: | ---: | ---: | ---: |
| 16 MiB | 122 | 114.7 MiB | 109.6 MiB |
| 4 MiB | 487 | 195.8 MiB | 102.0 MiB |
| 1 MiB | 1,946 | 565.8 MiB | 98.9 MiB |

At a 1 MiB budget the read buffers alone were 486 MiB. Sizing them as
`budget / segments` (floored at 4 KiB) removes the U entirely. This only
reproduces at real scale — every synthetic fixture in this file spills too few
segments to reach the crossover.

**Smaller fixtures.** 2,500 files / 37.5 MiB built from tgrep's own `.rs`
sources, timed end to end through the release binary (median of 3 runs):

| Strategy | Spill segments | Peak working set | Median build | Delta |
| --- | ---: | ---: | ---: | ---: |
| `memory` | - | 144.08 MiB | 0.725 s | - |
| `external --index-buffer 64` | 2 | 118.75 MiB | 0.791 s | +9.1% |
| `external --index-buffer 8` | 9 | 58.38 MiB | 0.789 s | +8.8% |

All three produce the same 14,379 trigrams over 2,500 files. Note that shrinking
the arena 8x costs nothing in time — going from 2 segments to 9 is free, because
merge fan-in is not the bottleneck at this scale, the spill encode/decode
round-trip is. The arena budget is therefore a fairly clean dial on peak memory.

Criterion microbenchmarks bracket that number from both sides:

| Case | `memory` | `external` | Delta | Spills? |
| --- | ---: | ---: | ---: | --- |
| 5,000 files | 1.2819 s | 1.2917 s | +0.8% | no |
| 1,000 high-diversity files | 464.42ms | 540.11ms | +16% | yes |

The 5,000-file case is the *no-spill* floor: its fixture repeats one line, so the
whole repo is only 67 distinct trigrams and the arena never fills, which is why
the overhead is indistinguishable from noise. The high-diversity case is the
worst-case ceiling: random-byte content makes nearly every trigram unique, so
segment groups hold a single posting each and per-group header overhead and
merge fan-in work are both maximal. Real code sits between them, at roughly +9%.

Total sort work is unchanged — `k` sorts of `n/k` elements plus an `n·log k`
merge is still `O(n·log n)` — so the delta is purely the spill write plus the
merge read.

### Query execution

| Case | Mean |
| --- | ---: |
| AND intersection, 100 files | 816.79ns |
| AND intersection, 1,000 files | 3.7150us |
| AND intersection, 10,000 files | 29.260us |
| OR union, 4 terms | 6.4297us |
| OR union, 16 terms | 29.912us |
| OR union, 64 terms | 122.23us |
| On-disk common literal, 1,000 files | 63.806us |
| On-disk common literal, 5,000 files | 475.88us |

The on-disk common-literal cases exercise a real built index through
`IndexReader::lookup_trigram_with_masks`. Keeping on-disk posting lists sorted by
file ID lets query execution skip redundant sorting and deduplication for these
already-normalized posting lists.

### Trigram extraction

| Case | 1 KiB | 16 KiB | 256 KiB |
| --- | ---: | ---: | ---: |
| Extract masks, lowercase ASCII | 14.253us | 206.39us | 2.9378ms |
| Extract merged masks, lowercase ASCII | 30.271us | 399.31us | 6.0330ms |
| Extract merged masks, mixed case | 29.960us | 374.22us | 6.1909ms |

For lowercase-only content, merged-mask extraction skips the lowercase copy and
second extraction pass. In the 256 KiB case, that improved the local Criterion
baseline by about 51%.

---

## chromium/chromium (496K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/25272408453)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/25272407721)

- **Repo**: [chromium/chromium](https://github.com/chromium/chromium) (495,548 files)
- **Queries**: 30 (mix of literals, multi-word, and regex)
- **Index build time**: ~92s (Linux), ~156s (Windows), ~314s (macOS)
- **Index size**: 2,486 MB (~2.5 GB)

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 886,720 | 29,557.3 |
| tgrep (client → serve) | 74,743 | 2,491.4 |

**tgrep is ~12x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 1,833,313 | 61,110.4 |
| tgrep (client → serve) | 78,889 | 2,629.6 |

**tgrep is ~23x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 70,868 | 2,362.3 |
| tgrep (client → serve) | 29,505 | 983.5 |

**tgrep is ~2.4x faster**

---

## mozilla/gecko-dev (388K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/25272406934)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/25272406169)

- **Repo**: [mozilla/gecko-dev](https://github.com/mozilla/gecko-dev) (387,841 files)
- **Queries**: 122 (mix of C++, JavaScript, and Python patterns)
- **Index build time**: ~67s (Linux), ~81s (Windows), ~166s (macOS)
- **Index size**: 1,938 MB (~1.9 GB)

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 1,976,224 | 16,198.6 |
| tgrep (client → serve) | 37,842 | 310.2 |

**tgrep is ~52x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 4,320,389 | 35,413.0 |
| tgrep (client → serve) | 60,064 | 492.3 |

**tgrep is ~72x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 235,601 | 1,931.2 |
| tgrep (client → serve) | 20,675 | 169.5 |

**tgrep is ~11x faster**

---

## torvalds/linux (94K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/25272401185)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/25272400497)

- **Repo**: [torvalds/linux](https://github.com/torvalds/linux) (93,693 files)
- **Queries**: 102 (mix of literals, multi-word, and regex)
- **Index build time**: ~35s (Linux), ~44s (Windows), ~42s (macOS)
- **Index size**: ~977 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 440,376 | 4,317.4 |
| tgrep (client → serve) | 95,250 | 933.8 |

**tgrep is ~5x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 216,606 | 2,123.6 |
| tgrep (client → serve) | 93,089 | 912.6 |

**tgrep is ~2.3x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 46,393 | 454.8 |
| tgrep (client → serve) | 48,607 | 476.5 |

**~1x (comparable — Linux I/O cache narrows the gap on this repo)**

---

## rust-lang/rust (59K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/25272402496)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/25272401869)

- **Repo**: [rust-lang/rust](https://github.com/rust-lang/rust) (59,419 files)
- **Queries**: 102 (mix of Rust patterns, macros, traits, and regex)
- **Index build time**: ~6s (Linux), ~9s (Windows), ~12s (macOS)
- **Index size**: ~190 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 202,842 | 1,988.6 |
| tgrep (client → serve) | 21,943 | 215.1 |

**tgrep is ~9x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 48,854 | 479.0 |
| tgrep (client → serve) | 14,707 | 144.2 |

**tgrep is ~3x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 19,172 | 188.0 |
| tgrep (client → serve) | 9,908 | 97.1 |

**tgrep is ~1.9x faster**

---

## kubernetes/kubernetes (30K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/25272405424)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/25272404506)

- **Repo**: [kubernetes/kubernetes](https://github.com/kubernetes/kubernetes) (29,953 files)
- **Queries**: 97 (mix of Go patterns, Kubernetes API types, and regex)
- **Index build time**: ~7s (Linux), ~10s (Windows), ~8s (macOS)
- **Index size**: ~205 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 144,450 | 1,489.2 |
| tgrep (client → serve) | 17,288 | 178.2 |

**tgrep is ~8x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 28,813 | 297.0 |
| tgrep (client → serve) | 8,524 | 87.9 |

**tgrep is ~3x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 15,839 | 163.3 |
| tgrep (client → serve) | 9,359 | 96.5 |

**tgrep is ~1.7x faster**

---

## golang/go (15K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/25272403694)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/25272403111)

- **Repo**: [golang/go](https://github.com/golang/go) (15,343 files)
- **Queries**: 103 (mix of Go stdlib patterns, testing, and regex)
- **Index build time**: ~3s (Linux), ~3s (macOS), ~4s (Windows)
- **Index size**: ~106 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 46,395 | 450.4 |
| tgrep (client → serve) | 7,230 | 70.2 |

**tgrep is ~6x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 16,604 | 161.2 |
| tgrep (client → serve) | 4,067 | 39.5 |

**tgrep is ~4x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) |
| --- | ---: | ---: |
| ripgrep | 7,216 | 70.1 |
| tgrep (client → serve) | 4,173 | 40.5 |

**tgrep is ~1.7x faster**

---

## Key takeaways

- tgrep's advantage grows with repo size — the trigram index eliminates scanning files that can't match
- On the 496K-file Chromium repo, tgrep is up to **23x faster** (macOS) and **12x faster** (Windows)
- On the 388K-file gecko-dev repo, tgrep is up to **72x faster** (macOS), **52x faster** (Windows), and **11x faster** (Linux)
- On the 94K-file Linux kernel, tgrep is **5x faster** on Windows and **2.3x faster** on macOS; on Linux the two are comparable
- On the 59K-file Rust repo, tgrep is **9x faster** on Windows, ~3x on macOS, ~2x on Linux
- On the 30K-file Kubernetes repo, tgrep is **8x faster** on Windows, ~3x on macOS, ~2x on Linux
- On the 15K-file Go repo, tgrep is **6x faster** on Windows, ~4x on macOS, ~2x on Linux
- On Windows, tgrep consistently shows the largest speedups (5–52x) due to slower Windows I/O
- On Linux, aggressive OS page caching often narrows the gap; tgrep is usually faster, but some repos (such as the Linux kernel on x86_64) can be comparable
- Index build is a one-time cost; the server watches for file changes and updates incrementally
