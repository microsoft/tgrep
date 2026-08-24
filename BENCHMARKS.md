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
| chromium/chromium | 504,351 | 30 | **17.6x** | **15.8x** | **3.81x** |
| mozilla/gecko-dev | 387,841 | 122 | **38.6x** | **51.9x** | **7.36x** |
| torvalds/linux | 95,831 | 102 | **34.8x** | **21.0x** | **9.38x** |
| rust-lang/rust | 62,326 | 102 | **7.69x** | **2.69x** | **1.61x** |
| kubernetes/kubernetes | 31,300 | 97 | **7.08x** | **2.81x** | **0.93x** |
| golang/go | 15,833 | 103 | **7.53x** | **3.12x** | **1.29x** |

All 18 cells are from the complete 24 August 2026 sweep at `82b88a1`, measured on
GitHub-hosted runners (`windows-latest`, `macos-latest`, `ubuntu-latest`). Every
headline latency, ratio, file count, and primary run link in the repo sections
below comes from that same sweep rather than mixing the latest totals with minima
from older runs. Older runs are cited only to show the kernel suite's variance.
Geometric mean speedup across the six repos is **14.6x on Windows, 8.61x on macOS,
and 2.82x on Linux**. The largest margin is Gecko on macOS at 51.9x.
**Kubernetes on Linux is the one cell ripgrep wins**, at 0.93x — a near-tie
(101.8 ms versus 94.4 ms per query) on a warm page cache, where match volume costs
tgrep more in delivery than the index saves in file selection. See
[What decides the margin](#what-decides-the-margin).

Shared-runner results are not controlled-machine measurements. Compare tgrep with
ripgrep **within a row**, not absolute milliseconds between workflow runs: CPU,
storage and page-cache variance move both tools, particularly on macOS.

### Index-build peak memory in the latest sweep

The benchmark logs also record the indexer's peak memory, in MiB:

| Repo | Windows | macOS | Linux |
| --- | ---: | ---: | ---: |
| chromium/chromium | **402.9** | **462.6** | **332.2** |
| mozilla/gecko-dev | **347.8** | **416.3** | **256.9** |
| torvalds/linux | **135.4** | **216.7** | **129.6** |
| rust-lang/rust | **114.7** | **150.8** | **109.4** |
| kubernetes/kubernetes | **109.1** | **145.3** | **110.7** |
| golang/go | **108.0** | **137.2** | **108.7** |

These are **not comparable to figures published before this sweep**, which were
peak working set / RSS and so counted mapped file pages the OS can reclaim.
`index` now reports private bytes — `PagefileUsage` on Windows, `RssAnon` on Linux
— and names the working set separately only when it is materially larger. None of
these six repos holds a file big enough to make the two diverge, so the numbers
land close to the old ones by coincidence; they measure the memory tgrep owns
rather than what the OS happened to have resident.

Peak memory tracks corpus size rather than query count, and stays bounded: the
largest here is Chromium at 504K files and 2.6 GB of index, which builds in under
470 MiB on every platform.

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
Windows, `VmHWM` on Linux), sampled externally. Since large files became
memory-mapped, that counter also counts mapped file pages, so it overstates what
the process holds — where that matters below, private bytes are reported
alongside it. `tgrep index` and `tgrep serve` now lead with private bytes for
that reason; see "What `peak memory` reports" below.

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
and with it the memory argument for a cap. ripgrep has no default
`--max-filesize` either, and this is the mechanism that lets it not need one.

(A 64 MiB default was later reinstated for a different reason: not build memory,
which mapping had solved, but repeated *scan* time on the pathological tail. See
"File size limits" in the README.)

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

**What `peak memory` reports.** Mapping large files made the number tgrep printed
wrong. It was `PeakWorkingSetSize` / `VmHWM`, which counts resident mapped file
pages, so once indexing mapped files it tracked the size of the files rather than
the memory tgrep held. A `tgrep serve` bootstrap over a 290k-file internal
monorepo reported `peak memory 13.49 GiB`, which prompted this investigation.

