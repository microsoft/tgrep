# Benchmarks

The large-repo benchmarks measure **search time only** — the trigram index is built before timing starts.
tgrep runs in client/server mode: `tgrep serve` runs in the background, and the `tgrep` client connects via TCP.

Every query is run through a **fresh `tgrep` client process**, so each measurement includes process
startup and the TCP round trip, exactly as a shell user or editor integration would pay them.
ripgrep is capped at **120s per query**; a query that hits the cap still contributes the full 120s to
ripgrep's total, so a non-zero **Timeouts** column means the ripgrep total is a lower bound and the
real speedup is larger than shown. All runs below reported zero timeouts.

The core benchmark suite also includes Criterion microbenchmarks for index building,
query execution, and trigram extraction. These are useful for tracking lower-level
performance changes that may not show up in end-to-end search latency.

## At a glance

Speedup = ripgrep total / tgrep total across the whole query suite for that repo.
Values below 1.0 mean ripgrep won.

| Repo | Files | Queries | Windows | macOS | Linux |
| --- | ---: | ---: | ---: | ---: | ---: |
| chromium/chromium | 503,699 | 30 | **14.4x** | **19.6x** | **3.3x** |
| mozilla/gecko-dev | 387,841 | 122 | **46.4x** | **54.7x** | **9.2x** |
| torvalds/linux | 95,531 | 102 | **3.8x** | **1.5x** | *0.75x* |
| rust-lang/rust | 62,129 | 102 | **6.2x** | **2.0x** | **1.6x** |
| kubernetes/kubernetes | 31,300 | 97 | **4.8x** | **3.0x** | **1.1x** |
| golang/go | 15,818 | 103 | **6.8x** | **3.4x** | **1.3x** |

