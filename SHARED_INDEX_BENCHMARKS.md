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

All **37 paired cases / 74 mode runs** below passed full JSON/context/filename
equality, positive backend proof, timed-sample checks, output-schema validation
and owned cleanup. This is one measured run per case, not a confidence interval
or production-scale proof. The primary corpus is deliberately small; the
separately labeled file-count supplement increases it 16-fold.

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
| Timing windows (UTC) | Primary 06:29:42-06:50:43; LF scale 06:50:53-06:58:37; CRLF scale 06:58:57-07:00:04 |

No own build, test, or cross-platform smoke ran concurrently with accepted
timing. Other hardening sessions coordinated a no-heavy-build window. This was
still a shared Windows/WSL host, not exclusive hardware or a controlled CPU/
storage laboratory. Primary Linux load averages were recorded at both ends
(`0.463/0.145/0.049` and `1.850/1.810/1.248`). No global OS caches were purged.
Fresh here means **no existing index**, not cold physical storage.

### Primary: 256 files x 8192 bytes = 2 MiB/tree

Thirty warm query samples per view (ten of each query), both startup conditions,
four workloads and 1/4/16/32 worktrees. `O / S` means ordinary aggregate / shared
daemon. Ready time is total sequential startup through all views' delivered
query-readiness contracts; see the restart-contract caveat below.

| LF views | Condition | Samples/mode | Ready ms O / S | Query p50 ms O / S | Query p95 ms O / S |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1 | Fresh | 30 | 67.6 / 577.1 | 5.84 / 14.01 | 16.23 / 45.65 |
| 4 | Fresh | 120 | 234.4 / 1566.1 | 5.40 / 13.96 | 13.48 / 39.88 |
| 16 | Fresh | 480 | 1030.8 / 5180.9 | 5.31 / 13.74 | 13.02 / 37.62 |
| 32 | Fresh | 960 | 2051.9 / 10185.1 | 4.39 / 13.17 | 11.45 / 35.48 |
| 1 | Restart | 30 | 21.9 / 357.1 | 4.74 / 15.06 | 12.28 / 41.18 |
| 4 | Restart | 120 | 88.4 / 1263.1 | 5.08 / 13.46 | 13.73 / 42.24 |
| 16 | Restart | 480 | 352.7 / 4861.2 | 4.46 / 12.78 | 11.53 / 35.25 |
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

| Artifact | SHA-256 of gzip | SHA-256 of decompressed JSON |
| --- | --- | --- |
| Primary | `700642cc711716c73ec548e5ff9048020f9b628e70aebb40103482bfbbe39b2a` | `89ea1bc4a7c6011acaf2c9d2e121dccf22ffbc1be812b7db6c0f53aa42b58646` |
| LF scale | `24e012e5d91ba9db61c5eea2ccca50b9a0ed1579813a5e6b8c6e79404d3024c5` | `c1aa0ba6c1d3f15d3476f461deeac6674d21eadf7adf5469016dafb1aa7a297b` |
| CRLF scale | `004dddaedc4e7ee2a0e2adf762a78b88f5ce3df0966ed0dc0cfcba780d8370d4` | `77a293b17b1dd80761c67de361759b8cff068add9ae56a998614eb2f1807b387` |

Read and validate without extracting files (works on Windows and Linux):

```powershell
python -B -c "import gzip,json,sys; from pathlib import Path; sys.path.insert(0,'scripts'); import benchmark_shared as b; reports=[json.loads(gzip.decompress(p.read_bytes())) for p in Path('scripts/benchmark-results').glob('*.json.gz')]; [b.validate(r) for r in reports]; assert all(r['ok'] for r in reports); print(sum(len(r['cases']) for r in reports),'validated paired cases')"
```

To decompress one file, `python -m gzip -d <artifact.json.gz>` writes its sibling
`.json` file. Check hashes before/after decompression when independently auditing.
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
Full parameter values and host notes are in each artifact.

The frozen harness passed 14 unit tests on Windows and 13 plus one Windows-only
skip on native Linux. Actual small smokes passed all four scenarios at 1/4
worktrees: eight fresh paired cases on Windows and sixteen fresh/restart paired
cases on Linux, with strict backend/JSON/schema gates and successful cleanup.
These smokes are functional cross-platform evidence, not additional timing rows
in the Linux baseline.

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

Limits include at most 32 views, 32 MiB generated text per tree, 100 churn rounds,
and configurable per-command/readiness deadlines (`--timeout`, default 60s).
Default corpus footprint across 32 trees is roughly 16 MiB plus Git/index data;
index/process memory is additional. `--files`, `--file-bytes`, `--seed`,
`--samples-per-view`, `--churn-rounds`, `--threads` and `--idle-seconds` are recorded.
There is deliberately no real-repository clone mode or global cache manipulation.
Each fixture checks free disk against a conservative source/index reserve before
population. This does not impose a process-memory cap; check host memory before
increasing the corpus, especially for transformed private overlays.

JSON schema ID is `tgrep.shared-benchmark.v1`. Top-level fields include parameters,
binary source-commit attestation/hash/version, harness hash, host/tool versions,
metric semantics, cases, errors and cleanup. Each case includes exact fixture
revision/fingerprints, cleanliness, mode order and paired equality. Per-mode
records retain startup samples/statuses, raw query/churn/refresh/idle samples,
resource snapshots/deltas, storage growth, equality digests and cleanup/logs.
Failures still emit a partial report with `ok: false`; it must never be compared
as a successful baseline. A binary SHA-256 identifies the executable exactly;
`--binary-commit` is an explicit caller attestation, not inferred from whatever
checkout happens to contain the script.
