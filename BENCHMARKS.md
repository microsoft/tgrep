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
| chromium/chromium | 503,903 | 30 | **13.5x** | **10.8x** | **2.36x** |
| mozilla/gecko-dev | 387,841 | 122 | **34.8x** | **42.9x** | **7.5x** |
| torvalds/linux | 95,776 | 102 | **25.1x** | **22.4x** | **5.5x** |
| rust-lang/rust | 62,179 | 102 | **8.1x** | **1.78x** | **1.36x** |
| kubernetes/kubernetes | 31,300 | 97 | **5.9x** | **1.72x** | **1.00x** |
| golang/go | 15,826 | 103 | **6.2x** | **3.2x** | **1.34x** |

All 18 cells are from the complete 22 August 2026 sweep at `90a41a6`, measured on
GitHub-hosted runners (`windows-latest`, `macos-latest`, `ubuntu-latest`). Every
headline latency, ratio, file count, and primary run link in the repo sections
below comes from that same sweep rather than mixing the latest totals with minima
from older runs. Older runs are cited only to show the kernel suite's variance.
Geometric mean speedup across the six repos is **12.3x on Windows, 6.8x on macOS,
and 2.4x on Linux**. Kubernetes on Linux is effectively tied (1.004x); the largest
margin is Gecko on macOS at 42.9x. See
[What decides the margin](#what-decides-the-margin).

Shared-runner results are not controlled-machine measurements. Compare tgrep with
ripgrep **within a row**, not absolute milliseconds between workflow runs: CPU,
storage and page-cache variance move both tools, particularly on macOS.

### Index-build peak memory in the latest sweep

The benchmark logs also record the indexer's OS high-water mark. Each cell shows
the previously published sweep → the latest sweep, in MiB:

| Repo | Windows | macOS | Linux |
| --- | ---: | ---: | ---: |
| chromium/chromium | 359.7 → **362.4** | 437.3 → **440.2** | 337.6 → **339.7** |
| mozilla/gecko-dev | 304.8 → **304.5** | 396.2 → **359.7** | 271.7 → **273.4** |
| torvalds/linux | 142.6 → **173.8** | 175.6 → **195.8** | 141.6 → **192.3** |
| rust-lang/rust | 110.5 → **113.1** | 151.3 → **148.8** | 103.3 → **111.9** |
| kubernetes/kubernetes | 113.3 → **115.5** | 150.8 → **148.5** | 112.6 → **120.2** |
| golang/go | 106.7 → **105.4** | 123.0 → **126.5** | 115.1 → **114.4** |

The kernel moved most because the latest build admits the 110 files that the old
1 MiB default cap skipped; Chromium changed by only 2–3 MiB despite admitting
more formerly capped files. These are peak working-set/RSS figures, so mapped
file pages count even though the OS can reclaim them. They are not private bytes
or heap usage.

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
sampled from the child process, under the 1 MiB indexing cap that was the
default at the time):

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
Since large files became memory-mapped, resident set also counts mapped file
pages, so it overstates what the process holds — where that matters below,
private bytes are reported alongside it.

**What a file-size cap costs.** The table above holds the arena fixed and varies
the budget. Raising the *indexing* cap from 1 MiB moved the floor under all of
it, because the builder read each file whole and in parallel, so several 20 MB
generated headers were resident at once. Same host and repo, default arena:

| `--max-filesize` | Files indexed | Skipped as too large | Peak working set | Build |
| ---: | ---: | ---: | ---: | ---: |
| 1 MiB (oldest default) | 94,637 | 110 | 151.2 MiB | 25.5 s |
| 8 MiB | 94,736 | 11 | 304.1 MiB | 24.4 s |
| 16 MiB | 94,745 | 2 | 341.2 MiB | 24.5 s |
| 64 MiB | 94,747 | 0 | 338.8 MiB | 25.5 s |