Reproducing at that scale showed the build itself was never the problem. On a
generated 290,700-file / 4.2 GB corpus:

| | Reported peak | Private bytes | Wall |
| --- | ---: | ---: | ---: |
| `tgrep index` | 305.1 MiB | 318.0 MiB | 816 s |
| `tgrep serve` bootstrap | 382.3 MiB | 386.8 MiB | 609 s |

The builder is bounded by construction — batches cap at 1,024 files and 64 MiB of
heap, the sort arena is 64 MiB, and the merge reads through buffers sized from
that same budget — so scale alone cannot produce gigabytes. Isolating a single
oversized file does, because `batch_ranges` gives a file larger than a budget a
batch of its own and the whole file is then mapped and scanned:

| Corpus | Reported peak (old) | Private bytes | Overstatement |
| --- | ---: | ---: | ---: |
| 1 x 2 GiB file | 1.99 GiB | **77.8 MiB** | 26x |
| 200 x 8 MiB files | 470.2 MiB | **365.4 MiB** | 1.3x |
| 290,700 files, 4.2 GB | 305.1 MiB | 318.0 MiB | none |

So the metric, not the build, is what scales with file size. Two consequences
were fixed:

*Reporting.* Both commands now lead with private bytes — `PagefileUsage` on
Windows, `RssAnon` on Linux — and name the working set separately only when it is
both 25% and 64 MiB larger, so the mapped-page component is visible instead of
being folded into a figure that reads as tgrep's own use. The 2 GiB case now
prints `77.8 MiB private, 1.99 GiB working set incl. memory-mapped files`.
Linux has no kernel high-water mark for anonymous memory alone, so the peak is
sampled on a background thread for the duration of a build; without it the
reported peak would be whatever survived the build, which is a small fraction of
the true high point once the sorter drops its arena. macOS exposes the split only
through Mach `TASK_VM_INFO`, which `libc` does not surface, so it still reports
resident set.

*The memory cap.* It was enforced against the working set too. Mapped pages are
file-backed and reclaimable, so charging them against a heap budget fires the cap
on memory no flush can return — the build pays for a full overlay flush and is
still over budget on the next check, which is exactly the "flush did not reclaim
memory" path the code warns about. The cap now charges private bytes.


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

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32763938779)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32763934524)

