# tgrep for coding agents

A short guide for AI agents (and the humans who wire them up) that want to
use `tgrep` as a fast search tool inside a repository. It complements the
[README](README.md), which documents every flag; this file covers the few
things an agent has to get right.

## The mental model

tgrep is ripgrep with a pre-built trigram index and an optional server.

```
tgrep index .        # once: build the index into ./.tgrep
tgrep serve .        # once per session: keep the index warm and watch for changes
tgrep "pattern" .    # every search: finds the server, answers in milliseconds
```

A search resolves in this order:

1. **Server** running for this tree: query it over TCP. Fastest. A file
   watcher keeps the index close to the filesystem, though a silently missed
   notification is repaired only by periodic reconciliation (scheduled hourly
   and deferrable for up to four hours while queried); `--no-watch` disables it.
2. **On-disk index** but no server: read `.tgrep/` directly. Fast, but only as
   fresh as the last successful index publication. Legacy indexes without
   hidden-file coverage metadata and incomplete builds fall back to scanning.
   Rebuild with `tgrep index .` or resume `tgrep serve .` to enable indexed queries.
3. **No index**: scan every file, like grep. Correct but slow on large trees.
   tgrep prints a warning on stderr when this happens.

An agent never has to choose between these; the command is the same. Results
can differ, though. An on-disk index omits changes since its last build. A
server started with no index, a partial index, or legacy hidden-file coverage
lets clients fall back to scanning until it establishes the full corpus.
`tgrep status .` shows `Hidden coverage: complete` once indexed queries are
available, and `Indexing: complete` once the initial build is done. Do not read either as a
freshness signal: a server that starts on an existing index reconciles it
against the filesystem in the background while already reporting complete,
and it never covers watcher events missed later. When a search must reflect current file
contents, pass `--no-index`. It reads every eligible file from disk instead of
consulting the index, still applying the normal ignore, hidden-file, binary
and size rules. It is slow on large trees, so use it deliberately.

## Setup

```bash
brew install tgrep                      # macOS, Linux
cargo install --path tgrep-cli --locked # from a checkout
```

Then, from the repository root, once:

```bash
tgrep serve . &
```

`serve` builds the index if none exists and answers queries while it builds.
It writes `.tgrep/serve.json` (PID and port) so clients can find it. Do not
commit `.tgrep/`; add it to `.gitignore`.

Check that a server is up:

```bash
tgrep status .
```

### Shared daemons: managed v2 recommended