The cost is a step, not a slope: almost all of it is paid on the first megabyte
past the old cap, and 16 MiB and 64 MiB are within noise of each other. So a cap
between the two gives up files without buying memory back — 8 MiB still drops 11
kernel files for 89% of the peak. Build time is flat throughout, and the index
grew 990.4 MB to 996.7 MB (+0.6%) for 110 more files totalling 437.9 MB, because
the files a size cap catches are generated and repetitive: `dcn_3_2_0_sh_mask.h`
is 24 MB and contributes 7,263 distinct trigrams, fewer than the 10,936 of the
224 KB `fs/ext4/super.c`.

**Bounding what the cap admits.** Those peaks are what the cap cost *before* the
builder was taught to bound it. Two changes: trigram extraction no longer
materialises a lowercased copy of each file, and batches are bounded by
cumulative bytes rather than file count, so the raw bytes in flight are capped
however large the files are. Same host and repo, minimum of three runs:

| | Peak working set | Build |
| --- | ---: | ---: |
| Before | 291.5 MiB | 36.2 s |
| After | 198.1 MiB (−32%) | 37.8 s (+4%) |

The variance matters as much as the mean. Across those runs the old path ranged
291.5–400.5 MiB while the new one ranged 198.1–203.5 MiB, so a 109 MiB spread
became a 5 MiB one: bounding the bytes in flight makes peak memory repeatable,
not just smaller. The budget is deliberately a flat 64 MiB rather than something
that scales with the thread pool — a larger, pool-scaled budget was measured and
was both slower *and* hungrier here (38.4 s, 254.0 MiB), because the kernel's
large files are a rare minority and the extra headroom bought no parallelism.

**Removing the cap.** Bounding the bytes in flight made the peak predictable but
left it proportional to the largest files, which is why a cap still looked
necessary. Mapping files past 1 MiB instead of reading them removes that link,
and with it the reason for the cap. ripgrep has no default `--max-filesize`
either, and this is the mechanism that lets it not need one.

From here the two metrics diverge and have to be reported separately. Resident
set counts mapped file pages, so it barely moves; *private* bytes — the memory
the process actually holds and cannot hand back — is what the change targets.
Interleaved A/B runs on the kernel, alternating binaries to keep the page cache
from favouring whichever ran second:

| | Private bytes | Resident set | Build |
| --- | ---: | ---: | ---: |
| 64 MiB cap, heap reads | 197.2 / 199.5 MiB | 202.8 / 205.1 MiB | 41.1 / 42.2 s |
| No cap, mapped over 1 MiB | **152.4 / 152.4 MiB (−23%)** | 192.2 / 195.7 MiB | **27.0 / 27.0 s (−36%)** |

Both produce the same index (447,080 trigrams over 94,744 files), because the
kernel has nothing above 64 MiB — the time and memory come from the mapping, not
from indexing more. The effect is far larger where the files are: on a single
192 MB file, private bytes are 67.1 MiB, and searching one fell from 291.3 MiB
to 58.2 MiB.

The threshold is 1 MiB because that is where the measurement pointed. In the AMD
register headers, the worst case in the tree, an 8 MiB threshold mapped only 11
of 488 files and left 69% of the bytes on the heap, while 1 MiB maps 102 files
covering 87%. It stays cheap on ordinary trees because only 110 of the kernel's
94,747 files reach it at all.

**What removing the cap still costs.** A file that is neither valid UTF-8 nor
detectably binary cannot be mapped: the index must hold the same `U+FFFD`-repaired
bytes a search will match against, so it is decoded onto the heap. Two fixes keep
that proportional rather than multiplied — the repaired buffer is now sized
exactly instead of doubling out of a `bytes.len()` guess, and indexing skips the
per-repair offset table it never reads. On a 135 MB Latin-1 file that took
private bytes from 504.2 MiB (3.7x the file) to 204.6 MiB (1.5x).

**Indexing large files at speed.** Admitting large files exposed a throughput
problem the cap had been hiding. Two causes, both only visible once a tree is
mostly large files:

