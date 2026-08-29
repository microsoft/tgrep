# tgrep

Trigram-indexed grep with a client/server architecture for fast regex search
in large codebases.

## Why?

Tools like `grep` and `ripgrep` scan every file on every search — O(total bytes)
per query. In a 100k+ file monorepo, that's painfully slow. tgrep pre-builds a
trigram index so searches only touch the small set of files that could match.

**Start a server once, search instantly forever.**

```bash
tgrep index .            # build the trigram index
tgrep serve .            # start server (watches for file changes)
tgrep "fn main" .        # instant — auto-connects to running server
```

See [full benchmark results](BENCHMARKS.md) — up to **52x faster** than ripgrep on large repos.

### Benchmark highlights (avg latency per query, index pre-built)

| Repo | Files | Platform | ripgrep | tgrep | Speedup |
| --- | ---: | --- | ---: | ---: | ---: |
| gecko-dev | 388K | macOS arm64 | 33,402ms | 643ms | **51.9x** |
| gecko-dev | 388K | Windows | 17,841ms | 463ms | **38.6x** |
| gecko-dev | 388K | Linux | 1,195ms | 162ms | **7.36x** |
| chromium | 504K | macOS arm64 | 41,806ms | 2,643ms | **15.8x** |
| chromium | 504K | Windows | 24,576ms | 1,396ms | **17.6x** |
| chromium | 504K | Linux | 2,404ms | 631ms | **3.81x** |
| go | 16K | Windows | 592ms | 79ms | **7.53x** |
| rust | 62K | Windows | 1,489ms | 194ms | **7.69x** |
| kubernetes | 31K | Windows | 1,342ms | 190ms | **7.08x** |
| linux | 96K | macOS arm64 | 5,390ms | 256ms | **21.0x** |
| linux | 96K | Windows | 3,280ms | 94ms | **34.8x** |
| linux | 96K | Linux | 427ms | 46ms | **9.38x** |

tgrep wins 17 of the 18 measured cells; the exception is Kubernetes on Linux, a
near-tie at 0.93x. The margin depends on repo size and on how many matches a query
returns — a search that returns tens of thousands of matches spends more on
delivering them than the index saves on finding them. See
[What decides the margin](BENCHMARKS.md#what-decides-the-margin).

## Architecture

```
tgrep <pattern> ---TCP---> tgrep serve (multi-client)
    (client)                   |
                          HybridIndex
                          /         \
                   IndexReader    LiveIndex
                   (mmap disk)   (in-memory overlay)
                        ^              ^
                        |              |
                  Periodic Flush  File Watcher (notify)
                  (50K files /    Background Indexer
                   5 min)         (rayon parallel)
```

- **IndexReader** — mmap'd on-disk index (zero-copy, binary search on sorted
  trigram lookup table)
- **LiveIndex** — in-memory overlay for files modified after server start, or
  being built by the background indexer
- **HybridIndex** — merges both layers; overlay takes precedence
- **Background Indexer** — builds the index in parallel batches of 500 files
  using rayon; queries are served immediately from partial data
- **Periodic Flush** — every 50K files or 5 minutes, the in-memory index is
  flushed to disk and the reader is swapped, keeping memory bounded
- **File Watcher** — `notify` crate watches the repo; updates LiveIndex in
  real time
- **TCP Server** — JSON-RPC 2.0 over newline-delimited TCP; each connection
  handled in a separate thread; multiple clients can connect simultaneously
- **File Cache** — 50K-entry content cache with RwLock for lock-free reads

## Performance

tgrep is designed to be significantly faster than ripgrep on large repos:

- **Parallel search** — candidate files are searched in parallel using rayon
- **Fast query planning** — sorted posting lists are intersected/unioned without
  unnecessary resorting, and on-disk posting lists skip redundant deduplication
- **Memory-efficient full builds** — index builds batch extraction and stream
  sorted postings, file entries, and lookup entries instead of retaining the full
  inverted index in memory
- **Smart file walking** — extension-based binary rejection (50+ formats) and an
  8KB content check, with a 64 MiB size cap on both indexing and searching
  (`--no-max-filesize` removes it)