- **Repo**: [chromium/chromium](https://github.com/chromium/chromium) (504,351 files)
- **Queries**: 30 (mix of literals, multi-word, and regex)
- **Index build time**: ~52s (Linux), ~73s (Windows), ~248s (macOS)
- **Index size**: ~2,584 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 737,275 | 24,575.8 | 0 |
| tgrep (client → serve) | 41,884 | 1,396.1 | — |

**tgrep is ~17.6x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 1,254,185 | 41,806.2 | 0 |
| tgrep (client → serve) | 79,293 | 2,643.1 | — |

**tgrep is ~15.8x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 72,127 | 2,404.2 | 0 |
| tgrep (client → serve) | 18,942 | 631.4 | — |

**tgrep is ~3.81x faster**

---

## mozilla/gecko-dev (388K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32763946964)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32763942874)

- **Repo**: [mozilla/gecko-dev](https://github.com/mozilla/gecko-dev) (387,841 files)
- **Queries**: 122 (mix of C++, JavaScript, and Python patterns)
- **Index build time**: ~35s (Linux), ~58s (Windows), ~165s (macOS)
- **Index size**: ~1,952 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 2,176,629 | 17,841.2 | 0 |
| tgrep (client → serve) | 56,435 | 462.6 | — |

**tgrep is ~38.6x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 4,075,017 | 33,401.8 | 0 |
| tgrep (client → serve) | 78,447 | 643.0 | — |

**tgrep is ~51.9x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 145,774 | 1,194.9 | 0 |
| tgrep (client → serve) | 19,815 | 162.4 | — |

**tgrep is ~7.36x faster**

---

## torvalds/linux (96K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32763930116)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32763926423)

- **Repo**: [torvalds/linux](https://github.com/torvalds/linux) (95,831 files)
- **Queries**: 102 (mix of literals, multi-word, and regex)
- **Index build time**: ~21s (Linux), ~26s (Windows), ~37s (macOS)
- **Index size**: ~1,000 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 334,555 | 3,280.0 | 0 |
| tgrep (client → serve) | 9,612 | 94.2 | — |

**tgrep is ~34.8x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 549,813 | 5,390.3 | 0 |
| tgrep (client → serve) | 26,124 | 256.1 | — |

**tgrep is ~21.0x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 43,542 | 426.9 | 0 |
| tgrep (client → serve) | 4,643 | 45.5 | — |

**tgrep is ~9.38x faster**

This suite has now been measured five times on separate runner sessions. Linux came
out at 5.2x, 5.5x, 5.7x, 6.2x and 9.38x; macOS at 21.0x, 22.4x, 24.7x, 27.3x and
31.7x ([32481331031](https://github.com/microsoft/tgrep/actions/runs/32481331031),
[32456274534](https://github.com/microsoft/tgrep/actions/runs/32456274534),
[32457317972](https://github.com/microsoft/tgrep/actions/runs/32457317972)).
Only the latest run carries this branch's read-once and lazy-line-index work, so
its Linux figure is not purely runner variance; the earlier four are. The tables
report the linked latest run consistently, and the spread is retained to show why a
shared-runner ratio should not be read as a controlled benchmark.

---

## rust-lang/rust (62K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32763971982)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32763967731)

- **Repo**: [rust-lang/rust](https://github.com/rust-lang/rust) (62,326 files)
- **Queries**: 102 (mix of Rust patterns, macros, traits, and regex)
- **Index build time**: ~4s (Linux), ~6s (Windows), ~8s (macOS)
- **Index size**: ~199 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 151,921 | 1,489.4 | 0 |
| tgrep (client → serve) | 19,759 | 193.7 | — |

**tgrep is ~7.69x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 66,769 | 654.6 | 0 |
| tgrep (client → serve) | 24,849 | 243.6 | — |

**tgrep is ~2.69x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 14,708 | 144.2 | 0 |
| tgrep (client → serve) | 9,115 | 89.4 | — |

**tgrep is ~1.61x faster**

---

## kubernetes/kubernetes (31K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32763963292)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32763959155)

- **Repo**: [kubernetes/kubernetes](https://github.com/kubernetes/kubernetes) (31,300 files)
- **Queries**: 97 (mix of Go patterns, Kubernetes API types, and regex)
- **Index build time**: ~4s (Linux), ~7s (Windows), ~5s (macOS)
- **Index size**: ~215 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 130,200 | 1,342.3 | 0 |
| tgrep (client → serve) | 18,381 | 189.5 | — |

**tgrep is ~7.08x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 27,731 | 285.9 | 0 |
| tgrep (client → serve) | 9,881 | 101.9 | — |

**tgrep is ~2.81x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 9,158 | 94.4 | 0 |
| tgrep (client → serve) | 9,878 | 101.8 | — |

**ripgrep is ~1.08x faster** — tgrep is 0.93x

---

## golang/go (16K files)

[Benchmark Run (Windows)](https://github.com/microsoft/tgrep/actions/runs/32763954981)
[Benchmark Run (Linux/macOS)](https://github.com/microsoft/tgrep/actions/runs/32763950876)

- **Repo**: [golang/go](https://github.com/golang/go) (15,833 files)
- **Queries**: 103 (mix of Go stdlib patterns, testing, and regex)
- **Index build time**: ~2s (Linux), ~3s (Windows), ~3s (macOS)
- **Index size**: ~113 MB

### Windows AMD64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 60,949 | 591.7 | 0 |
| tgrep (client → serve) | 8,093 | 78.6 | — |

**tgrep is ~7.53x faster**

### macOS Apple Silicon (Darwin arm64)

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 21,075 | 204.6 | 0 |
| tgrep (client → serve) | 6,753 | 65.6 | — |

**tgrep is ~3.12x faster**

### Linux x86_64

| Tool | Total (ms) | Avg per query (ms) | Timeouts (120s) |
| --- | ---: | ---: | ---: |
| ripgrep | 4,541 | 44.1 | 0 |
| tgrep (client → serve) | 3,516 | 34.1 | — |

**tgrep is ~1.29x faster**

---

## Key takeaways

- **Repo size is the strongest predictor.** The trigram index eliminates scanning files
  that can't match, so the advantage grows with the corpus. On the two largest repos
  (Chromium 504K files, gecko-dev 388K files) tgrep wins on every platform, by 3.81–51.9x.
  On the smallest (Go, 16K files) the Linux margin narrows to 1.29x.
- **Windows benefits most consistently** — geometric mean **14.6x**, and never below
  7.08x in any cell. Windows per-file open/read overhead is high, so skipping 90%+ of
  the files pays off everywhere.
- **macOS has the highest peak but more spread** — geometric mean **8.61x**, range
  2.69–51.9x.
- **Linux is the weakest case** — geometric mean **2.82x**, range 0.93–9.38x, and it
  holds the matrix's only loss. Linux's page cache plus ripgrep's parallel scan make
  brute force genuinely cheap on a warm repo, so the index buys less.
- ripgrep never hit the 120s per-query cap in any run (Timeouts = 0 in every table),
  so every ratio here is a true measured value, not a censored one.
- Index build is a one-time cost — ~3s for Go on macOS, ~248s for Chromium there — and the
  server then watches for file changes and updates incrementally.

### What decides the margin

tgrep loses exactly one of the eighteen cells — Kubernetes on Linux, at 0.93x — and
the measured margin elsewhere ranges from 1.29x to 51.9x. Two things move it.

**Repo size**, as above: more files means more files the index can skip.

**Match volume**, which is easy to miss. tgrep's cost per *delivered* match —
serialize, ship over IPC, deserialize, print — is higher than ripgrep's, because
ripgrep writes straight to stdout from the scanning thread. A query returning tens
of thousands of matches can spend more on delivery than the index ever saved on
file selection.

The kernel suite used to demonstrate this the hard way. Its queries were generic
tokens — `read`, `write`, `^#define\s+[A-Z_]+` — that matched most of the tree:
5,398,512 matches across 102 queries, with a single query returning 2,089,941.
On that query set kernel Linux lost every run: 0.81x, 0.74x and 0.95x, the closest
being 48.6s against ripgrep's 46.4s. Index pruning was not the problem; measured
directly on a local kernel checkout, the index still narrowed to **4–12% of the
95K files**.

The suite now uses queries a kernel developer would actually run
(`devm_platform_ioremap_resource`, `netif_napi_add`, `blk_mq_alloc_tag_set`):
a different set of 102 queries returning 188,862 matches, none above 6,000.
ripgrep's Linux total barely moved — 33–46s across the six most recent runs
spanning both query sets, with a single 103s outlier in the earliest, because it
scans every file whichever pattern it gets — while tgrep's fell from 49–128s on
the old set to 4.6–8.1s on the new one. That difference is the delivery cost,
isolated.

A caution on reading the ratios: the ripgrep baselines themselves move between
runner sessions, and macOS moves most. Five runs of the *identical* kernel suite
measured macOS ripgrep at 385s, 388s, 495s, 500s and 550s — a 1.4x spread from runner
variance alone — so treat a single macOS column as an order of magnitude rather than a
precise figure. The Linux column is the stable one, and it is also the least
flattering.

Practical guidance: tgrep is a strong replacement for ripgrep on large repos.
Expect the biggest wins on Windows and macOS, and on selective queries anywhere.
A repo-wide search for an extremely common token is the one case where it can lose
to ripgrep, and it is also the case where neither tool gives you a useful answer.