*The byte budget charged mapped bytes it never allocated.* A mapped file costs no
heap, but it was charged its full length against the 64 MiB budget, so a batch of
32 MiB files held two — and a batch is a barrier, so a 16-core pool ran two files
at a time. Mapped files are now charged a saturating stand-in for the one thing
they *do* put on the heap, their extracted trigram map, against a second budget
that caps mapped bytes in flight at 256 MiB.

*Extraction hashed every byte twice with SipHash.* A trigram key is its own
24-bit hash, so the collision resistance was paying for nothing, and the
lowercase pass re-walked the whole file even though most windows in real source
lower to the same key. Extraction now uses a multiply-xorshift hasher and folds
both cases into one pass, doing real work only for windows that actually contain
an uppercase byte.

Interleaved A/B runs, 512 MiB per corpus, minimum of alternating runs:

| Corpus | Before | After | |
| --- | ---: | ---: | ---: |
| 16 x 32 MiB, mixed case | 24.1 s | **1.7 s** | 14.2x |
| 16 x 32 MiB, lowercase | 9.4 s | **1.0 s** | 9.4x |
| 512 x 1 MiB, lowercase | 2.7 s | **1.9 s** | 1.4x |
| 20,000 x 8 KiB, mixed case | 12.5 s | 12.6 s | — |

The last row is the point: ordinary source trees are bound by file I/O and the
external sort, not by extraction, so they neither gain nor lose. The gain is
concentrated exactly where the old cap used to hide the cost.

This is what the reported case was made of. A `tgrep serve` opening an index
built under the old cap sees every formerly-oversized file as new and absorbs
them in one stale merge; on a 40,300-file index absorbing 16 x 32 MiB of new
mixed-case files, that merge went from 26.3 s to 3.4 s (7.7x), producing the same
79,078 trigrams over 40,316 files.

Memory moves the way mapping implies. On the mixed-case corpus, private bytes
went 71.4 MiB to 78.1 MiB while resident set went 59.3 MiB to 228.1 MiB: the
increase is entirely file-backed pages the kernel can reclaim, held by the
256 MiB mapped budget, and the heap the process actually owns barely moved.


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

Interleaved A/B on one machine, before and after replacing SipHash with a
multiply-xorshift hasher over the 24-bit trigram key and fusing the case-folding
pass into the main window loop:

| Case | 1 KiB | 16 KiB | 256 KiB |
| --- | ---: | ---: | ---: |
| Extract masks, lowercase ASCII | 18.860us → **6.7010us** | 240.73us → **73.384us** | 3.7818ms → **1.1429ms** |
| Extract merged masks, lowercase ASCII | 37.896us → **9.2151us** | 530.06us → **124.74us** | 8.4003ms → **2.0132ms** |
| Extract merged masks, mixed case | 38.483us → **9.9965us** | 536.00us → **139.44us** | 8.4413ms → **2.2813ms** |

A trigram packs three bytes into 24 bits, so the key is already collision-free
and SipHash's collision resistance bought nothing while running once per input
byte. Merged extraction previously walked the file a second time whenever it held
any uppercase byte; folding that into one pass means a window costs extra work
only if it actually lowers to a different trigram, which closes most of the gap
between the mixed-case and lowercase rows.

---

## chromium/chromium (504K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32580646448)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32580644568)

