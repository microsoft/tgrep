# Shared-worktree performance baseline

[`scripts/benchmark_shared.py`](scripts/benchmark_shared.py) compares **one ordinary
server and index per worktree** against **one repository daemon with private
views**. This is separate from the [large-repository search benchmarks](BENCHMARKS.md).
It measures the currently implemented sharing, not a proposed cache, base
migration, garbage collector, or runtime integration.

## Measured baseline: 6 October 2026

**Sharing saves substantial LF memory/storage in this fixture, but is not a
query-speed win; clean CRLF can reverse the memory/storage benefit.** At 32
worktrees and 2 MiB/tree, LF endpoint PSS fell from about 272 to 19 MiB and
persistent storage from 58.75 to 1.84 MiB. However, delivered ready startup rose
from 2.05 to 10.19 seconds and warm CLI query p95 from 11.45 to 35.48 ms.
For clean CRLF at the same size/count, PSS instead rose from 267 to 316 MiB and
storage from 58.84 to 148.54 MiB. Keep shared mode opt-in and evaluate the
checkout transformations, memory budget and latency tradeoff of the intended
workload before a targeted feature-flag rollout. These measurements do not
justify a universal default.

The **37 selected paired cases / 74 mode runs** below passed full JSON/context/filename
equality, positive backend proof, timed-sample checks, output-schema validation
and owned cleanup. Retained evidence contains **43 validated pairs / 86 runs**:
six original LF cases are superseded by six explicitly identified corrective
cases after a coordination-overlap audit. This is one selected measured run per
case, not a confidence interval or production-scale proof. The primary corpus
is deliberately small; the separately labeled file-count supplement increases
it 16-fold.

### Exact provenance

| Item | Value |
| --- | --- |
| Binary source | `2b2d497cc945c33ba4674b71c04f2ffd65221aff` (main plus the separately owned JSON position-detail fix) |
| Linux binary SHA-256 | `3d0e1a2fd9bf8019284c30393e440eb7ec079e3361b2656f48c01dcb1c26c253` |
| Version/build | `tgrep 1.0.11`; `cargo build --release --locked -j 4`; rustc 1.98.1 `48a229cea`, cargo 1.98.1 `797e8a9bc` |
| Frozen harness commit | `5c528fdd75e84a8e781db8c1c75fb94c31a160df` |
| Harness SHA-256 | `7165905b20edf2765a1dea46d3428ac2de670e664f9185e3aefe9857ad228650` |
| Host | Intel Xeon Platinum 8370C, WSL2 Linux `6.18.33.2-microsoft-standard-WSL2`, x86_64, 16 visible logical CPUs, about 32 GiB RAM |
| Filesystem/tools | Native ext4 binary and `/tmp` fixtures; Python 3.12.3; Git 2.43.0 |
| Runtime parameters | Seed `20261005`; 2 Rayon threads/process; shipped native-watch/view budgets; shared full-pass interval 120s |
| Timing windows (UTC) | Primary 06:29:42-06:50:43; LF scale 06:50:53-06:58:37; CRLF scale 06:58:57-07:00:04; corrective LF 07:08:01-07:09:18 |

The final harness includes **post-measurement provenance/validation/safety hardening**:
optional `rustc`/`cargo --version` probes now have five-second
deadlines and report missing executables, nonzero exits and invocation timeouts
as unavailable metadata. Process containment and cleanup failures remain fatal.
The validator now requires the documented evidence sections, checks storage
types/completeness, and cross-checks process inventories, aggregates/deltas,
startup, status/counters, refresh/churn/idle coverage and actual cleanup records
instead of trusting only summary booleans. All 43 historical pairs pass those
stricter checks unchanged.
Setup now caps generated file count at 4,096, reserves allocation/metadata
overhead before population, and shares workload-parameter bounds between CLI
parsing and report validation, including the previously unchecked churn pause.
The measured 4,096-file supplement remains supported.
Successful timing commands, workload, measurements and equality gates are
unchanged. The artifacts were neither rerun nor relabeled for these follow-ups:
their exact measured source is the frozen commit/hash above. Use that commit
when reproducing the historical source byte-for-byte.

