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