All 18 cells were measured in a single sweep on GitHub-hosted runners
(`windows-latest`, `macos-latest`, `ubuntu-latest`). Geometric mean speedup across the
six repos: **9.0x on Windows, 5.6x on macOS, 1.9x on Linux**. The one cell where ripgrep
wins is explained in [Where tgrep loses](#where-tgrep-loses).

---

## Core Criterion benchmarks

Local Criterion run on Windows from the `perf-benchmarks` branch. The short
measurement windows below are intended for PR validation; use larger sample sizes
and measurement windows for publication-quality comparisons.

> These microbenchmark numbers come from a local `cargo bench -p tgrep-core` run, not
> from the CI benchmark workflows, so they are not refreshed by the large-repo sweep
> above and are only comparable to other runs on the same machine.

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

The default build strategy, `external`, replaces the single in-heap posting
vector with an external merge sort: postings fill a fixed-size arena that spills
sorted, delta+varint encoded segments to disk, which are then k-way merged
directly into `index.bin` / `lookup.bin`. `--index-strategy=memory` selects the
older sort-everything-in-RAM path, retained as an escape hatch and as the
reference implementation for differential tests.

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
| `memory` | - | 2.20 - 3.76 GiB | 23 - 32 s |
| `external --index-buffer 256` | 8 | 430.6 MiB | ~24 s |
| `external` (64 MiB, **default**) | 31 | 151 - 160 MiB | ~23 s |
| `external --index-buffer 16` | 122 | 109.6 MiB | ~23 s |
| `external --index-buffer 4` | 487 | 102.0 MiB | ~24 s |
| `external --index-buffer 1` | 1,946 | 98.9 MiB | ~29 s |

At the default budget that is a **~17x reduction in peak memory for no time
cost** — build time is flat within run-to-run noise for any budget at or above
4 MiB, and only degrades at 1,946-segment fan-in. This is why `external` is the
default. In several runs it was measurably *faster* than `memory` (22.6 s vs
31.6 s), because sorting one ~2 GiB posting vector, and the doubling
reallocations that grow it, cost more than encoding and merging spill segments.

Two things worth noting beyond the headline. First, the `memory` figure is a
*range* because `Vec` growth doubles: peak lands wherever the final reallocation
happens to fall, and during that realloc both the old and new buffers are
resident. It varied by over a gigabyte across identical runs. The `external`
column varied by 8 MiB. Bounded memory is also *predictable* memory, which
matters more than the average when the question is whether a machine can finish
the build at all.

Second, peak decreases monotonically as the budget shrinks, which is the whole
point of the knob — but that property had to be fixed, not assumed. See below.

All configurations produce identical results. 15 literal and regex queries over
190,000+ matches returned byte-identical output against the `memory` index,
including at 1,946-segment fan-in; re-verified after `external` became the
default with 7 queries over 100,130 matches.

Peak figures here are the OS process high-water mark (`PeakWorkingSetSize` on
Windows, `VmHWM` on Linux), sampled externally. `tgrep index` now reports the
same counter itself on completion, so these numbers are reproducible without
external tooling; the self-reported and externally-sampled values agree exactly.

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

### Server bootstrap

`tgrep serve` on an empty index used to build through a different path than
`tgrep index`: it walked the repo and accumulated every posting in the live
in-memory overlay, flushing to disk once at the end. The soft memory cap that
was supposed to bound this only trips above `--memory-cap` (16 GB by default),
so on anything smaller than that the whole index simply stayed in heap.

A cold start now delegates to the same memory-bounded builder as `tgrep index`.
On the Linux kernel tree (94,181 files, same host as above, peak sampled
externally, timed to the point the server reports the index ready):

| Bootstrap path | Peak working set | Wall |
| --- | ---: | ---: |
| in-heap overlay (before) | 1,569.7 MiB | 73.6 s |
| external builder (after) | 148.6 MiB | 28.7 s |

That is a **10.6x reduction in peak memory and a 2.6x speedup**. The overlay
path was slower because it pays twice: it builds the posting map in memory and
*then* rewrites the whole thing through the append-overlay flush.

Two behavioural notes. The server no longer answers from a partially built
index during a cold start — queries see an empty index until the build
finishes, which is a deliberate trade, since results from a fraction of the
repo are misleading. And an interrupted bootstrap now leaves a usable index:
the old path wrote nothing until its single end-of-build flush, so killing it
at 99% left an empty index behind.

The watcher's ignore matcher is built from the `.gitignore` paths the build's
walk already collected, not by `gitignore::build_matcher`, which repeats the
entire walk single-threaded. That distinction is worth more than it sounds: on
a 289k-file repository the rewalk took **48.9 s**, longer than indexing the
repo. Reusing the walk's results makes it 0.07 s on the Linux tree.

Resuming a *partial* index still uses the incremental path, which can skip the
files already on disk.

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

## chromium/chromium (504K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32446984364)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32446980529)

- **Repo**: [chromium/chromium](https://github.com/chromium/chromium) (503,699 files)
- **Queries**: 30 (mix of literals, multi-word, and regex)
- **Index build time**: ~92s (Linux), ~122s (Windows), ~200s (macOS)
- **Index size**: 2,559 MB (~2.6 GB)

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 765,007 | 25,500.2 | 0 |
| tgrep (client → serve) | 53,276 | 1,775.9 | — |

**tgrep is ~14x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 1,137,042 | 37,901.4 | 0 |
| tgrep (client → serve) | 58,103 | 1,936.8 | — |

**tgrep is ~20x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 71,076 | 2,369.2 | 0 |
| tgrep (client → serve) | 21,337 | 711.2 | — |

**tgrep is ~3.3x faster**

---

## mozilla/gecko-dev (388K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32446992491)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32446988122)

- **Repo**: [mozilla/gecko-dev](https://github.com/mozilla/gecko-dev) (387,841 files)
- **Queries**: 122 (mix of C++, JavaScript, and Python patterns)
- **Index build time**: ~69s (Linux), ~72s (Windows), ~170s (macOS)
- **Index size**: ~1,930 MB (~1.9 GB)

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 1,936,726 | 15,874.8 | 0 |
| tgrep (client → serve) | 41,786 | 342.5 | — |

**tgrep is ~46x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 3,839,120 | 31,468.2 | 0 |
| tgrep (client → serve) | 70,243 | 575.8 | — |

**tgrep is ~55x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 214,800 | 1,760.7 | 0 |
| tgrep (client → serve) | 23,399 | 191.8 | — |

**tgrep is ~9x faster**

---

## torvalds/linux (96K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32446953640)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32446950062)

- **Repo**: [torvalds/linux](https://github.com/torvalds/linux) (95,531 files)
- **Queries**: 102 (mix of literals, multi-word, and regex)
- **Index build time**: ~38s (Linux), ~47s (Windows), ~49s (macOS)
- **Index size**: ~995 MB

This is tgrep's **worst case**, and the one repo where it can lose. See
[Where tgrep loses](#where-tgrep-loses) below.

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 411,685 | 4,036.1 | 0 |
| tgrep (client → serve) | 107,527 | 1,054.2 | — |

**tgrep is ~3.8x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 220,653 | 2,163.3 | 0 |
| tgrep (client → serve) | 147,626 | 1,447.3 | — |

**tgrep is ~1.5x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 45,420 | 445.3 | 0 |
| tgrep (client → serve) | 60,213 | 590.3 | — |

**ripgrep is ~1.3x faster** — the only measured case where tgrep loses.

---

## rust-lang/rust (62K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32446969257)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32446965511)

- **Repo**: [rust-lang/rust](https://github.com/rust-lang/rust) (62,129 files)
- **Queries**: 102 (mix of Rust patterns, macros, traits, and regex)
- **Index build time**: ~7s (Linux), ~10s (Windows), ~12s (macOS)
- **Index size**: ~199 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 156,614 | 1,535.4 | 0 |
| tgrep (client → serve) | 25,186 | 246.9 | — |

**tgrep is ~6.2x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 60,986 | 597.9 | 0 |
| tgrep (client → serve) | 31,024 | 304.2 | — |

**tgrep is ~2x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 18,945 | 185.7 | 0 |
| tgrep (client → serve) | 11,768 | 115.4 | — |

**tgrep is ~1.6x faster**

---

## kubernetes/kubernetes (31K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32446976688)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32446973175)

- **Repo**: [kubernetes/kubernetes](https://github.com/kubernetes/kubernetes) (31,300 files)
- **Queries**: 97 (mix of Go patterns, Kubernetes API types, and regex)
- **Index build time**: ~8s (Linux), ~10s (Windows), ~6s (macOS)
- **Index size**: ~215 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 104,187 | 1,074.1 | 0 |
| tgrep (client → serve) | 21,639 | 223.1 | — |

**tgrep is ~4.8x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 24,511 | 252.7 | 0 |
| tgrep (client → serve) | 8,311 | 85.7 | — |

**tgrep is ~3x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 13,856 | 142.8 | 0 |
| tgrep (client → serve) | 12,594 | 129.8 | — |

**tgrep is ~1.1x faster** (essentially comparable)

---

## golang/go (16K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32446961264)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32446957063)

- **Repo**: [golang/go](https://github.com/golang/go) (15,818 files)
- **Queries**: 103 (mix of Go stdlib patterns, testing, and regex)
- **Index build time**: ~4s (Linux), ~5s (Windows), ~3s (macOS)
- **Index size**: ~110 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 66,868 | 649.2 | 0 |
| tgrep (client → serve) | 9,798 | 95.1 | — |

**tgrep is ~6.8x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 11,524 | 111.9 | 0 |
| tgrep (client → serve) | 3,432 | 33.3 | — |

**tgrep is ~3.4x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 6,895 | 66.9 | 0 |
| tgrep (client → serve) | 5,275 | 51.2 | — |

**tgrep is ~1.3x faster**

---

## Key takeaways

- **Repo size is the strongest predictor.** The trigram index eliminates scanning files
  that can't match, so the advantage grows with the corpus. On the two largest repos
  (Chromium 504K files, gecko-dev 388K files) tgrep wins on every platform, by 3.3–55x.
  On the smallest (Go, 16K files) the Linux margin narrows to 1.3x.
- **Windows benefits most consistently** — geometric mean **9.0x**, and never below
  3.8x in any cell. Windows per-file open/read overhead is high, so skipping 90%+ of
  the files pays off everywhere.
- **macOS has the highest peak but more spread** — geometric mean **5.6x**, range
  1.5–55x.
- **Linux is the weakest case** — geometric mean **1.9x**, range 0.75–9.2x. Linux's
  page cache plus ripgrep's parallel scan make brute force genuinely cheap on a warm
  repo, so the index buys less. This is also the only platform with a losing cell.
- ripgrep never hit the 120s per-query cap in this run (Timeouts = 0 in every table),
  so every ratio here is a true measured value, not a censored one.
- Index build is a one-time cost — ~4s for Go, ~200s for Chromium on macOS — and the
  server then watches for file changes and updates incrementally.

### Where tgrep loses

On **torvalds/linux running on Linux x86_64, ripgrep is ~1.3x faster than tgrep**
(45.4s vs 60.2s over 102 queries). This is the only losing cell in the matrix, and it
reproduces on `main`, so it is a property of the workload rather than a regression.

The cause is not bad index pruning — measured directly on a local kernel checkout, the
index narrows to **4–12% of the 95K files** for the queries in this suite. The cause is
**match volume**. The kernel query set contains very high-frequency symbols
(`printk`, `EXPORT_SYMBOL`, `kmalloc`) that produce 24K–51K matches each. tgrep's cost
per *delivered match* — serialize, IPC, deserialize, print — is higher than ripgrep's
cost per match, because ripgrep writes straight to stdout from the scanning thread. Once
a query returns tens of thousands of matches, that per-match cost outweighs everything
the index saved on file selection.

The same effect is visible from the other side: on Windows, with the same repo and the
same index, low-match kernel queries run **58–202x faster** than ripgrep, and the ratio
falls as the match count rises.

Practical guidance: tgrep is a strong replacement for ripgrep on large repos on Windows
and macOS unconditionally. On Linux, it is a clear win on very large repos and on
selective queries, but a repo-wide search for an extremely common token can be slower
than ripgrep.
