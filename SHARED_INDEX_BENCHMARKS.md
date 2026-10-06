# Shared-worktree performance baseline

[`scripts/benchmark_shared.py`](scripts/benchmark_shared.py) compares **one ordinary
server and index per worktree** against **one repository daemon with private
views**. This is separate from the [large-repository search benchmarks](BENCHMARKS.md).
It measures the currently implemented sharing, not a proposed cache, base
migration, garbage collector, or runtime integration.

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

**RSS sums do not demonstrate physical mmap deduplication.** Linux PSS apportions
shared resident pages and is a useful additional observation, not proof of
application-level sharing by itself. The daemon's `base_sharing_views` and build/
extraction counters describe the implemented sharing more directly. Identical LF
trees still incur initial content reads; clean CRLF trees can require private
extraction for the entire corpus. The linked `.git` pointer file is private too.
The ordinary server has a private content cache; the shared daemon currently has
none. Memory and query-time differences therefore include cache policy, process/
thread overhead and discovery costs, not only trigram reader sharing.
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