For new linked-worktree integrations, start **one opt-in managed v2 repository
daemon** with `tgrep serve --shared <root> --shared-storage <existing trusted
external directory> --shared-policy <policy-file>`. Supply a complete
[managed policy](SHARED_WORKTREE_INDEXES.md#managed-policy) and follow the
[managed startup example](README.md#managed-shared-lifecycle-opt-in).
It runs in the foreground; keep it supervised. Managed v2 remains an explicit
opt-in: omitting `--shared-policy` still starts deprecated v1.

Follow the [managed lifecycle contract](SHARED_WORKTREE_INDEXES.md#managed-client-contract).
Use `shared manage` to negotiate capabilities and effective policy, persist
operation/lease tokens, register an instance-bound owner guard, and attach a
versioned view. `shared owner-hold --claim <issued-claim.json>` retains ownership
until stdin closes; supervise that process too. Do not replace its guard with a
PID check or inactivity timeout. Unknown ownership proof protects the lease.
Before `holding:true`, a typed claim failure is one `{"ok":false,"error":...}`
stdout line plus exit 2. Retry the same claim only for retryable
`not-committed` failures after the failed helper exits.
Normal queries discover a ready managed attachment without new search flags.
Managed RPC also requires a per-daemon private credential; the CLI loads it
automatically from an owner-only file. Public namespace/instance IDs are not
credentials. Do not copy the private file into logs or bypass authentication.

Managed advancement is version-checked and may be adaptive under explicit
policy. Recover the authoritative current exact commit/version after a lost
response or restart, rather than replaying the original starting commit as the
current pin. New instances require new ownership and lease tokens. Detach all
leases and wait for reader/root-handle drain before deleting the worktree.
`shared discover` and `shared maintenance` inspect external namespaces even
after Git is gone; collection requires explicit authorization and current
versions. Never delete a cache directory as a substitute for managed collection.
Legacy/unmanaged v1 storage remains retain-all.

Managed attachments use bounded parallel verified-read batches without raising
namespace operation-worker limits. Inspect `last_reconcile.timings` and its
read/reuse/resource counters to distinguish queueing, discovery, verification,
catalog/control and checkpoint work. Managed views are eventually consistent:
after their first publication they report `serving: true` and keep answering
queries from the last published snapshot while later edits are reconciled, so
results may lag recent edits. `ready: true` additionally means the view has
caught up with every observed invalidation. Healthy native managed views also get a
serving metadata verification after at least `--poll-interval` seconds (default
120) without reconciliation. It rewalks membership and reads only changed,
hinted or unverified files. Full content rereads wait at least 15 intervals
(30 minutes by default). Each tier also waits at least 19 times its previous
pass cost, so these are minimum delays, not guaranteed freshness bounds.
`ready` closes only when verification finds a difference. A deadline or
cancellation leaves the view ready but unverified (`last_verification.outcome`
`failed`/`cancelled`) and is retried with interval/cost backoff.
`views.refresh` explicitly verifies all admitted contents; use `--no-index`
when current disk bytes are required.
`--watch-mode poll` and actual native-watcher failure/budget fallback instead
close `ready` for their passes, using the same metadata/content tiers and delays.
`watch_events` in status counts watcher input by kind, metadata-only
notifications filtered as unchanged, invalidated paths and epoch advances.
`reconciliation_policy` distinguishes configured mode from each view's effective
polling and reports namespace monitor counts. `last_verification` reports the
tier and effective delays; `last_reconcile.metadata_reused` counts files not
reopened after the metadata walk. First attachment/restore and
reported watcher errors/rescans retain full verification. A clean Git status
never replaces those reads, even for another worktree at the same commit.

Moving an existing v1 integration to managed v2 requires this client lifecycle,
not just a startup flag. Detach all v1 leases and drain in-flight operations
before stopping v1, then establish new managed ownership and tokens.
Use `shared manage` / `views.attach`, not v1 `shared attach`, for managed leases.
Enabling v2 does not migrate or authorize collection of legacy storage.

### Legacy v1 (deprecated, still supported)

Shared daemon v1 is **deprecated as of tgrep 1.2.1**, not removed.
No-policy startup emits one deprecation warning on stderr; stdout and protocol
JSON remain unchanged. Existing clients and the fixed-pin/retain-all contract remain
supported. Use managed v2 above for new integrations; the following is legacy
compatibility guidance, not a change of default.

For linked-worktree sessions, an agent runtime can instead start **one opt-in
legacy v1 daemon** with `tgrep serve --shared <root> --shared-storage <existing
trusted external directory>`. It runs in the foreground; keep it supervised.
Choose and retain a unique caller-owned token before calling `tgrep shared attach
<worktree> --revision <starting-commit> --lease <token>` per session. Retry the
same token/root/revision after response loss; it returns the original attachment
without creating another lease. You can also detach that token without receiving
the first attach response. Different sessions use different tokens, even when
they share a worktree. Tokens contain 1-128 ASCII letters, digits, hyphens or
underscores. Omitting `--lease` prints a generated token to stderr before sending;
capture it and supply it explicitly on retry.
Wait for `tgrep status <worktree>` to report
`"ready": true`. Ordinary queries and `--files` then discover that attached view
without changing every search command. No attachment means the legacy behavior
above; a stale attachment means a diagnostic and a filesystem scan, not use of
an ordinary stale index.

After known edits, use `tgrep shared refresh <worktree> --lease <token> --changed
<relative-path>` (repeat `--changed` for multiple paths, including both rename
paths), or `--full` for missed-event repair. The returned `processed_epoch`
acknowledges those changes, not every concurrent filesystem write. Native/poll
refresh defaults to full verification every 120 seconds after completion;
`serve --shared --no-watch` requires runtime refresh after the initial pass.
Use `--no-index` whenever the current disk bytes must be searched.
On session teardown call `tgrep shared detach <worktree> --lease <token>`.
Independent leases cannot detach each other. Release all leases and finish
in-flight operations before removing a worktree; Windows root handles prevent
removal while its view is live. Restart requires fresh attach calls
with the intended revision; old leases are invalid. Bases stay pinned and
checkpoints remain outside disposable worktrees. CRLF/smudge differences can
require full private overlays, and initial verification still reads content.
A linked worktree also indexes its ordinary `.git` pointer file in the private
overlay; it is visible with `--hidden`, unlike a real Git metadata directory.
See [shared mode](README.md#shared-repository-daemon-opt-in) for budgets, profile
compatibility and retain-all storage. The runtime owns process lifetime and
offline cleanup for this default v1 mode; its leases do not expire and its
published cache is not collected.

### Without a supervised server

If your agent framework cannot keep a background process alive, skip `serve`
and run `tgrep index .` instead. Searches then use the on-disk index. That
index is not updated by searches or edits, so re-run `tgrep index .` after any
change a later search has to see, including your own edits.

## Searching

The command line follows ripgrep. The common `rg` flags are supported with the
same names; an unsupported flag is rejected with an error rather than ignored.
The full list is in the [README](README.md#cli-flags). Three accepted flags
only take effect on a full scan: `-L`/`--follow`, `--one-file-system` and
`--ignore-file`. An indexed search ignores them silently, so pair them with
`--no-index`.

```bash
tgrep -- "fn parse_config" .                 # regex, default
tgrep -F -- "Vec<Option<T>>" .               # literal string
tgrep -w -t rust -- handle .                 # whole word, Rust files only
tgrep -g "src/**" -C 2 -- "TODO|FIXME" .     # glob scope, 2 lines of context
tgrep -l -- "impl .* for Server" .           # file names only
tgrep -c -- deprecated .                     # count per file
tgrep --files -t py .                        # list searchable Python files
```

Rules of thumb for agents:

- **Put `--` before the pattern** and pass the search root explicitly. Shell
  quotes do not stop the parser from reading a bare `index`, `serve`,
  `search`, `shared`, `status`, `count-files` or `help` as a subcommand;
  `tgrep -- serve .` searches for the word. Everything after `--` is read as
  the pattern and paths, so all flags must come before it.
- **Prefer `-F`** when the query is a symbol or a string the user typed. It
  avoids regex-escaping mistakes.
- **Narrow with `-t` or `-g`** before adding `-m`. Both keep compatible indexes
  in use; globs filter the indexed corpus rather than reinclude ignored files.
  `-m` only trims output.
- **Use `-l` first** on a broad query, then search the specific files. This
  keeps output small.
- **Use `-C 2` or `-C 3`** when you need to read the surrounding code.
- **Use `-q`** when you only need a yes/no answer; read the exit code.

## Machine-readable output

`--json` emits one JSON object per line, in ripgrep's format. Record types
are `begin`, `match`, `context`, `end`, and `summary`.

Real output, run from a checkout of this repository:

```bash
tgrep --json -F -- "fn main" tgrep-cli/build.rs
```

```json
{"data":{"path":{"text":"tgrep-cli/build.rs"}},"type":"begin"}
{"data":{"absolute_offset":400,"line_number":11,"lines":{"text":"fn main() {\n"},"path":{"text":"tgrep-cli/build.rs"},"submatches":[{"end":7,"match":{"text":"fn main"},"start":0}]},"type":"match"}
{"data":{"binary_offset":null,"path":{"text":"tgrep-cli/build.rs"},"stats":{"bytes_printed":260,"bytes_searched":1775,"elapsed":{"human":"0.000086s","nanos":86400,"secs":0},"matched_lines":1,"matches":1,"searches":1,"searches_with_match":1}},"type":"end"}
{"data":{"elapsed_total":{"human":"0.002651s","nanos":2651400,"secs":0},"stats":{"bytes_printed":260,"bytes_searched":1775,"elapsed":{"human":"0.002651s","nanos":2651400,"secs":0},"matched_lines":1,"matches":1,"searches":1,"searches_with_match":1}},"type":"summary"}
```

Any parser written for `rg --json` works as is. As in ripgrep, `lines.text`
keeps each line's own terminator (`\r\n` for CRLF files), and a line that is not
valid UTF-8 is emitted as base64 `lines.bytes` with submatch offsets into those
raw bytes. Paths are always `path.text`.

`--vimgrep` gives `file:line:col:text`, one row per match, which is the
easiest format to feed into a "jump to location" step.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | At least one match |
| `1` | No match |
| `2` | Error (unreadable path, bad regex, ...) |

A match plus an error yields `2`, unless `-q` is set, which yields `0`. Same
as ripgrep.

## When the index is not used

These fall back to a full scan even with a server running:

- `--no-ignore` and variants, `-u`/`-uu`/`-uuu`
- `-a`/`--text`, `--binary`, non-`auto` `-E`/`--encoding`
- `--content-eligibility content`, unless a managed v2 view was attached with
  the `content-only` profile
- `--no-index` (explicit)
- naming a single file instead of a directory

Avoid these on large repositories unless you need them.

`index` and `serve` include hidden, non-ignored files by default; passing
`--hidden` to either is redundant. Queries without `--hidden` still apply normal
hidden-file filtering, including Windows hidden attributes. `--hidden` searches
and `--files --hidden` use compatible local/server indexes without disabling
ignore rules. Git's `.git` directory subtrees are excluded from indexes by
default, even with `--hidden`; use `--no-ignore` consistently on `index` and
`serve` to include them. Explicit `--no-index` scans retain normal hidden and
ignore behavior and can inspect Git internals without rebuilding the index.
Positive and negative `--glob`/`--iglob` patterns also stay
indexed, for both content searches and `--files`. They filter the indexed
corpus only, so a positive glob does not reinclude ignored files in indexed
mode. Use `--no-index` when those matches are needed; filesystem scans retain
ripgrep-style glob overrides. Negative globs such as `--glob '!.git'` exclude
entire matching directory subtrees.
The configured index directory and all its staging/retired generations are
excluded from indexing, watcher processing, and query filesystem walks, even
with a custom path.

Legacy or incomplete indexes, and older servers that cannot confirm hidden-file
coverage, fall back to scanning. A current server upgrades legacy coverage by
reconciling the tree; `tgrep index .` can rebuild it explicitly.

New indexes have a format boundary that older local readers reject instead of
exposing hidden paths without filtering. Downgrading requires rebuilding with
the older binary, preferably using a separate `--index-path`. New readers retain
legacy-format support for migration.

## Freshness

- With a **server**, results reflect the last watcher event the indexing
  worker has processed. Events are queued and applied asynchronously, so a
  search issued right after an edit can run before the index has caught up.
  Use `--no-index` when the very latest edit must be visible.
- With only an **on-disk index**, results reflect the last `tgrep index`.
  Files created since then are not found. Run `tgrep index .` again, or
  start `tgrep serve .`.
- `tgrep --files` reads from the index too. Add `--no-index` to list what is
  on disk right now.

## Keep `index`, `serve` and search flags aligned

Some flags describe the index, so `index`, `serve` and search have to agree on
them. If they differ, the client either cannot find the server or silently
searches a different set of files.

- `--exclude <DIR>`: `index` and `serve` only. Use the same value on both.
- `--no-ignore`: `index` and `serve` must agree. A server started without it
  on an index built with it treats the ignored files as deleted and drops
  them. Passing it to a search is different: that forces a full scan.
- `--index-path`, `--max-filesize`, `--no-max-filesize`, `--no-require-git`:
  `index`, `serve` and every search. An index built with
  `--no-max-filesize` but searched with the default cap hides every file
  above 64 MiB.

```bash
tgrep index . --index-path /tmp/idx --exclude vendor
tgrep serve . --index-path /tmp/idx --exclude vendor
tgrep "pattern" . --index-path /tmp/idx
```

## Repositories without `.git`

tgrep indexes plain directories normally, but like ripgrep it ignores
`.gitignore` files outside a Git repository. The index is then larger than
expected, and tgrep prints a warning saying so. Pass `--no-require-git` to
`index`, `serve` and search to apply the ignore rules anyway.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `warning: no index at ... - scanning every file` | No index at the path the search looked in | If a server or index uses `--index-path`, pass the same value to the search; otherwise run `tgrep index .` or `tgrep serve .` |
| `Server unreachable, falling back to local index` | Server died or `serve.json` is stale | Restart `tgrep serve .` |
| A new file is not found, no server | On-disk index predates the file | Run `tgrep index .` |
| A new file is not found, server running | First build still in progress, or the watcher event is still queued | Wait, or pass `--no-index` for this search; re-running `tgrep index .` does not update a running server |
| Search is slow despite a server | Flag bypasses the index (see above) | Drop the flag or scope with `-g`/`-t` |

## Tool definition sketch

If you expose tgrep to a model as a tool, a minimal schema is:

```json
{
  "name": "tgrep",
  "description": "Fast regex search over the repository. ripgrep-compatible flags. Use -F for literal strings, -t/-g to scope, -l for file names only, -C N for context.",
  "parameters": {
    "type": "object",
    "properties": {
      "pattern": {"type": "string", "description": "Regex, or literal string with -F"},
      "path": {"type": "string", "default": "."},
      "flags": {"type": "array", "items": {"type": "string"}}
    },
    "required": ["pattern"]
  }
}
```

Before invoking tgrep, canonicalize `path` beneath the configured repository root
and reject paths that resolve outside it. Allowlist search-only flags rather than
forwarding arbitrary tokens. Then run `tgrep <flags...> -- <pattern> <path>` and
return stdout, stderr and the exit code together. Treat `1` as "no results", not
as a failure. Treat `2` as an error; stderr carries the cause, such as a bad regex.
Always return stderr: the "no index" warning can arrive with code `0` or `1`.