No benchmark-session build/test ran concurrently with timing, but a later
timestamp audit found the coordinator's Windows combined smoke ran
**06:29:40-06:30:33 UTC**, overlapping the original primary's first 51 seconds.
Its native smoke ended at 06:29:42. The raw primary is retained unchanged:
**all LF1/4/16 fresh/restart rows in the table use the corrective artifact**,
rerun with identical workload/thread/sample parameters and `--idle-seconds 0`
after the coordinator confirmed no remaining local workloads. LF32 and the
later workloads/scales retain original provenance. No rows were silently
averaged, and the overlapped rows are not used to improve or worsen comparisons.

The sessions otherwise coordinated a no-heavy-build window. This was still a
shared Windows/WSL host, not exclusive hardware or a controlled CPU/storage
laboratory. Original primary Linux load averages were recorded at both ends
(`0.463/0.145/0.049` and `1.850/1.810/1.248`). No global OS caches were purged.
Fresh here means **no existing index**, not cold physical storage.

### Primary: 256 files x 8192 bytes = 2 MiB/tree

Thirty warm query samples per view (ten of each query), both startup conditions,
four workloads and 1/4/16/32 worktrees. `O / S` means ordinary aggregate / shared
daemon. Ready time is total sequential startup through all views' delivered
query-readiness contracts; see the restart-contract caveat below.

| LF views | Condition | Samples/mode | Ready ms O / S | Query p50 ms O / S | Query p95 ms O / S |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1 | Fresh, corrective | 30 | 45.8 / 455.8 | 4.83 / 13.06 | 11.25 / 35.79 |
| 4 | Fresh, corrective | 120 | 171.0 / 1468.6 | 4.50 / 12.32 | 11.99 / 34.50 |
| 16 | Fresh, corrective | 480 | 999.9 / 4964.2 | 4.22 / 12.11 | 11.38 / 34.86 |
| 32 | Fresh | 960 | 2051.9 / 10185.1 | 4.39 / 13.17 | 11.45 / 35.48 |
| 1 | Restart, corrective | 30 | 21.9 / 353.6 | 4.20 / 13.64 | 11.03 / 47.65 |
| 4 | Restart, corrective | 120 | 87.3 / 1262.7 | 4.29 / 11.99 | 10.99 / 34.49 |
| 16 | Restart, corrective | 480 | 351.7 / 4870.3 | 4.54 / 12.42 | 11.30 / 34.61 |
| 32 | Restart | 960 | 709.3 / 9679.0 | 4.47 / 12.19 | 11.52 / 36.14 |

In LF32/fresh, the first shared build/registration call took **265.47 ms** and
the first view became ready after **529.9 ms**; the 32nd attach reused the base,
taking **43.02 ms** for registration and **317.4 ms** through readiness. These
are distinct first/subsequent attach observations, not a pure base-build timer.
The daemon does not expose a standalone base-build duration; that metric remains
null with a reason. It does expose actual build/extraction counters below.

The other primary workload/count/condition results remain in the raw artifact;
these 32-view fresh endpoints illustrate the different overlay costs:

| Workload, 32 views | Ready ms O / S | Query p95 ms O / S (960 samples/mode) | Endpoint PSS MiB O / S | Persistent MiB O / S |
| --- | ---: | ---: | ---: | ---: |
| LF | 2051.9 / 10185.1 | 11.45 / 35.48 | 272.0 / 19.1 | 58.75 / 1.84 |
| Clean CRLF | 1925.5 / 17195.1 | 11.79 / 36.96 | 266.9 / 316.5 | 58.84 / 148.54 |
| Divergent/private | 1950.3 / 10280.4 | 11.90 / 35.93 | 273.4 / 18.8 | 58.34 / 1.86 |
| Bounded churn | 1898.0 / 10078.6 | 11.60 / 35.90 | 278.9 / 26.9 | 58.75 / 2.42 |

