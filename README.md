# tgrep

Trigram-indexed grep with a client/server architecture for fast regex search
in large codebases.

**tgrep is integrated into [GitHub Copilot CLI](https://github.com/github/copilot-cli)
to power fast grep searches across large repositories.**

## Quick start

[Install tgrep](#installation), then start a server:

```bash
tgrep serve .           # builds the index if needed; runs in the foreground
```

In another terminal:

```bash
tgrep -- "fn main" .    # automatically connects to the server
tgrep status .          # show indexing and refresh status
```

Without a server, run `tgrep index .` to build an on-disk index. Searches use
the server when available, then the local index, or scan the filesystem if no
usable index exists. Indexes are stored in `.tgrep/` by default; add this
directory to `.gitignore`.

**Indexes can lag filesystem changes.** The server updates asynchronously;
without a server, rerun `tgrep index .` after changes. Use `--no-index` when a
search must read the current files. Queries scan during an initial build until
complete coverage is available.

For coding agents, see [AGENTS.md](AGENTS.md). To install MCP search tools and
startup hooks for Codex or pi, run `bash install-agent.sh` from this checkout
(Linux/macOS, Python 3.11+). See the
[agent integration guide](scripts/agent/README.md).

## Performance

A trigram index narrows each query to files that could match, then verifies
those files with the regex engine in parallel. Patterns without useful
trigrams may still require searching every indexed file.

In the August 24, 2026 benchmark sweep, tgrep was faster than ripgrep in 17 of
18 repo/platform combinations, with speedups from 0.93x to 51.9x. These measure
client/server search latency with the index already built, not indexing time.
Results depend on the query, repository, storage, and match volume. See
[BENCHMARKS.md](BENCHMARKS.md) for measurements and methodology.

## Usage

### Build the index

```bash
tgrep index .                        # index current directory
tgrep index /path/to/repo             # index a specific repo
tgrep index . --index-path /tmp/idx    # custom index location
tgrep index . --exclude vendor --exclude third_party  # skip directories
```

Each build reports elapsed time and, where available, peak memory:

```
Index built successfully at /tmp/idx
Indexed in 22.6s using external strategy (peak memory 160.1 MiB)
```

Windows reports peak private committed bytes; Linux samples anonymous resident
memory. Both exclude file-backed mappings and report the working set separately
when it is substantially larger. macOS reports resident memory instead.

#### Repositories without a `.git` directory

Like ripgrep, tgrep applies `.gitignore` only inside a Git repository. It warns
when a root `.gitignore` is present but not applied. Use `--no-require-git` on
indexing, serving, and searching to apply those rules outside Git:

```bash
tgrep index . --no-require-git
tgrep serve . --no-require-git
tgrep --no-require-git -- "pattern" .
```

#### Case-insensitive repositories

When Git's `core.ignorecase` is enabled, tgrep applies an additional
case-insensitive exclusion pass for the repository's root `.gitignore` and
`.git/info/exclude`. Git-tracked paths are exempt from this additional pass,
but not from ordinary traversal, binary, or size filtering.

Nested `.gitignore` files and global ignore rules do not get this additional
pass. `--no-ignore` disables it along with the normal ignore rules.

#### Keep `index` and `serve` flags in step

Use the same `--no-require-git`, `--no-ignore`, `--max-filesize` (or
`--no-max-filesize`), and `--exclude` settings for `index` and `serve`.
Startup reconciliation removes indexed files excluded by the server's current
settings. For example, serving an uncapped index with `--max-filesize 8M`
removes files larger than 8 MiB from that index.

Pass the same size policy, `--no-require-git`, and any custom `--index-path`
to searches too. `--exclude` is only available on `index` and `serve`;
`--no-ignore` on a search forces a filesystem scan.

```bash
tgrep index . --max-filesize 8M
tgrep serve . --max-filesize 8M
tgrep --max-filesize 8M -- "pattern" .
```

#### Memory use on very large repos

`tgrep index` defaults to an external merge sort with a 64 MiB posting buffer.
When the buffer fills, sorted segments spill to disk and are merged into the
index. This bounds posting-buffer memory, not total process memory: file
tables, worker buffers, and decoded content also consume memory.

```bash
tgrep index .                          # external, 64 MiB posting buffer
tgrep index . --index-buffer 16        # smaller buffer, lower peak
tgrep index . --index-strategy=memory  # sort entirely in RAM
```

`--index-buffer` is in MiB and applies only to the external strategy.
`--index-strategy=memory` avoids temporary spill files but holds all postings
in RAM. Both strategies still need a writable index directory.

`tgrep serve` uses the external builder with the default buffer for new
indexes; it does not accept `--index-strategy` or `--index-buffer`.
Large files can be memory-mapped, but files needing decoding or UTF-8 repair
require heap buffers. Use `--max-filesize` to limit admitted file sizes.
See [index-build benchmarks](BENCHMARKS.md#index-build-strategies).

### Start the server

```bash
tgrep serve .                         # auto-build index if missing
tgrep serve . --index-path /tmp/idx   # custom index location
tgrep serve . --watch-mode poll       # poll without native subscriptions
tgrep serve . --poll-interval 60      # polling cadence after fallback
tgrep serve . --watch-budget 4096     # lower this process's native watch ceiling
tgrep serve . --no-watch              # disable all automatic refresh
tgrep serve . --exclude node_modules  # exclude directories from indexing
```

The server builds the index in the background if none exists and resumes
incomplete builds. Clients scan the filesystem until the full corpus is ready;
`tgrep status` reports indexing progress and hidden-file coverage. Legacy
indexes are upgraded by startup reconciliation. Multiple clients can connect
simultaneously.

Index builds and server refreshes exclude `.git` directory subtrees by default,
including nested repositories' Git internals. `--hidden` does not override this;
use `--no-ignore` consistently on `index` and `serve` to include them. Explicit
filesystem scans (`--no-index`) retain ripgrep-style hidden and ignore behavior.
Git metadata needed for ignore rules and tracked-file detection is still read.

On Windows, replaced index generations stay in `.retired` while readers have
them memory-mapped. Cleanup retries after publication, every minute, and on
startup; uncommitted backups are preserved for recovery. This does not reclaim
storage already stranded in NTFS's `$Deleted` namespace by older versions.

These tuning options apply only to `tgrep serve`:

| Flag | Default | Effect |
|------|---------|--------|
| `--max-memory <MB>` | 50% of RAM (512 MiB–16 GiB) | Overlay flush threshold for resumed partial builds and fallback in-memory builds; not a process-wide hard limit |
| `--max-cpu <PERCENT>` | `50` | Size the worker pool for resumed/fallback builds and stale-delta builds as a share of logical cores, with at least one worker |
| `--auto-save-mutations <N>` | `5000` | Pending content mutations that trigger a background save |
| `--watcher-queue-cap <N>` | `16384` | Buffered filesystem events; overflow triggers reconciliation |

Fresh external builds use the default 64 MiB posting buffer and global Rayon
pool, not `--max-memory` or `--max-cpu`. If external bootstrap fails, the server
falls back to an in-memory build where these settings apply.

The server checks for pending saves once a minute. It saves at the mutation
threshold, when filename-only membership changes, or when content changes
remain unsaved for at least ten minutes since startup or the last successful
save. Active builds and flushes defer this check.

Startup watcher traces count filesystem notifications, not confirmed content
edits. Replay re-reads notified files and avoids content-index mutations when
their decoded contents match the indexed fingerprint. Delta-save traces report
the unique candidate paths plus separate scan and live-overlay counts; these
inputs can overlap and do not represent a count of user edits.

#### Staying in step with the filesystem

Automatic refresh has two modes, configured on `tgrep serve`:

| Flag | Default | Effect |
|------|---------|--------|
| `--watch-mode <auto\|poll>` | `auto` | Prefer native notifications with polling fallback, or use polling only |
| `--poll-interval <SECONDS>` | `120` | Wait after each polling reconciliation completes; range 1-86400 |
| `--watch-budget <N>` | `8192` | Conservative process-local native watch ceiling; range 1-4294967295 |
| `--no-watch` | off | Disable all automatic refresh: native watching, polling, and periodic reconciliation |

In `auto` mode, a watch-budget or native-registration failure releases this
process's watches and switches it to polling until restart. Status reports the
reason. Explicit `poll` mode creates no native subscriptions.

On Linux/Android, the budget counts admitted directories; ignored subtrees do
not consume watches. The OS inotify quota is shared with other processes, so
registration can fail before this budget is reached. Raising `--watch-budget`
does not raise the OS quota. Windows and macOS use one recursive root
subscription and filter ignored events after delivery.

Polling checks filesystem metadata for additions, changes, and deletions.
The next poll waits `--poll-interval` seconds after the previous reconciliation
finishes; scan time and ongoing builds/saves add to the delay. Queries do not
defer polling. Queue overflows and native rescan notifications also request
reconciliation; read-only access events are discarded.

Native mode reconciles about once an hour to recover missed notifications.
It waits for a two-minute gap in queries, deferring no longer than four hours.
`--poll-interval` does not change this native safety cadence.

No-change scans do not rewrite the index; changed merges may rewrite it.
Metadata-preserving changes can be missed, and reconciliation is not an atomic
filesystem snapshot. Failures appear in status. Use `--no-index` for current
file contents.

`--no-watch` permits the initial build/startup reconciliation but disables later
refresh. It conflicts with explicitly supplied `--watch-mode`, `--poll-interval`,
and `--watch-budget`. `--watch-mode poll` also rejects an explicit
`--watch-budget`.

### Search

```bash
tgrep -- "pattern" .                    # regex search
tgrep -- "pattern" file1.rs file2.rs     # multiple files/paths
tgrep -- "TODO|FIXME" .                 # alternation
tgrep -- '\w+(?!_test)' .               # backtracking-engine fallback
tgrep -i -- "error" .                   # case-insensitive
tgrep -F -- "Vec<T>" .                  # literal string
tgrep -l -- "MyStruct" .                # filenames only
tgrep -c -- "pattern" .                 # matching lines per file
tgrep -m 5 -- "pattern" .               # at most 5 matching lines per file
tgrep -g "*.rs" -g "*.toml" -- "pattern" .  # multiple globs (OR)
tgrep -t rust -C 3 -- "pattern" .       # Rust files, 3 lines of context
tgrep -e "pattern" -e "also_this" .     # multiple patterns (OR)
tgrep --json -- "pattern" .             # JSON stream
tgrep --vimgrep -- "pattern" .          # editor jump targets
tgrep --stats -- "pattern" .            # query plan and timing
tgrep --no-index -- "pattern" .         # read current files, bypass index
tgrep -U -- 'first\nsecond' .           # multiline match
tgrep -q -- "pattern" .                 # exit code only
tgrep --files .                        # list admitted paths, including binary files
tgrep --files -t rust .                # list Rust files
tgrep --type-list                      # show file types
```

With the default traversal rules, `--files` reads the live server or the local
index instead of walking the repository. The local result is an index snapshot;
use `--no-index` to inspect the filesystem as it exists now. Flags that change
traversal membership or the file-size policy also fall back to a walk.

### Check status

```bash
tgrep status .
```

```
Server status for /src/my-monorepo
  PID:        37980
  Port:       51043
  Files:      152
  Trigrams:   12265
  Cache:      2/50000
  Watcher:    active
  Watch mode: native (requested: auto)
  Watch budget: 8192
  Poll interval: 120s (after completion)
  Reconcile:  idle
  Last successful reconcile: 2m ago
  Reconcile pending: no
  Reconcile overdue: no
  Last reconcile duration: 42ms
  Indexing:   complete
  Hidden coverage: complete
```

`Watcher` refers to native notifications, so it is normally inactive in polling
mode. Refresh fields report fallback reasons, errors, pending work, and elapsed
deadlines. Older servers may omit these fields.

`Indexing: complete` and `Hidden coverage: complete` describe index readiness,
not freshness. An existing index can be queried while startup reconciliation
is still running. Without a server, `status` shows on-disk metadata.

### Count files

```bash
tgrep count-files .              # count candidate text files (no server needed)
tgrep count-files /path/to/repo  # scan a specific repo
```

Counts files admitted by the walk's ignore, visibility, extension, and default
size rules, without reading their contents. The reported "text files" can
therefore include files containing NUL bytes. Prints the count to stdout and
details to stderr:

```
284957
284957 text files (47516 binary skipped, 0 too large, 0 errors) in 1200ms
```

## CLI Flags

These tables describe search flags. Use `tgrep index --help` and
`tgrep serve --help` for subcommand options.

| Flag | Description |
|------|-------------|
| `-i, --ignore-case` | Case-insensitive matching |
| `-s, --case-sensitive` | Force case-sensitive matching (overrides `-S`) |
| `-S, --smart-case` | Case-insensitive if pattern is all lowercase |
| `-F, --fixed-strings` | Treat pattern as a literal string |
| `-w, --word-regexp` | Match whole words only |
| `-v, --invert-match` | Show lines that do NOT match |
| `-o, --only-matching` | Print only the matched parts |
| `-e, --regexp <PAT>` | Additional pattern (repeatable for OR) |
| `-f, --file <FILE>` | Read patterns from file (one per line) |
| `-U, --multiline` | Enable multiline matching (`.` still excludes `\n`) |
| `--multiline-dotall` | Make `.` match `\n`; implies `-U` |
| `-n, --line-number` | Show line numbers (default: on when stdout is a terminal) |
| `-N, --no-line-number` | Suppress line numbers |
| `-c, --count` | Count matching lines per file |
| `-l, --files-with-matches` | Print only filenames |
| `--files-without-match` | Print files that do NOT match |
| `-q, --quiet` | Suppress output; exit code only |
| `-m, --max-count <N>` | Limit matching lines per file (see [Match limits](#match-limits)) |
| `-g, --glob <GLOB>` | Filter files by glob pattern, case-sensitive (repeatable) |
| `--iglob <GLOB>` | Case-insensitive glob filter (repeatable) |
| `--glob-case-insensitive` | Treat all `-g` globs as case-insensitive |
| `-t, --type <TYPE>` | Filter by file type (`rust`, `py`, `js`, …; repeatable) |
| `-T, --type-not <TYPE>` | Exclude a file type (repeatable) |
| `--type-add <SPEC>` | Add/extend a type, e.g. `--type-add 'web:*.html'` |
| `--type-clear <TYPE>` | Remove a type's definitions |
| `--type-list` | Print all supported file types (reflects `--type-add`/`--type-clear`) |
| `--files` | List admitted paths without content checks; includes binary files |
| `-A, --after-context <N>` | Lines of context after match |
| `-B, --before-context <N>` | Lines of context before match |
| `-C, --context <N>` | Lines of context before and after |
| `--heading / --no-heading` | Grouped vs flat output |
| `-H, --with-filename` | Show filenames (default: on unless a single file was named) |
| `-I, --no-filename` | Suppress filenames in output |
| `--json` | ripgrep-compatible JSON stream (one object per line) |
| `--vimgrep` | Vim-compatible `file:line:col:content`, one row per match |
| `--color auto/always/never` | Color mode control |
| `-0, --null` | NUL byte filename separator (for xargs) |
| `--trim` | Trim leading/trailing whitespace |
| `-., --hidden` | Include hidden files and directories |
| `--no-ignore` | Don't respect `.gitignore` or `p4ignore.ini` files |
| `-a, --text` | Search binary files as if they were text |
| `--binary` | Search binary files, reporting a note instead of their contents |
| `-u, --unrestricted` | Unrestricted: `-u` = no-ignore, `-uu` = +hidden, `-uuu` = +binary |
| `--max-filesize <SIZE>` | Skip files larger than `SIZE` (`K`/`M`/`G` suffixes); default 64M |
| `--no-max-filesize` | Apply no size limit, as ripgrep does |
| `-L, --follow` | Follow symbolic links |
| `--no-messages` | Suppress error messages about unreadable/missing paths |
| `--no-index` | Read files from disk, bypassing the server and index; normal filters still apply |
| `--exclude <DIR>` | Exclude directory from indexing (repeatable); `index` and `serve` only, not accepted by a search |
| `--stats` | Print query plan and candidate stats |
| `--index-path <DIR>` | Custom index directory |

**Pattern matching**

| Flag | Description |
|------|-------------|
| `-x, --line-regexp` | The pattern must match a whole line (beats `-w`) |
| `-P, --pcre2` | Use the backtracking engine (lookaround, backreferences) |
| `--engine <auto\|default\|pcre2>` | Pick the regex engine explicitly; `auto` falls back to `pcre2` |
| `--pcre2-version` | Print the backtracking engine in use and exit |
| `--no-unicode` | Disable Unicode-aware character classes |
| `--regex-size-limit <SIZE>` | Cap the compiled regex size (`K`/`M`/`G` suffixes) |
| `--dfa-size-limit <SIZE>` | Cap the regex DFA cache size |
| `-r, --replace <TEXT>` | Replace each match; `$1`/`${name}` expand capture groups |
| `--passthru` | Print every line, matching or not |
| `--stop-on-nonmatch` | Stop searching a file at its first non-matching line |

`--engine auto` (the default) uses Rust's `regex` crate, with fallback to
`fancy-regex` for lookaround and backreferences. `-P` and `--engine pcre2`
select `fancy-regex`, not the PCRE2 library; they do not promise full PCRE2
syntax compatibility.

**Output formatting**

| Flag | Description |
|------|-------------|
| `--column` / `--no-column` | Show the 1-based column of the first match |
| `-b, --byte-offset` | Show the byte offset of the line (or match, with `-o`) |
| `-M, --max-columns <N>` | Omit lines longer than `N` bytes |
| `--max-columns-preview` | Show a truncated preview instead of omitting |
| `--count-matches` | Count matches rather than matching lines |
| `--include-zero` | With `-c`, also print files with a count of `0` |
| `-p, --pretty` | Alias for `--color always --heading -n` |
| `--context-separator <SEP>` | Separator between context groups (default `--`) |
| `--no-context-separator` | Print no separator between context groups |
| `--field-match-separator <SEP>` | Separator between match fields (default `:`) |
| `--field-context-separator <SEP>` | Separator between context fields (default `-`) |
| `--path-separator <SEP>` | Rewrite the separator in printed paths |
| `--sort <KEY>` / `--sortr <KEY>` | Sort by `path`/`modified`/`accessed`/`created`/`none` |
| `--sort-files` | Shorthand for `--sort path` |
| `--line-buffered` / `--block-buffered` | Force line- or block-buffered stdout |

**Encoding**

| Flag | Description |
|------|-------------|
| `-E, --encoding <LABEL>` | Decode as `LABEL` (e.g. `utf-16le`, `latin1`, `sjis`); `none` disables BOM sniffing and transcoding |
| `--no-encoding` | Restore BOM-sniffing auto-detection |

By default, a UTF-8/UTF-16LE/UTF-16BE BOM selects decoding; otherwise files are
treated as UTF-8. A BOM overrides a named encoding, but not `-E none`.
Any non-`auto` encoding bypasses the index and server. Invalid UTF-8 is still
repaired, including with `-E none`; this is not raw-byte matching.

**File walking**

| Flag | Description |
|------|-------------|
| `--max-depth <N>` | Limit directory recursion depth |
| `--one-file-system` | Don't cross file-system boundaries |
| `--ignore-file <FILE>` | Read extra ignore rules from `FILE` (repeatable) |
| `--ignore-file-case-insensitive` | Match `--ignore-file` rules case-insensitively |
| `--no-ignore-dot` | Ignore `.ignore` files |
| `--no-ignore-exclude` | Ignore `.git/info/exclude` |
| `--no-ignore-files` | Ignore any `--ignore-file` arguments |
| `--no-ignore-global` | Ignore the global gitignore |
| `--no-ignore-parent` | Ignore rules from parent directories |
| `--no-ignore-vcs` | Ignore `.gitignore` files |
| `--no-ignore-messages` | Suppress errors about malformed ignore files |
| `--no-require-git` | Apply git ignore rules outside a git repository |
| `-j, --threads <N>` | Filesystem walker threads; does not set the regex-search or server worker pool |

**Accepted for compatibility**

`--mmap`/`--no-mmap` (tgrep chooses memory mapping automatically), `--crlf`/`--no-crlf`
(a trailing `\r` is always stripped), `--no-config` (tgrep reads no config
file), and `--colors <SPEC>` (colors are not yet configurable) are accepted and
ignored so ripgrep command lines keep working. `--debug`/`--trace` imply
`--stats`.

For indexed content searches, `--stats` reports **Raw candidates** before
path, visibility, glob, and type filters, and **Candidates** after those filters
but before size checks, reads, or matching. **`no index narrowing`** means the
raw set covers the entire nonempty index. **`(via server)`** describes transport,
not index effectiveness. Older servers may omit candidate statistics.

`-z/--search-zip` is **not** supported and exits with code `2` rather than
silently reporting no matches in compressed files.

> **Note:** `-L` means `--follow` (as in ripgrep). Use the long
> `--files-without-match` for the non-matching-files listing.

### Patterns and paths

Without `-e`/`-f`, the first positional argument is the pattern and the rest are
paths. As soon as `-e` or `-f` supplies a pattern, **every** positional becomes
a path, matching ripgrep:

```bash
tgrep -e needle .            # searches for "needle" under .
tgrep -e needle -e other .   # both patterns, still just one path
```

Use `--` before a positional pattern that starts with `-` or is a subcommand
name, such as `serve` or `index`. Quoting alone does not prevent subcommand
parsing. Put all flags before `--`:

```bash
tgrep -F -- serve .
```

### Output defaults

tgrep matches ripgrep's context-dependent defaults rather than fixed ones:

- **Line numbers** are on only when stdout is a terminal. Piping to another
  command drops them, so `tgrep needle . | cut -d: -f1` behaves the same as it
  does with ripgrep. `--column`, `--vimgrep` and `-p` turn them back on;
  `-b` and `-A/-B/-C` do not.
- **Filenames** are shown unless you named exactly one *file*. A directory
  argument always shows them. `-H`/`-I` override either way.
- **Paths** are printed by appending onto the argument you typed: the argument
  survives verbatim and only the appended part uses the platform separator. So
  `tgrep needle src/` prints `src/main.rs` while `tgrep needle src` prints
  `src\main.rs` on Windows. `--path-separator` rewrites every separator.

### Match limits

`-m/--max-count` limits matching lines, not individual matches. All matches
on an admitted line are reported. With `-U`, the unit is a contiguous block
of lines covered by matches, so a multiline match is not cut short.

tgrep reads a whole-file buffer even with `-m`; its `bytes_searched` statistic
can therefore exceed ripgrep's.

### Multiline matches

`-U/--multiline` allows matches across line boundaries and prints every covered
line. `--vimgrep` reports one row per match, on its starting line.
Unlike ripgrep, tgrep reports columns relative to each printed line rather
than repeating the starting column on continuation lines.

### Binary files

A file is treated as binary if its decoded content contains a NUL byte:

- Binary files found by walking a directory are **skipped silently** — they
  appear in neither the output, `-l`, `-c`, nor `--files-without-match`.
- A binary file **named explicitly** on the command line reports a note:
  `bin.dat: binary file matches (found "\0" byte around offset 7)`.
- `--binary` promotes traversal to the explicit behaviour, so binary files are
  searched and summarised with that note.
- `-a`/`--text` disables binary detection entirely and prints matches as text.
- `--json` has no note. As in ripgrep, the matching lines are emitted as
  ordinary `match` events and the file's `end` message carries
  `binary_offset` — the offset of the first NUL — so a consumer can still tell
  a binary hit apart from a text one. `stats.bytes_searched` stops at that
  offset rather than counting the whole file.

tgrep also skips known binary extensions during directory content searches
and indexing, unlike ripgrep. For searches, `--binary` and `-a` lift this
restriction and bypass the index; they do not change what `index` or `serve`
indexes. `--files` lists binary paths too.

### Flags that bypass the index

`index` and `serve` include hidden, **non-ignored** files by default.
`--hidden` is accepted on either command but is redundant. Ordinary queries
still hide hidden files and directories; `-./--hidden` includes them using the
index or server, for content searches and `--files`. Visibility is relative to
the requested search root and includes Windows hidden attributes. Ignore rules
inside hidden directories remain active. The configured index directory,
including its staging and retired generations, is always excluded from indexing
and query filesystem walks.

Flags that widen or re-interpret the indexed corpus still walk the tree:
non-`auto` `-E/--encoding`, `-a/--text`, `--binary`, and ignore-disabling flags
such as `--no-ignore`. Explicit file arguments also bypass the index.

For content searches, `--follow`, `--one-file-system`, `--ignore-file`, and
`--ignore-file-case-insensitive` do **not** trigger fallback: indexed searches
ignore them. Add `--no-index` to apply them. `--files` falls back to walking
for these options automatically. `index` and `serve` reject these traversal options,
`--max-depth`, and individual `--no-ignore-*` discovery switches.

Both positive and negative `--glob`/`--iglob` patterns filter the indexed corpus
when a compatible index or server is available, for content searches and
`--files`. Positive globs such as `--glob '*.ts'` do not reinclude ignored files
in indexed mode. Use `--no-index` when those matches are needed: filesystem
scans retain ripgrep-style glob overrides, which can reinclude ignored files.
Negative globs, such as `--glob '!.git'`, exclude entire matching directory
subtrees. `--hidden` never disables ignore rules.

Incomplete indexes and legacy indexes without hidden-file coverage fall back
to scanning. Rebuild with `tgrep index .` or let a current server reconcile
them. Older servers without coverage support also cause fallback.

Older binaries reject the new content-index format. To downgrade, rebuild
with the older binary, preferably at a separate `--index-path`.

### Invalid UTF-8

ripgrep can search raw bytes; tgrep searches decoded text, replacing invalid
UTF-8 sequences with `U+FFFD`. A pattern can match those replacement characters.
For UTF-8 repair, text-output columns and byte offsets are mapped back to the
source bytes.

JSON output uses `lines.text` with replacements and submatch offsets into that
text. ripgrep instead emits base64 `lines.bytes` for invalid UTF-8.
Transcoded encodings use decoded-text offsets rather than original byte
positions; offsets also exclude stripped BOM bytes.

### File size limits

Directory searches and indexing skip files larger than **64 MiB** by default;
ripgrep has no default limit. These files are absent from the index and its
filename listing, so their matches will not appear.

Use `--max-filesize` to choose another bound, or `--no-max-filesize` to remove
it. Rebuild or serve the index with the same setting; an uncapped query cannot
recover paths that a capped index never recorded. Use
`--no-index --no-max-filesize` to scan without either restriction.

A directly named file ignores the inherited cap, but respects an explicit
`--max-filesize`: `tgrep -- pattern ./huge.log` searches that file regardless
of its size.

### Exit codes

Same as ripgrep:

| Code | Meaning |
|------|---------|
| `0` | At least one match was found |
| `1` | No matches |
| `2` | An error occurred (e.g. a path could not be read) |

A match plus an error yields `2`, unless `-q` is set, which yields `0`.

An ignore file that fails to parse is *not* one of these errors. Like ripgrep,
tgrep reports it on stderr, skips the offending rule and carries on, leaving the
exit code determined by the search alone. Suppress the message with
`--no-ignore-messages`, or with `--no-messages`, which covers it as well.

## How It Works

1. **Indexing** walks the repository with ignore rules, including root-level
   `p4ignore.ini`, and filters by size and binary extension. It decodes files,
   checks the first 8 KiB for NUL bytes, and extracts overlapping three-byte
   trigrams in parallel. Sorted postings map trigrams to candidate files.
2. **Querying** decomposes regex literals into trigram lookups, intersects or
   unions posting lists, and verifies candidates with the full regex engine.
   Inline case flags participate in planning; unsupported constructs use
   conservative plans rather than excluding possible matches.
3. **Serving** combines a memory-mapped `IndexReader` with a mutable `LiveIndex`
   overlay in `HybridIndex`. Updates take precedence over disk entries.
   Clients use JSON-RPC 2.0 over newline-delimited TCP on loopback, with one
   thread per connection. Shared read locks allow concurrent queries but can
   contend with writers.

The server's content cache is limited to 50,000 entries and 1 GiB of decoded
content, with a 64 MiB per-entry limit. `--files` combines content-index paths
with a filename-only sidecar for admitted paths without searchable content.

### Shared worktree indexes (core API)

See the [shared worktree index design](SHARED_WORKTREE_INDEXES.md) for
architecture diagrams, the agent runtime boundary, base-generation lifecycle,
and the staged implementation plan.

`tgrep-core::generations` manages immutable committed-tree bases:

```rust
use std::path::Path;
use tgrep_core::generations::{GenerationManager, IndexingProfile, Repository};

let repository = Repository::discover(Path::new("."))?;
let manager = GenerationManager::new(repository)?;
let first = manager.ensure("HEAD", IndexingProfile::default(), None)?;
let next = manager.ensure("main", IndexingProfile::default(), Some(&first.generation))?;
// Keep this exact pin with the synchronized worktree registration below.
let pin = next.generation;
```

Repository identity is the **canonical Git common directory**, shared by linked
worktrees but not independent clones. Git is required; commands use arguments,
not shell interpolation, and ignore ambient `GIT_*` overrides. Symbolic revisions
such as `HEAD` resolve in the discovered worktree. Keys contain repository
identity, exact committed tree OID, indexing profile, and format/schema versions;
commit OIDs are retained for diagnostics. `new` stores generations below the
common directory's `tgrep-bases-v1`. `with_storage` accepts an existing trusted
directory outside registered worktrees and Git metadata.
Both that parent and its effective repository-identity subdirectory are checked;
the effective directory cannot itself be a linked worktree or index snapshot.

The current profile indexes **raw Git blobs** with the existing automatic text
decoder. It covers all tracked regular files, including hidden, ignored, and
extension-filtered paths: a superset, not a worktree's searchable membership.
`entries()` records each tracked path's mode, object OID, size and classification.
`Indexed { content_id }` includes empty/short files; `Binary`, `TooLarge`, and
`NotRegular` have no content-index entry. Symlinks and gitlinks are never followed.
Non-UTF-8 tracked paths and paths the platform/index cannot represent return
explicit unsupported errors; repository roots preserve their native identity.
Clean Git status does **not** establish compatibility with CRLF, encoding, LFS
or smudge transformations. `entry.matches_worktree_bytes(bytes)` compares the
decoded identity only; it proves neither stable filesystem reads nor visibility.

A compatible predecessor reuses unchanged blob postings and masks, including
copies/renames, while extracting only new content through one Git batch process.
Destination mode and size eligibility are recomputed. Without a predecessor,
a missing generation requires a full build. `EnsureResult.stats` reports actual
blob reads/extractions, bytes, reused files/postings, and publication/reuse.
`predecessor_posting_lists_read` counts actual predecessor traversal; it is zero
when no indexed paths can reuse postings, including when only empty/short files
are unchanged. Their paths and content identities still reuse normally.
Posting presence is collected during existing strict snapshot validation;
no persisted metadata or index-format change is required.
An OS file lock deduplicates cooperating starters across processes; live
generation pins share one in-process `Arc<Generation>` and `Arc<SharedBase>`.
The spill sorter bounds posting accumulation; one raw/decoded blob and its
extracted masks are held at a time. Removing the size cap can therefore require
substantial memory. Publication still streams a complete new index.

`open(key)` reopens an exact generation; `list()` returns validated published
keys. Complete staging directories are validated and atomically renamed before
discovery. Corrupt/incomplete final generations error rather than being replaced;
abandoned staging directories are not discoverable. File contents are synced,
but parent-directory power-loss durability is not guaranteed.
**Retention is explicitly `RetainAll`: no published generation is deleted**, even
after pins drop. Persist the key with retained overlay checkpoints. Online GC,
checkpoint eviction and resource budgets remain future daemon responsibilities;
offline cleanup requires stopping all users and discarding dependent checkpoints.
Storage and its ancestors must not be externally renamed, modified or removed
while in use. Do not run mutable index builders against generation directories.

`tgrep_core::worktrees` provides the core registration and synchronization layer:

```rust
use tgrep_core::query::build_query_plan;
use tgrep_core::worktrees::{WorktreeOptions, WorktreeView};

let view = WorktreeView::new(Path::new("."), pin, WorktreeOptions::default())?;
// An agent runtime subscribes to changes NOW, before the first refresh.
// Forward file/subtree events to invalidate_path; overflow/config changes
// and unknown events to invalidate_all. The core does not create a watcher.
let stats = view.refresh()?; // initially a full verification; now ready

let plan = build_query_plan("needle", false)?;
view.with_snapshot(|snapshot| -> std::io::Result<()> {
    for path in snapshot.candidates(&plan, "", false) {
        let file = snapshot.open_file(&path)?; // uses the view's retained root handle
        // Bounded-read `file`, auto-decode and run your final matcher here,
        // buffering results until with_snapshot succeeds; or use a private cache.
    }
    Ok(())
})??;
let files = view.with_snapshot(|snapshot| snapshot.files("", false))?;
// Candidates are root-relative paths, not final matches. Do not read a joined
// pathname with ordinary filesystem APIs: a raced link could leave the worktree.
```

The root must be the actual worktree root in the pin's repository, not a
subdirectory, bare repository or independent clone. `root()`, `repository()`
and `generation()` preserve its canonical identity and exact pin. Each view
keeps private whole-file replacements and tombstones relative to **that pin**,
not current `HEAD`; committing an edit does not clear its override. Missing,
sparse, ignored and ineligible base paths are hidden. Membership and visibility
use the existing hidden-inclusive walker and a frozen case-insensitive tracked
exemption snapshot. `files()` includes eligible filename-only binary paths.
Query `prefix` is empty or a root-relative directory ending in `/`.

`invalidate_path(relative_path)` immediately closes the query gate and queues
a bounded file/subtree hint (both paths for renames). Case aliases trigger
conservative verification; non-ASCII hints and queue overflow force a full pass.
Accepted trailing/repeated separators and interior `.` components are normalized,
so `dir/` still invalidates the entire `dir` subtree. Absolute paths, parent
components, and leading `.` remain explicit errors that force full repair.
`invalidate_all()` handles native watcher overflow, polling uncertainty, Git or
ignore configuration changes, and missed-event repair. `refresh()` rewalks
membership/visibility and processes hints; with no hints it does full content
verification. `reconcile_full()` always verifies all admitted bytes, including
same-size/restored-mtime edits, assume-unchanged and skip-worktree files.

Construction and checkpoint restoration are **not ready**. A successful refresh
atomically publishes overlay, filenames and visibility. `status()` exposes
readiness, invalidation/published epochs and pending/full work. A concurrent
invalidation causes `ChangedDuringReconcile`, not stale ready publication:
retry or scan. There is one attempt per call, never an unbounded churn loop.
Every reconciliation advances the epoch, including refreshes without hints;
the published epoch acknowledges all earlier invalidation tokens.
Discovery/read errors likewise leave the view unavailable and force full retry.
Reconciliation pins a root directory handle and opens only regular files:
Unix uses component-relative no-follow, nonblocking opens; Windows guards
ancestor handles against replacement and checks resolved handle
containment before reading. Detected root identity changes and read-path
swaps leave the view not-ready. Windows retains the root guard until view drop, so
an agent runtime should release registrations before removing or renaming a
worktree. Ordinary servers also retain their root guard for their lifetime:
stop `tgrep serve` before removing or renaming its served root on Windows.
The shared helper is `tgrep_core::rooted::RootedDir`
(`open`, `open_file` with a relative path, and `verify_root`); ordinary serving
retains one per server and reuses it across build, watcher and verification reads.
`WorktreeSnapshot::open_file` uses the view's existing reader, not a new root
registration per candidate. Neither API freezes file contents.
Metadata discovery rejects unrepresentable native names before conversion:
non-Unicode paths and literal Unix backslashes cannot alias other indexed paths.
Ordinary full scans retain native paths and remain available as the fallback.
`with_snapshot` holds readiness and overlay guards through candidate-ID
resolution, and verifies the pinned root before and after the callback, even for
empty/file-only results. Verification failure closes readiness and queues full
repair. Candidate-open failures are recorded too: `with_snapshot` returns an
outer I/O error and invalidates readiness before releasing the guard, even if
the callback swallowed the inner error or subsequently opened another file.
No live IDs or mutable `HybridIndex` escape. A callback may return an
owned read-only file handle for bounded matching outside the guard; before
publishing buffered results, reenter `with_snapshot` and reject a changed epoch.
Later reads through that handle remain caller-owned: report read errors and call
`invalidate_all()` after leaving the guard rather than publishing partial results.
Do not reenter the view from its closure. A refresh acknowledges processed hints,
**not an atomic filesystem snapshot**. Periodic full reconciliation remains necessary; no-watch
callers must explicitly refresh, and final reads can race subsequent edits.

Full verification reads, auto-decodes and hashes checkout bytes; clean Git
status, blob identity and index stat data are never byte-equivalence proofs.
Matching decoded content reuses base postings without extraction. Verified
renames/copies can copy base masks in one streaming posting pass; unchanged
private overlays also avoid extraction. A hinted refresh can avoid rereading
unaffected, previously verified files under the event-driven freshness contract,
but still performs a metadata/membership walk. `ReconcileStats` separates
reads/bytes/decodes, actual extraction calls, base and overlay reuse, copied
files/postings, and content reads avoided.
`hint_lookups` counts ordered-set probes: each file checks its normalized path
and ancestor prefixes, at most one probe per component rather than a scan of
all queued hints. Each probe is logarithmic in the hint count. Even a hinted
pass opens eligible file handles to verify regular-file metadata safely;
avoided content reads do not mean zero filesystem I/O.

**Raw LF bases versus CRLF/smudge checkouts can require an all-file overlay**,
even for equal committed trees and clean Git status. Position and next-byte
masks make line-ending normalization unsafe. Regression fixtures measure zero
extractions for three byte-identical tracked files, but four for four transformed
tracked files. A linked worktree's plain `.git` file adds one private extraction
in either case; subsequent unchanged reconciliation reuses those private postings. There is no
cross-view transformed-content cache. Preparation retains per-path evidence
and changed-file masks until atomic publication; removing the size cap increases
per-file memory, and widespread transforms can make the private overlay large.

For persistence, configure an existing dedicated `checkpoint_directory` in
`WorktreeOptions`. Construction, restoration and saving explicitly reject
non-directory checkpoint paths. `save_checkpoint()` requires readiness and atomically writes
only `overlay.json`: private postings/tombstones plus exact generation key, base
fingerprint and canonical root, through the existing directory-bound publisher.
The entire checkpoint directory, `.tgrep`, Git metadata directories and the generation store
are excluded, even with `no_ignore`. A linked worktree's plain `.git` pointer
file remains eligible under ordinary walker rules: `--hidden` exposes its
filename and searchable text unless ignored or over the size cap. This does
not expose the metadata directory it names. Custom storage directories belong in
`walk.exclude_paths`. Checkpoint storage and ancestors must remain trusted and
not externally renamed while in use. The directory cannot be an index snapshot,
Git metadata, or a worktree ancestor. `WorktreeView::restore(root, exact_pin,
options)` rejects missing, malformed, wrong-key and wrong-root checkpoints.
Successful restoration still requires subscribing and full reconciliation;
restored overlays have no trusted read evidence and may re-extract private
postings on that first pass. Generation retention remains `RetainAll`.

`tgrep-core::shared::SharedBase` is the first building block for sharing one
content index across worktrees. Open a complete, current-format index in an
**immutable snapshot directory** once, then call `create_worktree(root)` for
each worktree. The returned `HybridIndex` instances share the same
`Arc<IndexReader>` (including the path table), but have independent roots,
live postings, and deletion tombstones. Cloning `SharedBase` does not copy the
base index.

Shared-base opening rejects mismatched empty lookup/posting sections and
metadata counts inconsistent with the opened index. Legitimately empty
indexes and files too short to produce trigrams remain supported. Shared
snapshots also require aligned, contiguous posting ranges covering `index.bin`,
valid trigrams, strictly increasing valid file IDs within each posting list,
and nonzero location masks; ordinary readers retain their existing validation
behavior.

The caller must populate each overlay before exposing it to searches:
index whole changed/new files using `view.live.upsert_file`, and hide deleted
or ineligible base paths using `view.live.delete_file`. Include committed
branch differences, staged and unstaged edits, and eligible untracked files,
not just `git diff HEAD`. Only reuse base postings when their decoded content
matches the worktree's indexing semantics; Git checkout filters, encoding,
and line-ending conversion can change that content. Clear a path's override
with `clear_reconciled_paths` only after proving it matches the base again.

`save_overlay(&view, checkpoint_path)` atomically saves only the worktree's
live postings, masks, and tombstones. It never merges changes into the base.
`restore_worktree(root, checkpoint_path)` checks the checkpoint version,
canonical root, and a fingerprint of the base's path table, lookup table,
and postings. Wrong-base, wrong-root, missing, and malformed checkpoints
return errors rather than silently revealing base entries. Opening the base
computes its fingerprint once; attaching more worktrees does not rescan it.
Checkpoint parent directories must already exist and be outside the base
snapshot directory. Repeated saves atomically replace the existing checkpoint,
including on Windows. Base-directory exclusion checks directory identities,
not just pathname prefixes. On Unix, staging, replacement, and cleanup use the
opened parent directory handle. On Windows, open handles prevent renaming or
deleting the canonical parent and its ancestors until publication finishes.
Replacing a parent pathname cannot redirect a save into the shared base.
Paths with trailing separators or `/.` are rejected rather than normalized
into filenames.
Unicode worktree roots retain the existing JSON string representation;
non-Unicode roots use tagged Unix bytes or Windows UTF-16 units to preserve
their exact identity. Existing Unicode-root checkpoints remain readable;
older readers cannot restore the new non-Unicode representation.
Atomic replacement does not guarantee power-loss
durability: file contents are synced before replacement, but the parent
directory is not synced afterwards. A successful save may be lost after a
system crash; callers must reconcile or rebuild stale/missing checkpoints.

The lower-level `SharedBase` API alone does not synchronize a checkout; use
`WorktreeView` for the reconciliation/readiness contract above. Neither API
creates native watchers, shares content caches, or provides automatic
multi-worktree CLI/server registration.
The existing CLI, RPC protocol, index format, and single-root server are
unchanged. Do not point existing servers at a common `--index-path`: their
publication path still writes a complete index. Keep shared base files
immutable for the lifetime of every view or query holding their reader.

## On-Disk Format

| File | Description |
|------|-------------|
| `lookup.bin` | Sorted 16-byte entries: `trigram(u32) + offset(u64) + length(u32)` |
| `index.bin` | Concatenated 6-byte postings: `file_id(u32) + loc_mask(u8) + next_mask(u8)` |
| `files.bin` | Version 3 header, then `file_id(u32) + path_len(u16) + path_bytes`; legacy headerless tables remain readable |
| `files-extra.bin` | Version 2 filename-only paths, visibility, and file-table identity |
| `meta.json` | Version, file/trigram counts, timestamps, coverage, visibility, and file-table identity |
| `filestamps.json` | Per-file metadata, content identities, and version evidence for reconciliation |
| `serve.json` | Server PID and TCP port (for client discovery) |
| `serve.lock` | Exclusive lock preventing multiple servers from owning the same index |

## Project Structure

| Directory | Contents |
|-----------|----------|
| `tgrep-core/` | Traversal, decoding, trigram extraction, index storage, and query planning |
| `tgrep-cli/` | CLI parsing, matching, output, server, and integration tests |
| `scripts/` | Benchmarks, agent integration, and development utilities |
| `fuzz/` | Fuzz targets for index reading and query/trigram parsing |

## Building

```bash
cargo build --release --locked  # build optimized binary
cargo test --workspace          # run tests
make check                      # check formatting and run clippy
make install                    # install to ~/.cargo/bin (Unix)
```

## Installation

### From source

```bash
git clone https://github.com/microsoft/tgrep.git
cd tgrep
cargo install --path tgrep-cli --locked
```

### Homebrew (Linux, macOS)

```bash
brew install tgrep
```

### Pre-built binaries

Download from [GitHub Releases](https://github.com/microsoft/tgrep/releases)
for Linux, macOS (Intel & Apple Silicon), and Windows.

Download the archive for your architecture, extract it to a temporary
directory, then copy the executable into a directory on `PATH`.

```bash
# Linux (x86_64)
tmpdir="$(mktemp -d)"
gh release download --repo microsoft/tgrep -p '*x86_64-unknown-linux-musl.tar.gz' -D "$tmpdir"
tar xzf "$tmpdir"/tgrep-*-x86_64-unknown-linux-musl.tar.gz -C "$tmpdir"
install -Dm755 "$tmpdir/tgrep" "$HOME/.local/bin/tgrep"
rm -rf "$tmpdir"

# macOS (Apple Silicon)
# For Intel, replace aarch64 with x86_64.
tmpdir="$(mktemp -d)"
gh release download --repo microsoft/tgrep -p '*aarch64-apple-darwin.tar.gz' -D "$tmpdir"
tar xzf "$tmpdir"/tgrep-*-aarch64-apple-darwin.tar.gz -C "$tmpdir"
mkdir -p "$HOME/.local/bin"
install -m755 "$tmpdir/tgrep" "$HOME/.local/bin/tgrep"
rm -rf "$tmpdir"
```

```powershell
# Windows x64 (PowerShell); for ARM64, replace x86_64 with aarch64.
$download = Join-Path $env:TEMP ("tgrep-dl-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $download | Out-Null
gh release download --repo microsoft/tgrep -p '*x86_64-pc-windows-msvc.zip' -D $download
Get-ChildItem $download -Filter '*.zip' | ForEach-Object {
    Expand-Archive -LiteralPath $_.FullName -DestinationPath "$HOME\.cargo\bin" -Force
}
Remove-Item -LiteralPath $download -Recurse
```

Ensure `$HOME/.local/bin` (Unix) or `$HOME\.cargo\bin` (Windows) is on `PATH`.

## Contributing

This project welcomes contributions and suggestions.  Most contributions require you to agree to a
Contributor License Agreement (CLA) declaring that you have the right to, and actually do, grant us
the rights to use your contribution. For details, visit https://cla.microsoft.com.

When you submit a pull request, a CLA-bot will automatically determine whether you need to provide
a CLA and decorate the PR appropriately (e.g., label, comment). Simply follow the instructions
provided by the bot. You will only need to do this once across all repos using our CLA.

This project has adopted the [Microsoft Open Source Code of Conduct](https://opensource.microsoft.com/codeofconduct/).
For more information see the [Code of Conduct FAQ](https://opensource.microsoft.com/codeofconduct/faq/) or
contact [opencode@microsoft.com](mailto:opencode@microsoft.com) with any additional questions or comments.

## License

[MIT](LICENSE)