- **Repo**: [chromium/chromium](https://github.com/chromium/chromium) (503,903 files)
- **Queries**: 30 (mix of literals, multi-word, and regex)
- **Index build time**: ~82s (Linux), ~142s (Windows), ~247s (macOS)
- **Index size**: 2,581 MB (~2.6 GB)

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 769,549 | 25,651.6 | 0 |
| tgrep (client → serve) | 57,187 | 1,906.2 | — |

**tgrep is ~13.5x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 1,046,616 | 34,887.2 | 0 |
| tgrep (client → serve) | 97,204 | 3,240.1 | — |

**tgrep is ~10.8x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 46,294 | 1,543.1 | 0 |
| tgrep (client → serve) | 19,646 | 654.9 | — |

**tgrep is ~2.36x faster**

---

## mozilla/gecko-dev (388K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32580650472)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32580648298)

- **Repo**: [mozilla/gecko-dev](https://github.com/mozilla/gecko-dev) (387,841 files)
- **Queries**: 122 (mix of C++, JavaScript, and Python patterns)
- **Index build time**: ~73s (Linux), ~101s (Windows), ~201s (macOS)
- **Index size**: ~1,953 MB (~2.0 GB)

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 2,196,618 | 18,005.1 | 0 |
| tgrep (client → serve) | 63,088 | 517.1 | — |

**tgrep is ~34.8x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 4,183,208 | 34,288.6 | 0 |
| tgrep (client → serve) | 97,407 | 798.4 | — |

**tgrep is ~42.9x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 215,729 | 1,768.3 | 0 |
| tgrep (client → serve) | 28,639 | 234.7 | — |

**tgrep is ~7.5x faster**

---

## torvalds/linux (96K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32580642696)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32580641264)

- **Repo**: [torvalds/linux](https://github.com/torvalds/linux) (95,776 files)
- **Queries**: 102 (mix of literals, multi-word, and regex)
- **Index build time**: ~42s (Linux), ~53s (Windows), ~37s (macOS)
- **Index size**: ~1,005 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 333,512 | 3,269.7 | 0 |
| tgrep (client → serve) | 13,283 | 130.2 | — |

**tgrep is ~25.1x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 384,507 | 3,769.7 | 0 |
| tgrep (client → serve) | 17,142 | 168.1 | — |

**tgrep is ~22.4x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 44,343 | 434.7 | 0 |
| tgrep (client → serve) | 8,085 | 79.3 | — |

**tgrep is ~5.5x faster**

This suite has now been measured four times on separate runner sessions. Linux
came out at 5.2x, 5.5x, 5.7x and 6.2x; macOS at 22.4x, 24.7x, 27.3x and 31.7x
([32481331031](https://github.com/microsoft/tgrep/actions/runs/32481331031),
[32456274534](https://github.com/microsoft/tgrep/actions/runs/32456274534),
[32457317972](https://github.com/microsoft/tgrep/actions/runs/32457317972)).
The tables now report the linked latest run consistently; the spread is retained
to show why a shared-runner ratio should not be read as a controlled benchmark.

---

## rust-lang/rust (62K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32580663644)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32580661455)

- **Repo**: [rust-lang/rust](https://github.com/rust-lang/rust) (62,179 files)
- **Queries**: 102 (mix of Rust patterns, macros, traits, and regex)
- **Index build time**: ~8s (Linux), ~11s (Windows), ~12s (macOS)
- **Index size**: ~199.4 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 203,510 | 1,995.2 | 0 |
| tgrep (client → serve) | 25,145 | 246.5 | — |

**tgrep is ~8.1x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 62,400 | 611.8 | 0 |
| tgrep (client → serve) | 34,982 | 343.0 | — |

**tgrep is ~1.78x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 18,607 | 182.4 | 0 |
| tgrep (client → serve) | 13,730 | 134.6 | — |

**tgrep is ~1.36x faster**

---

## kubernetes/kubernetes (31K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32580659311)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32580657354)

- **Repo**: [kubernetes/kubernetes](https://github.com/kubernetes/kubernetes) (31,300 files)
- **Queries**: 97 (mix of Go patterns, Kubernetes API types, and regex)
- **Index build time**: ~6s (Linux), ~11s (Windows), ~7s (macOS)
- **Index size**: ~215.8 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 135,099 | 1,392.8 | 0 |
| tgrep (client → serve) | 22,925 | 236.3 | — |

**tgrep is ~5.9x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 25,634 | 264.3 | 0 |
| tgrep (client → serve) | 14,898 | 153.6 | — |

**tgrep is ~1.72x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 11,062 | 114.0 | 0 |
| tgrep (client → serve) | 11,018 | 113.6 | — |

**tgrep and ripgrep are effectively tied (1.004x)**

---

## golang/go (16K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32580655162)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32580653074)

- **Repo**: [golang/go](https://github.com/golang/go) (15,826 files)
- **Queries**: 103 (mix of Go stdlib patterns, testing, and regex)
- **Index build time**: ~4s (Linux), ~5s (Windows), ~3s (macOS)
- **Index size**: ~113.7 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 61,702 | 599.0 | 0 |
| tgrep (client → serve) | 10,020 | 97.3 | — |

**tgrep is ~6.2x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 14,939 | 145.0 | 0 |
| tgrep (client → serve) | 4,641 | 45.1 | — |

**tgrep is ~3.2x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 6,706 | 65.1 | 0 |
| tgrep (client → serve) | 5,019 | 48.7 | — |

**tgrep is ~1.34x faster**

---

## Key takeaways

- **Repo size is the strongest predictor.** The trigram index eliminates scanning files
  that can't match, so the advantage grows with the corpus. On the two largest repos
  (Chromium 504K files, gecko-dev 388K files) tgrep wins on every platform, by 2.36–42.9x.
  On the smallest (Go, 16K files) the Linux margin narrows to 1.34x.
- **Windows benefits most consistently** — geometric mean **12.3x**, and never below
  5.9x in any cell. Windows per-file open/read overhead is high, so skipping 90%+ of
  the files pays off everywhere.
- **macOS has the highest peak but more spread** — geometric mean **6.8x**, range
  1.72–42.9x.
- **Linux is the weakest case** — geometric mean **2.4x**, range 1.004–7.5x. Linux's
  page cache plus ripgrep's parallel scan make brute force genuinely cheap on a warm
  repo, so the index buys less.
- ripgrep never hit the 120s per-query cap in any run (Timeouts = 0 in every table),
  so every ratio here is a true measured value, not a censored one.
- Index build is a one-time cost — ~3s for Go on macOS, ~247s for Chromium there — and the
  server then watches for file changes and updates incrementally.

### What decides the margin

tgrep does not lose a cell in the latest matrix, but Kubernetes/Linux is effectively
parity and the measured margin ranges from 1.004x to 42.9x. Two things move it.

**Repo size**, as above: more files means more files the index can skip.

**Match volume**, which is easy to miss. tgrep's cost per *delivered* match —
serialize, ship over IPC, deserialize, print — is higher than ripgrep's, because
ripgrep writes straight to stdout from the scanning thread. A query returning tens
of thousands of matches can spend more on delivery than the index ever saved on
file selection.

The kernel suite used to demonstrate this the hard way. Its queries were generic
tokens — `read`, `write`, `^#define\s+[A-Z_]+` — that matched most of the tree:
5,398,512 matches across 102 queries, with a single query returning 2,089,941.
Kernel Linux is the only cell tgrep has ever lost, and on that query set it lost
every run: 0.81x, 0.74x and 0.95x, the closest being 48.6s against ripgrep's
46.4s. Index pruning was not the problem; measured directly on a local kernel
checkout, the index still narrowed to **4–12% of the 95K files**.

The suite now uses queries a kernel developer would actually run
(`devm_platform_ioremap_resource`, `netif_napi_add`, `blk_mq_alloc_tag_set`):
a different set of 102 queries returning 188,862 matches, none above 6,000.
ripgrep's Linux total barely moved — 33–46s across the five most recent runs
spanning both query sets, with a single 103s outlier in the earliest, because it
scans every file whichever pattern it gets — while tgrep's fell from 49–128s on
the old set to 6.5–8.1s on the new one. That difference is the delivery cost,
isolated.

A caution on reading the ratios: the ripgrep baselines themselves move between
runner sessions, and macOS moves most. Four runs of the *identical* kernel suite
measured macOS ripgrep at 385s, 388s, 495s and 500s — a 1.3x spread from runner
variance alone — so treat a single macOS column as an order of magnitude rather than a
precise figure. The Linux column is the stable one, and it is also the least
flattering.

Practical guidance: tgrep is a strong replacement for ripgrep on large repos.
Expect the biggest wins on Windows and macOS, and on selective queries anywhere.
A repo-wide search for an extremely common token is the one case where it can lose
to ripgrep, and it is also the case where neither tool gives you a useful answer.