Memory is `resources_final` (sampled, not peak), after warm queries and any
churn/refresh/idle observation. LF32 RSS sums were **492.0 / 20.9 MiB**; these
are not physical mmap-deduplication measurements. Primary LF32 startup server
CPU was **2.09 / 1.68 seconds**, while the 960-query phase used **2.16 / 9.23
seconds** in the sampled servers. Whole-CLI wall time includes work excluded
from those CPU totals.

At LF32 readiness, Linux `rchar` was **68.64 / 72.52 MiB**. Kernel
`read_bytes` was **0 / 28,672 bytes**. These are supported, observed counter
values, not claims of zero physical I/O or daemon-only file bytes. The cache was
already warm from constructing and checking out the fixture.

### Real sharing and idle/churn work

Each 32-view fresh shared case built **one base**, extracted **259 blobs** for
that base, reused that generation on **31** subsequent attaches, and reported
`base_sharing_views = 32`. Initial reconciliation still read every admitted
file, including the ordinary linked-worktree `.git` pointer:

| Workload | Initial view reads | Initial content bytes read | Private extractions | Base reuses |
| --- | ---: | ---: | ---: | ---: |
| LF | 8,320 | 67,112,736 | 32 | 8,288 |
| Clean CRLF | 8,320 | 69,357,440 | 8,256 | 64 |
| Divergent/private | 8,320 | 66,589,932 | 96 | 8,224 |
| Churn, before edits | 8,320 | 67,112,736 | 32 | 8,288 |

These are actual daemon counters, not inferred ordinary-server extraction
counts. LF's 32 private extractions are the linked `.git` files. CRLF needs
private extraction of every transformed text file plus those gitfiles. The
shared base occupied about **1.792 MiB** in these cases; private checkpoints at
the final endpoint occupied **0.045 MiB LF**, **146.751 MiB CRLF**,
**0.067 MiB divergent** and **0.625 MiB churn**.

In LF32's 130-second idle observation, all 32 shared views completed one
additional full reconciliation: **8,320 additional content reads, zero new
extractions**, and **3.56 seconds** of sampled server CPU. Changes were observed
in status samples around 120.11-121.13 seconds; the daemon advertised one
serialized reconcile worker. These are polling-resolution observations, not
exact per-view scheduler deadlines or latency targets. Ordinary native servers
did not report a full reconciliation in that window and used **1.97 seconds**
of aggregate server CPU; the idle status probes themselves add overhead.
One shared-view lookup at 120.107 seconds reported `ready: false` while its
reconciliation was running. This was an idle status observation, not a timed
query failure; no queries were issued during that phase, and these samples do
not establish the duration of that readiness transition.

At 32 views, twelve all-view write bursts produced **384 observed latency
samples per mode**. Fresh churn write-to-indexed-observation p50/p95/max was
**282.0/504.3/555.0 ms ordinary** and **844.9/1178.2/1231.2 ms shared**.
Serial polling and scan checks delay later observations in each burst; these
are upper bounds, not isolated watcher service times. Shared no-op hint
acknowledgment after churn had p50/p95 **31.43/33.01 ms**, 32 samples.
Shared logical storage grew from **1.837 to 2.418 MiB**, with its base unchanged;
ordinary persisted storage stayed **58.747 MiB** through the observed endpoint.
This records each implementation's checkpoint/autosave behavior, not equal
durability guarantees.

### Separate file-count/content-scale supplement: 32 MiB/tree

4096 files x 8192 bytes, fresh only, same binary/harness/seed. LF used
1/4/16/32 views (at most 1 GiB working-tree text); clean CRLF used four views.
Before population the host had about 934 GiB free native-ext4 space and 30 GiB
available RAM. No resource downscaling was necessary.

Only **three samples/view** (one of each query) were collected here: totals
3/12/48/96 for the LF rows and 12 for CRLF. The displayed p95 is descriptive
and often the maximum, **not a robust tail estimate**. Do not combine these
samples with the 30/view primary matrix.

