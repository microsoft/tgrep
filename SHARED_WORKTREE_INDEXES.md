# Shared worktree index design

**Status:** end-state architecture with the shared-reader and overlay-checkpoint
foundation implemented in [PR #168](https://github.com/microsoft/tgrep/pull/168),
with [committed-tree base generations](tgrep-core/src/generations/mod.rs) and
[core worktree synchronization](tgrep-core/src/worktrees.rs) implemented.
Automatic multi-worktree CLI/server support is not implemented yet.

## Goal and ownership

An agent runtime may create many worktrees from the same repository. Rebuilding
a complete trigram index for every session duplicates indexing work, disk
storage, and memory. Instead, tgrep should share immutable base data and index
only the differences belonging to each worktree.

The boundary is: **tgrep owns search correctness; the agent runtime owns
session lifecycle.** The main checkout is another worktree with its own
overlay, not a mutable source of truth for every session.

| Responsibility | Owner |
| --- | --- |
| Shared readers, base generations, overlay precedence, persistence | tgrep |
| Git-difference discovery, watchers, reconciliation, safe scan fallback | tgrep |
| Worktree-specific filtering, filename membership, and content caches | tgrep |
| Starting/supervising the daemon and configuring resource budgets | Agent runtime |
| Attaching/releasing worktrees as sessions start/stop | Agent runtime |
| Supplying the intended starting revision and known-change hints | Agent runtime, validated by tgrep |

Runtime notifications are an optimization, not the source of truth. External
editors, Git commands, and build tools can change files independently.

## Overall architecture

Run one repository-scoped daemon, identified by the canonical Git common
directory, rather than by a remote URL or branch name. Linked worktrees share
this identity; unrelated clones do not accidentally share mutable state.

```mermaid
flowchart TB
    subgraph Runtime["agent runtime"]
        Manager["Session manager"]
        A["Agent session A"]
        B["Agent session B"]
    end

    subgraph Daemon["tgrep daemon - one per repository"]
        Router["Request router<br/>Select view by worktree identity"]
        VA["Worktree A view<br/>Pinned base B1 + private overlay A<br/>Own visibility rules and content cache"]
        VB["Worktree B view<br/>Pinned base B1 + private overlay B<br/>Own visibility rules and content cache"]
        Base["SharedBase B1<br/>One immutable IndexReader<br/>Shared postings and path table"]
        Changes["Per-worktree change tracking<br/>Git differences + filesystem watchers<br/>Reconciliation"]
        Generations["Base generation manager<br/>Build, publish, pin and reclaim"]
    end

    subgraph Disk["Filesystem"]
        FA["Worktree A files"]
        FB["Worktree B files"]
        Bases["Immutable base generations<br/>Outside disposable worktrees"]
        Overlays["Separate overlay checkpoints<br/>A and B: changes + tombstones only"]
    end

    Manager -. "start / attach / detach" .-> Router
    A -->|"search A"| Router
    B -->|"search B"| Router
    Router --> VA
    Router --> VB
    VA -->|"base candidates"| Base
    VB -->|"base candidates"| Base
    VA -->|"verify against A's contents"| FA
    VB -->|"verify against B's contents"| FB
    FA -. "changes" .-> Changes
    FB -. "changes" .-> Changes
    Changes -. "update A only" .-> VA
    Changes -. "update B only" .-> VB
    VA -. "save A delta" .-> Overlays
    VB -. "save B delta" .-> Overlays
    Router -. "request suitable base" .-> Generations
    Generations -->|"publish new generation"| Bases
    Bases -->|"open once and share"| Base
```

The daemon shares the actual `Arc<IndexReader>`, including its loaded path
table, not just the names of the memory-mapped files. Worktrees retain
independent roots, overlays, visibility/membership state, watcher queues,
and synchronization so a checkout in one worktree need not block another's
searches.

## Base snapshots and worktree overlays

A base describes an exact committed tree under a compatible indexing profile.
Its implemented key includes repository identity, Git tree OID, indexing profile,
and index-format version; retain the commit OID for diagnostics. The profile
must account for content decoding, checkout transformations, and corpus
coverage. Dirty and untracked files from the main checkout belong in its
overlay, never in the shared base.

Every worktree pins an exact base generation. Its overlay contains the
differences between that base and the worktree's current searchable contents,
not merely differences from its current `HEAD`.

| Difference | Overlay representation |
| --- | --- |
| Added or modified file | Postings for the complete current file |
| Deleted or newly ineligible base file | Tombstone hiding its base entry |
| Renamed file | Old-path tombstone and new-path entry |
| Eligible untracked file | Worktree-only entry |
| File restored exactly to the base | Remove the override and expose the base |

Use Git diffs to discover paths, not to index patch hunks. Whole-file indexing
preserves matching across edit boundaries, context lines, and line numbers.
Discovery must include committed branch divergence, staged changes, unstaged
changes, and eligible untracked files. A commit inside a session does not make
its differences from the pinned base disappear.

## Search flow

```mermaid
flowchart LR
    Q["Search in worktree A"] --> BC["Find candidates in base B1"]
    Q --> OC["Find candidates in overlay A"]
    BC --> Hide["Exclude every base path<br/>replaced or deleted by A"]
    Hide --> Union["Combine candidates"]
    OC --> Union
    Union --> Filter["Apply A's scope,<br/>visibility and query filters"]
    Filter --> Verify["Run full matcher against<br/>A's file contents"]
    Verify --> Results["Results for session A"]
```

This illustrates the existing `HybridIndex` precedence model: overlay entries
shadow base paths even when their new content does not match the query.
Candidate IDs and path resolution must use the same pinned reader snapshot,
with overlay mutations synchronized for that operation.

The base supplies candidates, not another checkout's contents. Final matching
reads the requesting worktree or its correctly versioned cache. Cache entries
must not be keyed only by relative path across worktrees. Sharing cached bytes
by content identity is a later optimization requiring verified identity.

For example, editing `auth.rs` in A hides that path's base postings only in A.
Deleting `old.rs` in A hides it with a tombstone. B still sees its own versions.
The same membership/visibility rules must apply to content searches and
`--files`, including filename-only paths without searchable postings.

## Registration, readiness, and freshness

The proposed versioned daemon API has these logical operations; names and wire
schemas are not yet a public protocol:

| Operation | Purpose |
| --- | --- |
| `attach(root, expected_head)` | Validate the worktree, select/pin a base, establish its view, and return a registration identity/readiness state |
| `search(worktree_id, ...)` / `files(worktree_id, ...)` | Route requests to the correct worktree view |
| `refresh(worktree_id, changed_paths)` | Promptly process known-change hints |
| `detach(worktree_id)` | Release the caller's registration; reclaim idle state when unused |

Normal CLI invocations should discover the same view automatically. Releasing
one session must not stop a daemon or worktree view still used by other clients.
Canonicalize and validate roots and request paths; a worktree identity is not
permission to read arbitrary paths. Protocol capability negotiation must
distinguish this service from the existing single-root server.

On attach, start capturing changes before establishing the initial delta.
Reconcile Git/filesystem state, replay queued events, and publish a consistent
base-plus-overlay view. If coverage is incomplete or change discovery fails,
scan rather than presenting a partial index as complete. A known changed path
can be verified by direct scanning while its replacement postings are built.

Keep periodic reconciliation and watcher-overflow recovery. Merely draining a
watcher queue does not prove that no filesystem notifications were missed.
An acknowledged refresh can establish that specified edits have been processed;
it is not a filesystem-wide snapshot guarantee. `--no-index` remains the escape
hatch for reading current disk contents under normal traversal rules.

Sharing removes repeated full-content indexing, not necessarily all
repository-sized startup work. Git status, untracked-file discovery, and
watcher registration may still inspect many paths. Measure these costs
separately before introducing filesystem-monitor or registration optimizations.

## When the base changes

**Never update a shared base in place.** Build and publish a new immutable
generation when a requested starting revision needs one. Deduplicate concurrent
build requests for the same compatible base.

| Event | Action |
| --- | --- |
| First session at a revision | Build a compatible base if none exists |
| Another session at the same tree/profile | Reuse the base and establish its overlay |
| Agent edits files or commits on its branch | Update only that worktree's overlay |
| New session starts from an updated `main` | Build or reuse a base for the new revision |
| `main` advances without a new session | No required work; background prewarming is optional |

```mermaid
flowchart LR
    subgraph Old["Existing sessions remain pinned"]
        B1["Base B1<br/>Committed tree C1"]
        A1["Session A<br/>Overlay relative to B1"] --> B1
        BView["Session B<br/>Overlay relative to B1"] --> B1
    end

    Advance["main advances to C2<br/>New session requests C2"] --> Build["Build B2<br/>Reuse unchanged postings<br/>Index changed content"]
    B1 -. "reuse" .-> Build

    subgraph New["New generation"]
        B2["Base B2<br/>Committed tree C2"]
        C["Session C<br/>Overlay relative to B2"] --> B2
    end

    Build --> B2
```

Building B2 should reuse compatible unchanged postings and extract only changed
content. It can still require streaming/writing a new full index generation;
avoiding extraction does not eliminate publication I/O.

Initially, do not migrate active sessions automatically. A checkout or rebase
can be represented by recomputing that worktree's overlay against its pinned
base. An explicit base migration must instead recompute the delta against the
new base and atomically publish the new base-and-overlay pair. Swapping only
the reader would give the overlay the wrong meaning.

Retain old generations until no active worktree, in-flight query, or retained
restorable checkpoint needs them. A retention policy may evict idle checkpoints
and release their pins; restoration must then rebuild/reconcile, not silently
substitute another base. Defer removal of mapped generations on Windows.

The current core manager deliberately implements **retain-all**, not online GC:
it never deletes or rewrites a published generation, even when its last
`Arc<Generation>` pin is dropped. This also protects escaped readers/views and
restorable checkpoints across process restarts. There is no deletion API or
automatic active-session migration. Reclaiming storage offline requires stopping
all users and discarding dependent checkpoints. A later daemon can introduce
ownership-aware retention and resource budgets; absence of a live in-process pin
alone is not proof that a generation is deletable.

## Persistence and compatibility

Store bases outside disposable worktree directories. Persist each worktree's
delta separately, bound to its exact base and identity. Checkpointing a session
must not materialize another complete index or merge its changes into the base.

| Concern | Required behavior |
| --- | --- |
| Ignore rules and corpus coverage | Use worktree-specific membership. A shared tracked-path superset can avoid irreversible omissions; newly admitted files absent from the base still need discovery/indexing |
| Hidden files and `--files` | Maintain worktree visibility and filename-only membership, not just content postings |
| CRLF, encoding, LFS, smudge filters | Git blobs are not necessarily checkout bytes; reuse postings only with compatible content semantics, otherwise index affected files or scan |
| Sparse checkout, symlinks, submodules, case behavior | Respect materialized paths and existing traversal semantics, not just the committed tree |
| Incompatible query options or partial coverage | Fall back to the existing scan behavior rather than omit eligible files |
| Missing daemon/checkpoint/base | Use a validated reconciled view or scan; never silently answer from the base alone |
| Restart or interrupted publication | Validate the generation/checkpoint pairing and re-establish freshness before enabling indexed queries |

The current checkpoint implementation guarantees atomic replacement visibility,
including overwriting an existing checkpoint on Windows. It syncs file contents
before replacement but does not sync the parent directory afterwards; this is
**not a power-loss durability guarantee**. Stale or missing checkpoints must be
reconciled or rebuilt.

Existing CLI behavior, RPC protocol, index format, and single-root serving must
remain available while shared mode is introduced explicitly. Do not implement
sharing by pointing today's servers at the same `--index-path`: their
publication path still writes a complete index.

## Implemented generation API

`tgrep_core::generations::Repository::discover(root)` invokes Git without a shell
and identifies the canonical native common directory. `.git` files, linked
worktrees, subdirectories and bare repositories are supported. Independent
clones do not share identity, even with equal tree OIDs and remotes.
`Repository::git_dir()` retains the discovered worktree's metadata directory so
symbolic revisions such as `HEAD` resolve there, not at another worktree's HEAD.
Ambient `GIT_*` environment overrides and replace refs are ignored; Git failures,
missing objects and unsupported native/index paths are explicit errors.

`GenerationManager::new(repository)` uses
`<common-dir>/tgrep-bases-v1/<repository-identity>/`.
`with_storage(repository, storage)` accepts an existing trusted external
directory and uses a repository-identity subdirectory. It rejects storage in
registered worktrees, Git metadata or another index snapshot. Stores and their
ancestors must not be externally renamed or modified while in use.
Checks cover the effective canonical repository-identity directory as well as
the supplied parent, including an existing worktree or snapshot at that child.

| API | Contract |
| --- | --- |
| `ensure(revision, profile, predecessor)` | Resolve an exact commit/tree, build or reuse the key; incompatible supplied predecessors error |
| `EnsureResult` | Pinned `Arc<Generation>`, requested commit, and per-request `BuildStats` |
| `open(key)` / `list()` | Open exact or list validated published generations; errors never become empty bases |
| `Generation::key()` / `commit_oid()` | Serializable exact repository/tree/profile/format/schema key; first publishing commit for diagnostics |
| `Generation::base()` | The shared `Arc<SharedBase>`; use existing worktree/overlay APIs without changing their validation |
| `entries()` / `entry(path)` | Sorted complete tracked membership, Git modes/OIDs/raw sizes and content classification |
| `TrackedEntry::matches_worktree_bytes(bytes)` | Decoded-content identity comparison only, not a freshness/visibility/eligibility claim |
| `retention()` | `RetentionPolicy::RetainAll`, including after all current pins drop |

The versioned profile is `RawGitBlobAutoV1` plus `TrackedRegularFilesV1` and a raw
blob-size cap (64 MiB by default; `None` disables it). It invokes the existing
automatic decoder, binary classifier and masked trigram extractor on raw blobs,
never checkout-filtered bytes. Ignore rules, hidden visibility and binary
extensions are deliberately not applied to this tracked-path superset.
Destination mode/size eligibility is always recomputed. Only regular-file
predecessor entries with compatible indexed content supply reusable postings.
Binary entries may reuse their classification, but cannot supply absent postings.
Empty and short text entries carry a content identity even with zero postings.
Oversized, binary, symlink and gitlink records remain distinguishable in tracked
membership; symlinks and gitlinks are not followed or content-indexed.

Clean Git status and equal blob IDs do not prove equivalence of worktree bytes.
The synchronization layer verifies decoded identities from checkout reads, or
overlays transformed/changed files. It also applies worktree-specific membership,
including ignored/extension-filtered files, sparse checkout and filesystem case
behavior. Non-UTF-8 tracked names, unsafe relative paths, and paths unrepresentable
on the current platform are explicit unsupported errors, not lossy aliases.
Native repository paths are retained losslessly. Content/coverage enum versions
must advance if their semantics change, independently of the index format.

A supplied compatible predecessor provides postings by blob identity, including
renames/copies, without reading unchanged blob contents. Changed/new blobs use
one `git cat-file --batch` process and the existing spill sorter, then publish a
complete streamed index. Memory includes one blob, its decoded text/masks,
O(tracked paths) metadata and the bounded sorter arena, not all postings.
Without a predecessor the first build extracts the full committed corpus.
`BuildStats` counts actual blob reads/bytes, extraction calls, reused indexed
files, copied postings, and whether this request published or reused a generation.
`predecessor_posting_lists_read` counts decoded predecessor lists; when no indexed
paths reuse postings, generation creation does not traverse the predecessor.
Empty/short indexed files retain their paths and content identities without
triggering that traversal. Strict snapshot opening records per-file posting
presence during its existing validation pass, including for older generations;
there is no new metadata field, format boundary, or additional posting scan.
Publication I/O and metadata enumeration are not eliminated by extraction reuse.

One OS file lock per repository store serializes cooperating processes, including
different-tree builds; it is released on process exit/crash. A weak in-process
cache shares live `Arc<Generation>` pins and their shared reader/path table.
Cold validation/fingerprinting runs outside the process-wide cache mutex, so it
does not block unrelated repository cache hits; insertion rechecks for a winner.
Staging uses unique `.stage-*` directories on the same filesystem. The complete
index is strictly opened, its fingerprint and checksummed tracked metadata
validated, and files synced before atomic directory rename to the key's name.
Final directories are never overwritten, even if corrupt. Abandoned staging
directories are ignored, not advertised or automatically garbage-collected.
Atomic visibility is not a parent-directory power-loss durability guarantee.
These checks detect inconsistent/corrupt data; storage must still be trusted,
not treated as an authenticated format for adversarially rewritten files.

The synchronization layer retains the generation pin with each view and serializes
its key inside the delta checkpoint. The generation's `SharedBase` fingerprint still
binds the checkpoint to exact index bytes. Reopening that key is not readiness:
reconcile the worktree before enabling indexed queries.

## Implemented worktree synchronization API

`tgrep_core::worktrees::WorktreeView::new(root, pin, options)` validates that the
canonical root is an actual worktree root in the pinned repository. Independent
clones, bare repositories and subdirectory roots are rejected. The exact
`Arc<Generation>`, canonical root and worktree Git directory stay with the view;
no mutable base/flush handle is exposed and no base migration occurs.

| API | Contract |
| --- | --- |
| `WorktreeOptions` | Existing `MetaWalkOptions`, bounded `hint_capacity`, optional existing private `checkpoint_directory` |
| `invalidate_path(relative)` | Close query gate; queue a file/subtree hint, including old/new rename paths; invalid input errors and forces full repair |
| `invalidate_all()` | Close gate and require full verification for startup, overflow, Git/config changes, missed events or polling uncertainty |
| `refresh()` | Rewalk membership/visibility; verify hints/new/stat-changed files or, without hints, all admitted contents |
| `reconcile_full()` | Always read/verify content, irrespective of size/mtime/Git-status equality |
| `status()` | Ready flag, invalidation epoch, published epoch, pending-path count and full-required flag |
| `with_snapshot(closure)` | Guarded query-only view with root/epoch/visibility, resolved paths and read-only candidate opens; verifies pinned-root identity before/after the callback |
| `save_checkpoint()` | Ready-only, delta-only atomic `overlay.json`, including exact generation key/root/base binding |
| `restore(root, pin, options)` | Explicit errors for missing/invalid/mismatched checkpoints; successful restore remains not ready |
| `ReconcileStats` | Actual content reads/bytes/decodes/extractions, base/overlay reuse, copied base files/postings, reads avoided and `hint_lookups` ordered-set probes |

The agent runtime subscribes **after construction and before initial refresh**,
then forwards native/polling/no-watch inputs through these same invalidations.
Each refresh makes one bounded attempt, serialized per view. Files are prepared
outside the readiness lock; invalidations can continue to arrive. Overlay,
visibility and filename membership publish together only if the captured epoch
still matches. Otherwise `ChangedDuringReconcile` leaves the view not ready with
full work pending. Retrying replays by full verification; sustained churn must
scan or wait, never spin indefinitely. Incomplete metadata walks (including
ignore-rule errors), unreadable tracked exemptions, failed reads and detected
read instability also keep the gate closed. No base-only success is possible.
Native directory/file names are validated before converting metadata-walk paths
or recording visibility. Non-Unicode names and literal Unix backslashes produce
discovery errors instead of aliasing valid or ignored paths; ordinary native-path
full scans remain the fallback.
Reconciliation holds a stable root directory handle. The extracted
`tgrep_core::rooted::RootedDir` helper (`open`, relative `open_file`, `verify_root`)
is also retained once per ordinary server and reused across indexing, watcher
and verification passes, rather than reconstructed for each file open. Unix descends with component-relative
`openat`, `O_NOFOLLOW` and `O_NONBLOCK`, then verifies the final handle is regular.
Windows holds ancestor handles without delete sharing, rejects reparse
points and validates the final handle's containment and actual parent before
reading. File version and identity are rechecked through rooted handles; a root identity change also
prevents publication. Errors leave readiness closed, including directory/link
swaps and regular-file/FIFO swaps. Windows keeps the root guard until view drop;
agent runtimes must release registrations before removing or renaming worktrees.
Ordinary serving retains its root guard for the server lifetime too: stop the
ordinary server before removing or renaming its served root on Windows.
Each reconciliation advances the epoch, including no-hint full repairs, so
successful publication acknowledges earlier invalidation tokens with an equal
or later epoch.

`WorktreeSnapshot::candidates(plan, prefix, include_hidden)` resolves both base
and live IDs before releasing the overlay guard. `files(prefix, include_hidden)`
comes from complete walker filename membership, not posting lists.
`open_file(relative)` returns a read-only regular-file handle from the view's
retained reader, never from a new registration of the root pathname.
`with_snapshot` checks pinned-root identity before and after every callback,
including empty/file-only queries; failure invalidates readiness and queues full
reconciliation. Candidate-open failures are latched and returned as an outer
I/O error, with readiness invalidated before releasing the guard even if the
callback swallowed the error or subsequently opened another file successfully.
A callback may return the owned handle for bounded matching
outside the guard; reenter `with_snapshot` and verify the original epoch before
publishing buffered results. File contents are not frozen by these handles.
Later handle-read errors remain caller-owned: report them and call
`invalidate_all()` after leaving the guard, without publishing partial results.
Hidden files
are included in canonical coverage and filtered at query time, including Windows
attributes and explicit hidden-directory scopes. Existing walker rules determine
ignore/extension, materialized symlink/submodule and regular-file behavior.
Missing, sparse and ineligible base paths are tombstoned; staged, unstaged,
committed-divergent and eligible untracked files use whole-file overlays. Restoring
the pinned content removes the override; committing divergent content does not.

Full passes walk without a metadata-only size cutoff, then bound actual reads to
the configured limit plus one byte and classify the bytes read. Eligible content
is auto-decoded and hashed before reuse, so CRLF, smudge, encoding, assume-unchanged
and skip-worktree cases cannot rely on clean status or index stat data. Verified
content-identical copies/renames can populate new overlay paths by copying base
masks in one streaming posting pass. Matching existing overlays keep their masks.

This saves trigram extraction, **not all repository-sized startup work**. Every
refresh walks metadata and membership. Full passes read/verify all admitted
content; hinted passes can retain previous evidence for unaffected paths under
the explicit event-driven freshness contract. Case-alias hints conservatively
reverify; non-ASCII hints and capacity overflow require full verification.
Accepted hints are rebuilt from normal path components, so trailing/repeated
separators and interior `.` spellings cannot lose subtree invalidations. Absolute,
parent-component and leading `.` component hints still error and require full repair.
Hint matching probes a `BTreeSet` for the normalized full path and ancestor
prefixes, rather than scanning every hint for every file. `hint_lookups` counts
these probes (at most path depth per file, each logarithmic in queue size);
the regression uses 2,001 pending hints but only five probes for two files.
Hinted passes still open eligible regular-file handles for safe metadata
verification even when content reads are avoided.
Same-size/restored-mtime edits without notifications are repaired by forced
full checks, not promised by hints. Successful refresh acknowledges processed
inputs, not an atomic filesystem snapshot. Final matching uses
`WorktreeSnapshot::open_file` or a private versioned cache, not an ordinary read
of `snapshot.root().join(path)` that could follow a raced link outside the
worktree. Contents can still change through later writes to the opened file.

Raw LF blobs and clean CRLF/smudge checkouts may differ for **every file**.
Normalizing line endings would invalidate positional/next-byte masks. Fixtures
assert three identical tracked files are read/decoded with zero extractions,
versus four clean transformed tracked files requiring four private extractions.
The linked worktree's plain `.git` pointer file adds one private extraction
in either case; later unchanged passes reuse all private overlays.
No transformed-content cross-view cache is
implemented. Preparation retains O(paths) metadata plus changed-file masks
until publication, in addition to the existing private overlay. A large
transformed checkout can therefore require substantial private memory.

Checkpoints use the foundation's single directory-bound atomic publication;
there are no unprotected auxiliary manifest writes. A configured private
checkpoint path must be an existing directory; construction, restoration and
saving reject regular-file paths instead of accepting an unusable view.
The checkpoint directory is excluded in its entirety (including staging files),
along with `.tgrep`, Git metadata directories, generation storage and caller-supplied
`walk.exclude_paths`. It cannot be an index snapshot, Git metadata or a worktree
ancestor. Storage must remain trusted and not externally renamed while in use.
The plain `.git` pointer in a linked worktree follows ordinary walker membership:
its filename and content are available with hidden inclusion, subject to ignore
and size rules. The named Git metadata directories remain excluded; the file's
contents are not followed as a filesystem link.
Restoration validates the exact generation key, base bytes and canonical root but
does not restore readiness or trusted read evidence; first reconciliation may
re-extract restored private postings. Missing/stale/invalid state is an explicit
recoverable error, not an empty overlay. Bases remain immutable and retain-all.

This core layer does **not** own native watchers, automatic shared CLI serving,
daemon routing/wire schemas or content caches. Existing CLI/server behavior is
unchanged; opt-in integration is the next layer.

## Implementation status and rollout

The implemented increments are core-library APIs, not automatic shared indexing:

| Surface | Current status |
| --- | --- |
| [`SharedBase`](tgrep-core/src/shared.rs) | Opens a validated reader once; creates independent worktree-rooted views sharing its `Arc` |
| [`HybridIndex`](tgrep-core/src/hybrid.rs) / [`LiveIndex`](tgrep-core/src/live.rs) | Reuses existing candidate merging, replacements, and tombstones |
| Overlay checkpoints | Atomic delta-only save/restore; validate format, root, paths, and exact base fingerprint |
| Base identity | Index fingerprint plus repository/tree/profile/format/schema generation key |
| Base immutability | Generation manager stages/validates/publishes once; external callers must not mutate mapped files |
| [`Regression coverage`](tgrep-core/tests/shared_worktrees.rs) | Sharing/isolation, masks, tombstones, restoration, compatibility, repeated saves, and Windows failure recovery |
| [`Generation management`](tgrep-core/src/generations/mod.rs) | Canonical common-dir identity, raw committed-tree builds, incremental posting reuse, cross-process deduplication, pins and retain-all |
| [`Generation coverage`](tgrep-core/tests/generations.rs) | Temporary repositories/worktrees, thread/process races, content transformations, immutable old readers, errors and interrupted publication |
| [`Worktree synchronization`](tgrep-core/src/worktrees.rs) | Pinned private views, actual-content verification, atomic readiness/membership, bounded invalidations, full repair and bound checkpoints |
| [`Synchronization coverage`](tgrep-core/src/worktrees/tests.rs) | Real divergent worktrees, scan/candidate parity, CRLF/smudge/decoding, sparse/assume-unchanged, filtering, epochs, errors and extraction counts |
| Shared CLI/daemon routing and native watchers | Follow-up work; not yet implemented |

Shared-base validation rejects mismatched empty lookup/posting sections and
metadata counts inconsistent with the opened sections, while allowing empty
indexes and short files. Shared snapshots require aligned, contiguous posting
ranges covering the entire postings section, valid trigrams, strictly
increasing valid file IDs in each posting list, and nonzero location masks.
Checkpoint destinations reject trailing separators and
current-directory suffixes, and exclude the base and its
descendants by directory identity, even if the base has been renamed. Unix
staging, replacement, and cleanup are relative to an open parent-directory
handle; Windows holds non-delete-sharing handles on the canonical parent and
all its ancestors until publication completes. A renamed/replaced pathname
cannot redirect publication into the base. Root identities preserve the existing
JSON string encoding for Unicode paths and use tagged platform-native units for
non-Unicode Unix/Windows paths; older readers reject the latter representation.

Merge the foundation independently once its normal review and checks are
satisfied; do not expand it into the entire feature. Keep follow-up work in
reviewable increments, each with its own correctness coverage:

1. **Base generations (implemented):** Git tree/profile identity, immutable
   build/publication, incremental reuse, pins and conservative retain-all.
2. **Worktree synchronization:** complete delta discovery, private membership,
   watchers/reconciliation, checkpoint recovery, and readiness gating.
3. **Daemon and integration:** worktree registration/routing, CLI discovery,
   versioned agent runtime integration, resource budgets, and scan fallback.

Before enabling automatic shared mode, verify isolated results against scans
for divergent branches, edits/deletes/renames, ignore changes, checkout
transformations, sparse worktrees, watcher loss, and restarts. Exercise existing
single-root clients and formats on supported platforms. Measure content
extraction, startup work, memory, and publication I/O separately.

Keep the first implementation two-layered: one shared base plus one private
overlay per worktree. Content-addressed caches, shared branch intermediates, and
automatic base migration can follow only if measured workloads justify them.