- **Lock-free reads** — `RwLock<HashMap>` cache allows concurrent reads
  without contention
- **Hot serving** — queries work immediately during background index building;
  no need to wait for full index

See [BENCHMARKS.md](BENCHMARKS.md) for end-to-end large-repo benchmarks and
Criterion microbenchmarks for query execution, trigram extraction, and index
building.

## Usage

### Build the index

```bash
tgrep index .                          # index current directory
tgrep index /path/to/repo             # index a specific repo
tgrep index . --index-path /tmp/idx   # custom index location
tgrep index . --exclude vendor --exclude third_party  # skip directories
```

Each build reports its elapsed time and peak memory when it finishes:

```
Index built successfully at /tmp/idx
Indexed in 22.6s using external strategy (peak memory 160.1 MiB)
```

The peak is the memory the process itself holds — private/committed bytes, not
resident set. The two differ once large files are memory-mapped: mapped pages are
file-backed and reclaimable, so counting them would report the size of the files
being indexed rather than tgrep's own use. Indexing a single 2 GiB file holds
77.8 MiB while its working set reaches 1.99 GiB. When the working set is
substantially larger it is named alongside, so nothing is hidden:

```
Indexed in 46.1s using external strategy (peak memory 77.8 MiB private, 1.99 GiB working set incl. memory-mapped files)
```

#### Repositories without a `.git` directory

`.gitignore` files only take effect inside a git repository. This matches
ripgrep, which gates them the same way, but it surprises people indexing a
Perforce, Source Depot, or plain-directory enlistment: the root `.gitignore` is
read by nothing, and the only symptom is an index far larger than expected.

tgrep says so rather than leaving you to guess:

```
Walking /src/enlistment...
warning: /src/enlistment has a .gitignore but is not a git repository, so it is
not applied (this matches ripgrep). Pass --no-require-git to apply it.
Found 290018 text files (2893 binary skipped, 0 too large, 0 errors)
```

`--no-require-git` applies the rules anyway, and works on `index`, `serve`, and
search alike, so the index and your queries agree on which files exist:

```bash
tgrep index . --no-require-git
tgrep serve --no-require-git
```

#### Case-insensitive repositories

When git clones onto a filesystem that does not distinguish case — which on
Windows is every clone — it sets `core.ignorecase` and stops distinguishing case
when it matches ignore rules. A rule spelled `QLogs` then hides a directory
named `qlogs`.

Most tools, ripgrep included, always match ignore rules case-sensitively, so
that directory is walked, read and indexed even though `git status` never
mentions it. On one Windows enlistment that was a single 13.4 GiB build
artifact, 71% of the corpus, adding about 16 seconds to *every* query.

tgrep reads `core.ignorecase` and matches the way the repository itself does.
Files git **tracks** are exempt, which is git's own rule — ignore rules only
decide the fate of files git does not already know about. Without that
exemption the same change would have hidden 273 tracked `.JPG`, `.PNG` and
`.RLL` files caught by rules written in lower case.

On that enlistment the walk went from listing one file more than
`git ls-files --cached --others --exclude-standard` to matching it exactly, at a
cost of roughly 0.4 s on a 293k-file walk. `--no-ignore` turns it off along with
every other ignore source, and repositories that distinguish case are unaffected
and pay nothing.

Only the repository's own root `.gitignore` and `.git/info/exclude` are matched
this way. Rules in nested `.gitignore` files are not, because the walk does not
know they exist until it reaches their directory. Missing one only leaves a file
visible that git would hide, which is what every other tool does anyway.

#### Keep `index` and `serve` flags in step

Flags that decide *which files belong in the index* — `--no-require-git`,
`--no-ignore`, `--max-filesize`, `--exclude` — must match between the `tgrep
index` that built an index and the `tgrep serve` that serves it.

The server compares the index against the filesystem at startup and treats an
indexed file it cannot see as deleted. So serving an index built without a cap
under a `--max-filesize 8M` server drops every file above 8 MiB from that index,
permanently. Both sides default to 64 MiB, so this only bites when one side
names a limit; pass the same flags to both:

```bash
tgrep index . --max-filesize 8M
tgrep serve   --max-filesize 8M
```

#### Memory use on very large repos

Builds default to `--index-strategy=external`, which bounds peak memory with an
external merge sort: postings accumulate in a fixed-size arena that spills
sorted, compact segments to disk when full, and the segments are k-way merged
straight into the index. Peak memory is roughly flat in repo size rather than
linear.

If the arena never fills, nothing is spilled and the build takes exactly the
in-memory path, so small and mid-size repos pay nothing for this default.

```bash
tgrep index .                                 # external, 64 MB arena
tgrep index . --index-buffer 16               # smaller arena, lower peak
tgrep index . --index-strategy=memory         # opt out: sort entirely in RAM
```

On the Linux kernel (94,634 files, 990 MiB index), measured under the 1 MiB
indexing cap that was the default at the time, the default strategy is a **~17x
reduction in peak memory, and no slower**:

| Strategy | Spill segments | Peak working set | Build |
| --- | ---: | ---: | ---: |
| `external` (default, 64 MiB arena) | 31 | **160.1 MiB** | 22.6 s |
| `external --index-buffer 16` | 122 | 109.6 MiB | ~23 s |
| `memory` | - | 2.20 - 3.76 GiB | 23 - 32 s |

`--index-buffer` trades peak memory against merge fan-in. Bounded memory is also
*predictable* memory — the `memory` row varied by over a gigabyte across
identical runs because `Vec` growth doubles and both buffers are briefly
resident during the final reallocation, while `external` varied by 8 MiB.

The 1 MiB indexing cap those rows were measured under is now 64 MiB, and files
past 1 MiB are memory-mapped rather than read onto the heap. On the same repo the
`external` build now settles at roughly **152 MiB of private memory in 27 s**,
against 197-200 MiB and 41-42 s when every admitted file was read onto the heap.
The arena bound is unchanged; what changed is that a handful of 20 MB generated
headers no longer cost their full size in heap in every worker that touches one.
The 152 MiB figure was taken with no cap at all, and the 64 MiB default excludes
only files *above* 64 MiB, so it leaves these numbers alone here: the largest file
in the kernel tree is a 22.9 MiB generated AMD register header, and nothing in its
95,862 files reaches the cap.