| Workload/views | Ready ms O / S | Endpoint PSS MiB O / S | Persistent MiB O / S | Query p50 ms O / S | Descriptive p95 ms O / S |
| --- | ---: | ---: | ---: | ---: | ---: |
| LF / 1 | 548.2 / 2453.6 | 73.4 / 45.1 | 29.00 / 28.33 | 8.05 / 23.44 | 114.63 / 409.25 |
| LF / 4 | 2342.7 / 4562.2 | 269.0 / 57.1 | 115.98 / 28.33 | 8.30 / 23.17 | 138.85 / 425.51 |
| LF / 16 | 9515.2 / 13210.5 | 1066.9 / 81.2 | 463.90 / 28.35 | 8.64 / 22.86 | 126.12 / 402.08 |
| LF / 32 | 18735.5 / 25730.1 | 2129.6 / 107.9 | 927.81 / 28.37 | 9.90 / 23.30 | 131.00 / 410.25 |
| Clean CRLF / 4 | 2275.4 / 23096.1 | 274.7 / 758.4 | 116.16 / 321.74 | 7.98 / 33.36 | 131.47 / 412.08 |

LF32 scale startup server CPU was **29.77 / 19.17 seconds**. The shared first
attach's build/registration took **1805.0 ms**, versus **49.1 ms** for the 32nd;
full content verification still follows registration. CRLF4 scale read **16,400
files** and performed **16,392 private extractions** during initial view
reconciliation. This supplement exposes path/content-size costs beyond fixed
per-process overhead, but 4096 synthetic files are still not a giant production
repository with realistic language, directory, ignore and query distributions.

### Audit the retained evidence

These are lossless gzip files of the original completed JSON, with gzip timestamp
zero. Raw samples, successful backend diagnostics, exact parameters,
fixture revisions, daemon counters, host/binary metadata and cleanup records
are retained. No timing fields were rewritten for publication.

| Artifact | Raw bytes | Gzip bytes |
| --- | ---: | ---: |
| [Primary](scripts/benchmark-results/2026-10-06-linux-primary.json.gz) | 30,574,049 | 971,523 |
| [LF scale](scripts/benchmark-results/2026-10-06-linux-scale-lf.json.gz) | 1,396,774 | 58,428 |
| [CRLF scale](scripts/benchmark-results/2026-10-06-linux-scale-crlf.json.gz) | 122,851 | 10,477 |
| [LF quiet-window correction](scripts/benchmark-results/2026-10-06-linux-lf-correction.json.gz) | 2,135,306 | 97,204 |

| Artifact | SHA-256 of gzip | SHA-256 of decompressed JSON |
| --- | --- | --- |
| Primary | `700642cc711716c73ec548e5ff9048020f9b628e70aebb40103482bfbbe39b2a` | `89ea1bc4a7c6011acaf2c9d2e121dccf22ffbc1be812b7db6c0f53aa42b58646` |
| LF scale | `24e012e5d91ba9db61c5eea2ccca50b9a0ed1579813a5e6b8c6e79404d3024c5` | `c1aa0ba6c1d3f15d3476f461deeac6674d21eadf7adf5469016dafb1aa7a297b` |
| CRLF scale | `004dddaedc4e7ee2a0e2adf762a78b88f5ce3df0966ed0dc0cfcba780d8370d4` | `77a293b17b1dd80761c67de361759b8cff068add9ae56a998614eb2f1807b387` |
| LF correction | `cb18cfd78f63f9aa2065b8f4695009b10701d080589864316d0432001d52a112` | `f63d1810f390e2662296bf73aa209152e8050c1650e27dc28679070780a89a51` |

Read and validate without extracting files (works on Windows and Linux):

```powershell
python -B -c "import gzip,json,sys; from pathlib import Path; sys.path.insert(0,'scripts'); import benchmark_shared as b; reports=[json.loads(gzip.decompress(p.read_bytes())) for p in Path('scripts/benchmark-results').glob('*.json.gz')]; [b.validate(r) for r in reports]; assert all(r['ok'] for r in reports); print(sum(len(r['cases']) for r in reports),'validated paired cases')"
```

To decompress one file, `python -m gzip -d <artifact.json.gz>` writes its sibling
`.json` file. Check hashes before/after decompression when independently auditing.
The validation command reports **43 validated paired cases**. For the 37-case
selected baseline, exclude original-primary `scenario == "lf"` rows with
`worktrees in [1, 4, 16]`, and use the six corrective cases in their place.
Original offset-failure, restricted-output experiments, and an interrupted
pre-backend-proof run are **not accepted baseline inputs** and are not mixed
into these artifacts or tables.

## Reproduce

Python 3.10+, Git, and an already-built `tgrep` binary are required. The harness
uses only the Python standard library and never builds during measurements.
Build the exact source revision you intend to measure:

```powershell
cargo build --release --locked
python -B scripts\benchmark_shared.py --binary target\release\tgrep.exe --binary-commit <full-source-SHA> --output shared-baseline.json
```

On Linux, replace the executable with `target/release/tgrep` and use `/` in
script paths. In WSL, keep **fixtures and the binary on native ext4**, for example
`--temp-parent /tmp`; a checkout on `/mnt/c` or `/mnt/q` is not an equivalent
native watcher test. Do not benchmark while building or running unrelated tests.
Use `--host-note` to describe unavoidable shared-host activity.

The default matrix is 1, 4, 16, and 32 worktrees, four scenarios, and two startup
conditions. Each tree has 128 synthetic 4 KiB text files. At most one mode/case
is live at a time. `RAYON_NUM_THREADS=2` is applied to both modes' processes;
this is a per-process limit, not an equal aggregate CPU quota (ordinary mode
has up to 32 server processes). Shared daemon worker/watch/view budgets and the
120-second reconciliation interval retain their shipped defaults.

The accepted primary used the defaults except `--files 256 --file-bytes 8192
--samples-per-view 30 --churn-rounds 12 --churn-interval 0.25 --temp-parent /tmp`.
Its `--binary-commit` is the exact source SHA above. The LF scale run instead used
`--scenarios lf --conditions fresh --files 4096 --file-bytes 8192
--samples-per-view 3 --churn-rounds 2 --idle-seconds 0 --temp-parent /tmp`;
CRLF scale additionally used `--scenarios crlf --worktrees 4`.
The corrective run kept the primary's file size/count, seed, 30 samples/view and
thread settings, using `--scenarios lf --worktrees 1,4,16 --idle-seconds 0`.
Full parameter values and host notes are in each artifact.

The frozen harness passed 14 unit tests on Windows and 13 plus one Windows-only
skip on native Linux. Actual small smokes passed all four scenarios at 1/4
worktrees: eight fresh paired cases on Windows and sixteen fresh/restart paired
cases on Linux, with strict backend/JSON/schema gates and successful cleanup.
These smokes are functional cross-platform evidence, not additional timing rows
in the Linux baseline.
The current post-measurement source passes **27 tests on Windows** and
**26 plus one Windows-only skip on Linux**, including failed optional-tool
probes, fatal cleanup-error propagation, missing/inconsistent report evidence,
shared parameter bounds and allocation-aware disk safety.
An additional real LF/fresh smoke with one view, eight 256-byte files and three
queries/view passed on Windows and native-ext4 Linux. It exercised `main()`'s
pre-finalization validation, finalized report validation, and owned cleanup;
these functional timings are not added to the baseline.