Two caveats on reading those numbers. The peak tgrep prints is *private bytes*,
which excludes mapped file pages, so it reports what the process actually holds
rather than the size of the files it is reading — the same build is 152 MiB
private against ~192 MiB resident, and the working set is named alongside only
when it is substantially larger, as in [Build the index](#build-the-index).
macOS is the exception: `libc` does not surface the Mach counter that separates
the two, so it still reports resident set there. And a very large file that is
neither valid UTF-8 nor detectably binary still costs about its own size, because
the index has to hold the same repaired bytes a search will match against; a
135 MB Latin-1 file indexes at roughly 205 MiB.

Pass `--max-filesize` if a build has to fit a tighter budget, or
`--no-max-filesize` to lift the 64 MiB default entirely.

`--index-strategy=memory` remains available as an escape hatch for environments
where spilling is undesirable or impossible, such as a read-only or full index
volume. Both strategies produce byte-identical indexes from the same walk —
note that file IDs follow walk order, which the parallel walker does not fix
between runs, so two builds of the same tree need not be byte-identical to each
other. See
[BENCHMARKS.md](BENCHMARKS.md#index-build-strategies) for full numbers.

`tgrep serve` uses the same bounded builder when it has to create an index from
scratch, so starting a server on an unindexed repo costs the same memory as
`tgrep index` (**148.6 MiB** rather than 1.6 GiB on the Linux kernel, and 2.6x
faster). While that first build runs the server answers from an empty index
rather than a partial one; incremental updates after it completes are
unaffected.

### Start the server

```bash
tgrep serve .                          # start server (auto-builds index if missing)
tgrep serve . --index-path /tmp/idx    # custom index location
tgrep serve . --no-watch               # skip file watcher (saves memory)
tgrep serve . --exclude node_modules   # exclude directories from indexing
```

The server builds the index in the background if none exists, and serves
queries immediately from partial data. Multiple clients can connect
simultaneously.

Resource use during that initial build can be tuned. These apply to both
`tgrep serve` and `tgrep index`:

| Flag | Default | Effect |
|------|---------|--------|
| `--max-memory <MB>` | 50% of RAM (512 MB–16 GB) | Flush to disk once the in-memory index exceeds this, bounding peak memory |
| `--max-cpu <PERCENT>` | `50` | Confine parallel reading and trigram extraction to this share of logical cores |
| `--auto-save-mutations <N>` | `5000` | Accumulated index changes that trigger a background save; higher means fewer pauses but more to redo if killed |
| `--watcher-queue-cap <N>` | `16384` | Filesystem events buffered between the OS watcher and the indexing worker; raise it if bulk changes log watcher queue overflows, since each overflow forces a full stale check |

#### Staying in step with the filesystem

Once the index is built, everything that changes it arrives as an OS
notification, and a notification can go missing — a queue overflow, a network
or virtualised filesystem that declines to report a change, a tree replaced
wholesale by a branch switch or a build. Overflow is detected and repaired at
once; the rest is silent, and nothing else in the server revisits a file it
believes it already knows. A missed change would otherwise last until that
file happened to change again, which for a deleted file is never.

So a watching server also reconciles on a timer: about once an hour it walks
the tree and compares it against the index, which finds any drift regardless
of what the watcher heard. It waits for a two-minute gap in queries first, and
gives up waiting after four hours so a continuously busy server still
reconciles. On an unchanged tree it finds nothing and leaves the index alone.
`--no-watch` turns it off along with the watcher.

On Linux and Android, tgrep registers only the non-ignored directories with
inotify, avoiding watch-descriptor growth beneath ignored trees. This guarantee
is backend-specific: the implementation intentionally keeps one recursive
`ReadDirectoryChangesW` root subscription on Windows and one root FSEvents
stream on macOS, where ignored events are filtered after delivery and ignored
descendants remain watched. kqueue and `PollWatcher` are not covered.

### Search

```bash
tgrep "pattern" .                 # basic regex search
tgrep "pattern" file1.rs file2.rs # search multiple files/paths
tgrep "TODO|FIXME" .              # alternations
tgrep '\w+(?!_test)' .            # PCRE-style lookahead fallback
tgrep "error" . -i                # case-insensitive
tgrep "error" . -S                # smart-case (auto if all lowercase)
tgrep -F "Vec<T>" .               # literal string
tgrep "MyStruct" . -l             # filenames only
tgrep "pattern" . -c              # count per file
tgrep "pattern" . -o              # only matching text
tgrep "pattern" . -w              # whole word
tgrep "pattern" . -v              # invert match
tgrep "pattern" . -m 5            # max 5 matches per file
tgrep "pattern" . -g "*.rs"       # glob filter
tgrep "pattern" . -g "*.rs" -g "*.toml"  # multiple globs (OR)
tgrep "pattern" . -t rust         # type filter
tgrep "pattern" . -e "also_this"  # multiple patterns
tgrep "pattern" . -A 3            # 3 lines after match
tgrep "pattern" . -B 2            # 2 lines before match
tgrep "pattern" . -C 3            # 3 lines before & after
tgrep "pattern" . --json          # ripgrep-compatible JSON stream
tgrep "pattern" . --vimgrep       # vim-compatible output
tgrep "pattern" . --stats         # show query plan & timing
tgrep "pattern" . --no-index      # brute-force (skip index)
tgrep "pattern" . -U              # multiline matching
tgrep "pattern" . -q              # quiet: exit code only
tgrep "pattern" . --files-without-match  # files that DON'T match
tgrep "pattern" . --no-filename   # suppress filenames
tgrep "pattern" . -N              # suppress line numbers
tgrep --files .                   # list searchable files
tgrep --files src/main.rs         # list a single file if searchable
tgrep --files -t rust .           # list Rust files only
tgrep --type-list                 # show all file types
```

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
  Indexing:   complete
```

### Count files

```bash
tgrep count-files .              # count text files (no server needed)
tgrep count-files /path/to/repo  # scan a specific repo
```

Prints the count to stdout (scriptable) and details to stderr:

```
284957
284957 text files (47516 binary skipped, 0 errors) in 1200ms
```

## CLI Flags

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
| `-c, --count` | Print match count per file |
| `-l, --files-with-matches` | Print only filenames |
| `--files-without-match` | Print files that do NOT match |
| `-q, --quiet` | Suppress output; exit code only |
| `-m, --max-count <N>` | Limit matches per file |
| `-g, --glob <GLOB>` | Filter files by glob pattern, case-sensitive (repeatable) |
| `--iglob <GLOB>` | Case-insensitive glob filter (repeatable) |
| `--glob-case-insensitive` | Treat all `-g` globs as case-insensitive |
| `-t, --type <TYPE>` | Filter by file type (`rust`, `py`, `js`, …; repeatable) |
| `-T, --type-not <TYPE>` | Exclude a file type (repeatable) |
| `--type-add <SPEC>` | Add/extend a type, e.g. `--type-add 'web:*.html'` |
| `--type-clear <TYPE>` | Remove a type's definitions |
| `--type-list` | Print all supported file types (reflects `--type-add`/`--type-clear`) |
| `--files` | List files that would be searched |
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
| `--no-index` | Skip index, grep all files |
| `--exclude <DIR>` | Exclude directory from indexing (repeatable) |
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
| `-E, --encoding <LABEL>` | Decode files as `LABEL` (e.g. `utf-16le`, `latin1`, `sjis`), or `none` for raw bytes |
| `--no-encoding` | Restore BOM-sniffing auto-detection |

By default tgrep sniffs a UTF-8/UTF-16LE/UTF-16BE BOM and decodes accordingly,
so BOM-marked UTF-16 files are searched as text rather than reported as binary.
A BOM always wins over `-E`, matching ripgrep. Because the index is built with
auto-detection, an explicit `-E` bypasses the index and server and searches
files directly, so results stay correct.

**File walking**

| Flag | Description |
|------|-------------|
| `--max-depth <N>` | Limit directory recursion depth |
| `--one-file-system` | Don't cross file-system boundaries |
| `--ignore-file <FILE>` | Read extra ignore rules from `FILE` (repeatable) |
| `--ignore-file-case-insensitive` | Match ignore rules case-insensitively |
| `--no-ignore-dot` | Ignore `.ignore` files |
| `--no-ignore-exclude` | Ignore `.git/info/exclude` |
| `--no-ignore-files` | Ignore any `--ignore-file` arguments |
| `--no-ignore-global` | Ignore the global gitignore |
| `--no-ignore-parent` | Ignore rules from parent directories |
| `--no-ignore-vcs` | Ignore `.gitignore` files |
| `--no-ignore-messages` | Suppress errors about malformed ignore files |
| `--no-require-git` | Apply git ignore rules outside a git repository |
| `-j, --threads <N>` | Number of search threads |

**Accepted for compatibility**

`--mmap`/`--no-mmap` (tgrep always reads files directly), `--crlf`/`--no-crlf`
(a trailing `\r` is always stripped), `--no-config` (tgrep reads no config
file), and `--colors <SPEC>` (colors are not yet configurable) are accepted and
ignored so ripgrep command lines keep working. `--debug`/`--trace` imply
`--stats`.

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

`-m/--max-count` limits matching *lines*, as ripgrep does, not individual
matches. A line holding several matches spends one unit of the budget and all
of its matches are still reported, so `tgrep -m1 --vimgrep foo` prints one row
per match on the first matching line. Under `-U/--multiline` the unit is the
contiguous block of lines a match covers, which keeps a match that straddles a
line boundary whole instead of truncating it mid-pattern.

One divergence: ripgrep stops reading a file once the limit is reached, so its
`--stats` `bytes_searched` is lower than tgrep's, which searches from a
whole-file buffer. The match counts themselves agree.

### Multiline matches

`-U/--multiline` lets a match cross line boundaries, and every line a match
covers is printed. `--vimgrep` is the exception: it reports one row per match
so editors get one jump target each, so a match spanning several lines is
reported only on the line it starts on.

Two divergences, both cases where ripgrep names a column that doesn't exist on
the line it prints — `rg -U --column` reports the same column for every line of
a match, and `rg -U --vimgrep -o` can report column 19 of a 7-character line.
tgrep reports the real match position instead.

### Binary files

A file is binary if it contains a NUL byte. Following ripgrep:

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

tgrep additionally rejects ~65 binary file *extensions* during the walk to keep
indexing cheap, which ripgrep does not do. `--binary` and `-a` also lift that
restriction, and `--files` never applies it.

### Flags that bypass the index

The index is built over text files only, skipping hidden and ignored ones, so
flags that widen or re-interpret that set are answered by walking the tree
instead: `-E/--encoding`, `-a/--text`, `--binary`, `-./--hidden`, and every
`--no-ignore*` variant. Naming a single file also skips the index, since
reading one file directly is cheaper than loading one.

### Invalid UTF-8

ripgrep searches raw bytes. tgrep decodes first, repairing any undecodable byte
into a `U+FFFD`, which is what makes the trigram index possible. Reported
positions are mapped back, so `--column`, `-b`, `--vimgrep` and `-r` all report
the byte offsets on disk exactly as ripgrep does. Two differences remain on
lines that are not valid UTF-8:

- A pattern can match the substituted `U+FFFD`; ripgrep, seeing raw bytes, never
  matches there. So `tgrep '.' ` finds one more match per repaired byte.
- `--json` always reports `lines.text` with the substitutions in place, and
  submatch offsets that index it. ripgrep instead emits `lines.bytes` as base64
  and reports source offsets. `absolute_offset` is a real file offset either
  way.

### File size limits

Searching and indexing both skip files larger than **64 MiB** by default. This
is a deliberate divergence from ripgrep, which has no default limit.

The divergence is affordable because tgrep is not a one-shot scanner. A file a
walk picks up is also a file the index carries and re-reads on every query whose
trigrams make it a candidate, so an outlier's cost is paid repeatedly rather
than once. On a 292,911-file enlistment where one 13.41 GiB generated build
artifact was 71% of all searchable bytes, the cap cut a cold index build from
214.5 s to 64.2 s and a warm query from 21.30 s to 0.55 s — a 39x difference —
at the cost of one match in one generated file, and it excluded 2 files out of
292,911.

The cost is real: an oversized file is counted but its path is never recorded,
so a match inside one is reported as no match. Two rules keep that from being
silent:

- `--no-max-filesize` restores the uncapped, ripgrep-identical behaviour, and
  `--max-filesize` sets a different bound.
- A file named directly on the command line is never dropped by the *inherited*
  default, only by a limit you passed. `tgrep pattern ./huge.log` searches
  `huge.log`.

The limit is resolved once, before the walk and the search diverge, so an index
and the queries against it always agree on which files exist. A walk that capped
where the search did not would be indistinguishable from "this file contains no
match".

The cap is not what bounds memory. Files past 1 MiB are memory-mapped during
both indexing and searching, so their pages are file-backed and reclaimable, and
mapped files are batched by what they actually put on the heap rather than by
their length — so a build over large files fills its worker pool instead of
running two files at a time. Combined with a faster trigram extractor, that made
indexing a tree of 32 MiB source files about 14x faster. Nor is size a good
proxy for *index* cost: oversized files are overwhelmingly generated and
therefore repetitive, contributing far fewer distinct trigrams per byte than
ordinary source. What the cap buys is bounded *scan* time. See
[BENCHMARKS.md](BENCHMARKS.md).

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

1. **Indexing** — walks the repo (respecting `.gitignore` and root-level
   `p4ignore.ini`), skips binary files
   by extension (50+ formats) and content check (first 8KB), extracts all
   overlapping 3-byte trigrams from each text file in parallel (rayon), and
   writes a compact binary inverted index. Full builds stream sorted posting
   groups directly to disk to keep peak memory bounded.

2. **Querying** — the regex is parsed with `regex-syntax`, decomposed into
   literal fragments, converted to trigram hashes, and looked up via binary
   search in the mmap'd index. Posting lists are intersected (AND) or
   unioned (OR) to find candidate files, reusing sorted posting-list order when
   possible. Only those candidates are verified with the full regex engine in
   parallel (rayon).

3. **Serving** — `tgrep serve` wraps the index in a HybridIndex, watches for
   filesystem changes, and serves queries over TCP. If no index exists, it
   builds one in the background (batches of 500 files, parallel extraction)
   while serving queries from partial data. The index is flushed to disk
   every 50K files or 5 minutes. Multiple clients connect simultaneously;
   searches use read locks for zero contention.

## On-Disk Format

| File | Description |
|------|-------------|
| `lookup.bin` | Sorted 16-byte entries: `trigram(u32) + offset(u64) + length(u32)` |
| `index.bin` | Concatenated posting lists: `file_id(u32)` per entry |
| `files.bin` | File ID→path mapping: `file_id(u32) + path_len(u16) + path_bytes` |
| `meta.json` | Version, file/trigram counts, timestamps |
| `serve.json` | Server PID and TCP port (for client discovery) |

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Matches found |
| 1 | No matches |
| 2 | Error |

## Project Structure

```
tgrep/
├── tgrep-core/               # Library crate
│   └── src/
│       ├── trigram.rs            # Trigram extraction & hashing
│       ├── filetypes.rs          # File type definitions (rust, py, js, …)
│       ├── walker.rs             # Git/Perforce ignore-aware file traversal
│       ├── ondisk.rs             # On-disk binary format
│       ├── builder.rs            # Index construction (parallel via rayon)
│       ├── reader.rs             # Mmap'd index reader
│       ├── query.rs              # Regex → trigram query decomposition
│       ├── live.rs               # LiveIndex (in-memory mutable overlay)
│       ├── hybrid.rs             # HybridIndex (reader + live overlay)
│       ├── meta.rs               # Index metadata
│       └── error.rs              # Error types
└── tgrep-cli/                # Binary crate
    └── src/
        ├── main.rs               # CLI entry (clap)
        ├── index.rs              # `tgrep index`
        ├── search.rs             # `tgrep <pattern>` with server delegation
        ├── serve.rs              # `tgrep serve` (TCP JSON-RPC + file watcher)
        ├── status.rs             # `tgrep status`
        └── output.rs             # Output formatting
```

## Building

```bash
cargo build --release    # build optimized binary
make check               # run fmt + clippy + tests
make install             # install to ~/.cargo/bin
```

## Installation

### From source

```bash
git clone https://github.com/microsoft/tgrep.git
cd tgrep
cargo install --path tgrep-cli --locked
```

### Pre-built binaries

Download from [GitHub Releases](https://github.com/microsoft/tgrep/releases)
for Linux, macOS (Intel & Apple Silicon), and Windows.

```bash
# Linux (x86_64)
gh release download --repo microsoft/tgrep -p '*x86_64-unknown-linux-musl*' -D /tmp/tgrep-dl
tar xzf /tmp/tgrep-dl/tgrep-*-x86_64-unknown-linux-musl.tar.gz -C ~/.local/bin

# macOS (Apple Silicon)
gh release download --repo microsoft/tgrep -p '*aarch64-apple-darwin*' -D /tmp/tgrep-dl
tar xzf /tmp/tgrep-dl/tgrep-*-aarch64-apple-darwin.tar.gz -C /usr/local/bin

# macOS (Intel)
gh release download --repo microsoft/tgrep -p '*x86_64-apple-darwin*' -D /tmp/tgrep-dl
tar xzf /tmp/tgrep-dl/tgrep-*-x86_64-apple-darwin.tar.gz -C /usr/local/bin
```

```powershell
# Windows (PowerShell)
gh release download --repo microsoft/tgrep -p '*windows*' -D $env:TEMP\tgrep-dl
Expand-Archive $env:TEMP\tgrep-dl\tgrep-*-windows*.zip -DestinationPath $HOME\.cargo\bin -Force
```

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