**Merge prerequisite:** [PR #175](https://github.com/microsoft/tgrep/pull/175),
specifically the generic root-script discovery in
[`ci.yml` at `e7b62505f104f72b5cffaa3c816dd06b000d58f7`](https://github.com/microsoft/tgrep/blob/e7b62505f104f72b5cffaa3c816dd06b000d58f7/.github/workflows/ci.yml),
must land before this benchmark PR. It runs
`python -B -m unittest discover -s scripts -p 'test_*.py' -v` in the
Ubuntu/macOS/Windows test matrix. The standalone benchmark branch's preexisting
CI does **not** discover these tests yet; its green Rust/agent CI is not a claim
that it ran this Python suite. The coordinator independently verified combined
root discovery with the frozen 14-test suite on Windows/Linux; the same discovery
command runs all 27 current tests locally. Workflow changes remain owned by the
qualification PR, not duplicated here.

For a small cross-platform functional smoke, not a performance claim:

```powershell
python -B -m unittest discover -s scripts -p test_benchmark_shared.py -v
python -B scripts\benchmark_shared.py --binary target\release\tgrep.exe --binary-commit <full-source-SHA> --output shared-smoke.json --worktrees 1,4 --scenarios lf,crlf,divergent,churn --conditions fresh --files 8 --file-bytes 256 --samples-per-view 3 --churn-rounds 2 --idle-seconds 0
```

The benchmark exits nonzero on a correctness discrepancy, timeout, child failure,
or incomplete cleanup. There are **no hardware-sensitive performance thresholds**.
The output path must not already exist. A completed run can also be checked with
`benchmark_shared.validate(json.load(...))` after importing the module from
`scripts`; validation rejects incomplete matrices and inconsistent aggregations.

## Workload and correctness gate

The harness creates a deterministic **real Git repository**, not independent
copies pretending to be worktrees. Source bytes, seed, commit dates and identities,
revision, file count/size, queries and ignore configuration are recorded. Git
system/global configuration, inherited `GIT_*` overrides, hooks and templates
are excluded from the fixture commands. No network clone or user repository is
modified. Each pair runs against the **same physical worktrees** and verifies
their content fingerprints before each mode:

| Scenario | Checkout contents |
| --- | --- |
| `lf` | Clean LF files identical to committed blobs |
| `crlf` | Clean Git checkouts with deterministic `text eol=crlf` conversion; committed blobs remain LF |
| `divergent` | Per-worktree committed branch difference, private deletion and untracked addition, all pinned to the same starting base |
| `churn` | LF plus bounded all-view write bursts; six rounds by default, with a pause between completed rounds |

There is a hidden file and ignored noise in every tree. Three fixed queries
exercise a selective symbol, a broadly matching token and a miss. Before timing,
every view's indexed results must equal a `--no-index` scan, including full JSON
match/context records, source byte offsets, submatch spans and line text.
Context output is separately checked with `-C 1`. Only row ordering and path
separators are normalized; summary timing is excluded. `--files --hidden` is
also checked. **Every invocation uses `--stats` and must positively identify the
expected backend**: ordinary server, shared daemon, or filesystem scan/walk.
Identical output from the wrong backend, missing diagnostics, fallback warnings,
and unexpected diagnostic lines are rejected. Every timed query is checked
against its initial scan digest and retains its backend diagnostic.
Churn requires the new token to become visible **without a scan-fallback
diagnostic**, checks its full JSON result against disk, and repeats corpus parity.
The paired modes must produce equal scan-checked output digests.

An early strict smoke at `e9d55dbbf232f0e228695f15a640cf207d4b3348`
found an ordinary-server JSON `absolute_offset` error after a watched append:
the server reported 0 where a scan reported 256, despite matching line/text.
That failed run is not accepted as performance evidence. It was sent to the
parity/lifecycle owner rather than changing production behavior in this PR.
The focused fix is `2b2d497cc945c33ba4674b71c04f2ffd65221aff`; build that
revision or a descendant for the strict benchmark, including the small smoke.
The benchmark itself can remain a standalone change based on main.

## Measurement semantics

| Measurement | What is included / excluded |
| --- | --- |
| Fresh startup | No persisted index/base; sequential server starts or attaches through query readiness. **Not** a cold OS page cache: Git checkout creation has already read/written the corpus. |
| Restart startup | New server processes with retained ordinary indexes or shared bases/checkpoints. Worktree bytes reset to the same scenario. No OS cache purge. Mode order alternates by worktree count. |
| First vs subsequent attach | Per-view attach/build/registration RPC wall time and time through readiness are separate. The first attach may build a base; its real build counters identify this. There is no standalone daemon base-build timer, so that metric is null, not an invented subtraction. |
| Warm queries | A new CLI process for each `--json --stats -F` query, after equality/warmup; includes discovery, Git subprocesses, TCP, stats/output formatting/capture and process cleanup. Raw per-view/query samples, backend diagnostics, count, nearest-rank p50/p95/max. Serialized client load, not a concurrent throughput benchmark. |
| Churn | All views edited in a bounded burst, then polled serially for indexed visibility. Write-to-observed-result upper bounds include CLI/polling and observer head-of-line delay; they are not internal watcher timings. Raw rounds/polls/fallback diagnostics and per-round statuses are retained. |
| Explicit refresh | Shared-only no-op changed-path hint acknowledgment, including metadata walk/checkpoint, with returned counters. Ordinary has no equivalent acknowledged hint RPC and is labeled unavailable. This is distinct from mutation-to-visibility latency. |
| CPU and I/O | Aggregate **long-lived server processes only**, sampled at stage boundaries. CPU/memory exclude the harness, short-lived CLI clients and transient Git children; Linux `/proc/PID/io` may include waited-for children according to kernel accounting. Wall times include child work. These are not uniformly complete process-tree resource costs. |
| Memory | Sampled endpoint sums, **not peaks**. Linux RSS, anonymous RSS (`private_bytes`) and PSS from `/proc/PID/smaps_rollup`; Windows working set and `PrivateUsage`. Windows private bytes and Linux anonymous RSS are not identical concepts. |
| Read bytes | Linux `rchar` counts read-family syscall transfer bytes, including pipes; `read_bytes` is kernel storage-read attribution, not all mapped/cache traffic. Windows `ReadTransferCount` includes cached and non-file I/O. Neither is a direct physical-device read measurement. |
| Storage | Best-effort sum of logical file lengths, retrying concurrent atomic replacement. Records ready, after churn, final, and after stop; shared bases/checkpoints separately. Excludes fixture, logs and Git metadata; not allocated filesystem blocks. Retain-all remains in force. |
| Extraction/reconciliation | Actual daemon `attach_build`, `last_reconcile`, cumulative successful reads/extractions, readiness, attempts and base-sharing counts. Failed work is not included in successful-work totals. Ordinary lacks equivalent extraction counters: null with reason. |

Startup compares each backend's **delivered query-readiness contract**, not an
equal fresh-filesystem verification milestone. Ordinary waits for complete
hidden coverage and initial indexing to finish; on restart, it can serve an
existing index while startup reconciliation continues. Shared readiness includes
initial full revalidation and checkpoint publication. Interpret restart latency
differences with that stronger shared readiness contract in mind.

**RSS sums do not demonstrate physical mmap deduplication.** Linux PSS apportions
shared resident pages and is a useful additional observation, not proof of
application-level sharing by itself. The daemon's `base_sharing_views` and build/
extraction counters describe the implemented sharing more directly. Identical LF
trees still incur initial content reads; clean CRLF trees can require private
extraction for the entire corpus. The linked `.git` pointer file is private too.
The ordinary server has a private content cache; the shared daemon currently has
none. Memory and query-time differences therefore include cache policy, process/
thread overhead and discovery costs, not only trigram reader sharing.
Linux [`/proc/PID/io`](https://man7.org/linux/man-pages/man5/proc_pid_io.5.html)
includes waited-for children; its `rchar` must not be described as daemon-only
filesystem bytes. Windows `PrivateUsage` is committed private memory, not private
resident bytes. These exceptions govern interpretation of the raw resource
snapshots as well as the tables.
Zero is only recorded when a supported counter actually reports zero; unavailable
resource metrics contain `value: null` and an explicit reason.

The largest LF/fresh case spends 130 seconds without searches or mutations,
sampling status about once a second in **both** modes. Shared mode's default
120-second full pass uses one serialized reconcile worker. Ordinary native
watching uses a different periodic reconciliation schedule; this is observed,
not forced to match by changing defaults. Raw per-view attempts/read/extraction
totals and changed statuses show when idle work was observed. Status probes add
some observer CPU; completion detection has polling resolution, not a claimed
latency target.

## Safety and result format

Each case owns a unique temporary directory, a synthetic Git common directory,
all linked worktrees and external shared storage. Shared leases are registered
for cleanup before attach is sent; teardown detaches them, stops only owned
process trees, waits for exit, then removes only the owned fixture. Unix process
groups cover subprocess timeout/exception paths. Windows children are created
**suspended**, assigned to a kill-on-close Job Object, then resumed via documented
thread APIs. Teardown terminates the job, waits for zero active job processes,
reaps the direct child and closes its handle; it also handles a parent that has
already exited. Assignment/resume errors never run an uncontained child.
Transient Windows fixture-deletion permission failures are logged and retried
for at most five seconds, then fail explicitly. SIGINT/SIGTERM follow cleanup;
an uncatchable kill or host crash cannot
guarantee Python cleanup. Child stdout/stderr never uses an undrained pipe for
long-lived servers.

Limits include at most 32 views, 4,096 generated text files and 32 MiB nominal LF
text per tree, 100 churn rounds, and configurable per-command/readiness deadlines
(`--timeout`, default 60s). CLI parsing and report validation share the same
type/range checks for all workload parameters, including `churn_interval`,
file count/size, threads, samples/rounds, idle duration and timeout.
Default corpus footprint across 32 trees is roughly 16 MiB plus Git/index data;
index/process memory is additional. `--files`, `--file-bytes`, `--seed`,
`--samples-per-view`, `--churn-rounds`, `--threads` and `--idle-seconds` are recorded.
There is deliberately no real-repository clone mode or global cache manipulation.
Before Git initialization or corpus population, each fixture reads the temp
filesystem's allocation unit (`GetVolumePathNameW`/`GetDiskFreeSpaceW` on Windows,
`statvfs` on Unix). A failed geometry probe aborts rather than guessing. The
reserve rounds twice each nominal file size (worst-case CRLF expansion) to at
least a 4 KiB allocation unit, adds one unit of per-entry metadata allowance,
and allows eight auxiliary entries per tree. It covers the main checkout plus
all worktrees, multiplies by four for Git, both indexes and temporary
publications, and retains a 256 MiB fixed allowance. The file-count cap also
prevents millions of tiny files from passing the logical-byte cap.

This is a conservative preflight heuristic, not a disk reservation, a
filesystem-specific metadata upper bound or a process-memory cap. Other users
can consume free space after the check; check host memory before increasing the
corpus, especially for transformed private overlays.

JSON schema ID is `tgrep.shared-benchmark.v1`. Top-level fields include parameters,
binary source-commit attestation/hash/version, harness hash, host/tool versions,
metric semantics, cases, errors and cleanup. Each case includes exact fixture
revision/fingerprints, cleanliness, mode order and paired equality. Per-mode
records retain startup samples/statuses, raw query/churn/refresh/idle samples,
resource snapshots/deltas, storage growth, equality digests and cleanup/logs.
Storage `total`, `bases` and `checkpoints` are **separate sequential directory
walks**, not an atomic partition snapshot. Background checkpoint publication
between walks can make a later subtree sum exceed an earlier total. Validation
therefore requires every storage stage and nonnegative integer byte/file counts,
but does not impose `total >= bases + checkpoints` on live observations. Do not
interpret subtree arithmetic as simultaneous filesystem accounting. The 172
retained shared storage observations happen to satisfy that relationship; this
is an observation, not a guarantee made by the sampling method.

`validate(report)` requires a finalized successful envelope and complete
measurement/cleanup evidence. Internally, `main()` uses `finalized=False` before
writing its final `ok`/`error`/`finished_utc` fields; this skips only that final
envelope check, not any measurement or cleanup checks. Idle status validation
allows the observed transient unready state during a full reconciliation.
Once report construction succeeds, measurement, validation and cleanup failures
emit a partial report with `ok: false`; it must never be compared as a successful
baseline. Argument validation and required binary/Git version probes happen
before that boundary: their failures exit with a diagnostic and do not create
a JSON report. Optional Rust probe failures instead become unavailable metadata.
A binary SHA-256 identifies the executable exactly;
`--binary-commit` is an explicit caller attestation, not inferred from whatever
checkout happens to contain the script.
