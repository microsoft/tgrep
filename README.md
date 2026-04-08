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

See [full benchmark results](BENCHMARKS.md) — up to **35x faster** than ripgrep on large repos.

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
- **Smart file walking** — extension-based binary rejection (50+ formats),
  8KB content check, 1MB file size limit
- **Lock-free reads** — `RwLock<HashMap>` cache allows concurrent reads
  without contention
- **Hot serving** — queries work immediately during background index building;
  no need to wait for full index

## Usage

### Build the index

```bash
tgrep index .                          # index current directory
tgrep index /path/to/repo             # index a specific repo
tgrep index . --index-path /tmp/idx   # custom index location
tgrep index . --exclude vendor --exclude third_party  # skip directories
```

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

### Search

```bash
tgrep "pattern" .                 # basic regex search
tgrep "TODO|FIXME" .              # alternations
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
tgrep "pattern" . --json          # JSON output
tgrep "pattern" . --vimgrep       # vim-compatible output
tgrep "pattern" . --stats         # show query plan & timing
tgrep "pattern" . --no-index      # brute-force (skip index)
tgrep "pattern" . -U              # multiline matching
tgrep "pattern" . -q              # quiet: exit code only
tgrep "pattern" . -L              # files that DON'T match
tgrep "pattern" . --no-filename   # suppress filenames
tgrep "pattern" . -N              # suppress line numbers
tgrep --files .                   # list searchable files
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
| `-S, --smart-case` | Case-insensitive if pattern is all lowercase |
| `-F, --fixed-strings` | Treat pattern as a literal string |
| `-w, --word-regexp` | Match whole words only |
| `-v, --invert-match` | Show lines that do NOT match |
| `-o, --only-matching` | Print only the matched parts |
| `-e, --regexp <PAT>` | Additional pattern (repeatable for OR) |
| `-f, --file <FILE>` | Read patterns from file (one per line) |
| `-U, --multiline` | Enable multiline matching |
| `-n, --line-number` | Show line numbers (default: on) |
| `-N, --no-line-number` | Suppress line numbers |
| `-c, --count` | Print match count per file |
| `-l, --files-with-matches` | Print only filenames |
| `-L, --files-without-match` | Print files that do NOT match |
| `-q, --quiet` | Suppress output; exit code only |
| `-m, --max-count <N>` | Limit matches per file |
| `-g, --glob <GLOB>` | Filter files by glob pattern (repeatable) |
| `-t, --type <TYPE>` | Filter by file type (`rust`, `py`, `js`, …) |
| `--type-list` | Print all supported file types |
| `--files` | List files that would be searched |
| `-A, --after-context <N>` | Lines of context after match |
| `-B, --before-context <N>` | Lines of context before match |
| `-C, --context <N>` | Lines of context before and after |
| `--heading / --no-heading` | Grouped vs flat output |
| `-H, --with-filename` | Show filenames (default: on) |
| `--no-filename` | Suppress filenames in output |
| `--json` | JSON output (one object per line) |
| `--vimgrep` | Vim-compatible `file:line:col:content` |
| `--color auto/always/never` | Color mode control |
| `-0, --null` | NUL byte filename separator (for xargs) |
| `--trim` | Trim leading/trailing whitespace |
| `--hidden` | Include hidden files and directories |
| `--no-ignore` | Don't respect .gitignore files |
| `-u` | Unrestricted: `-u` = no-ignore, `-uu` = +hidden |
| `--no-index` | Skip index, grep all files |
| `--exclude <DIR>` | Exclude directory from indexing (repeatable) |
| `--stats` | Print query plan and candidate stats |
| `--index-path <DIR>` | Custom index directory |

## How It Works

1. **Indexing** — walks the repo (respecting `.gitignore`), skips binary files
   by extension (50+ formats) and content check (first 8KB), extracts all
   overlapping 3-byte trigrams from each text file in parallel (rayon), and
   writes a compact binary inverted index.

2. **Querying** — the regex is parsed with `regex-syntax`, decomposed into
   literal fragments, converted to trigram hashes, and looked up via binary
   search in the mmap'd index. Posting lists are intersected (AND) or
   unioned (OR) to find candidate files. Only those candidates are verified
   with the full regex engine in parallel (rayon).

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
│       ├── walker.rs             # .gitignore-aware file traversal
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
gh release download --repo microsoft/tgrep -p '*x86_64-unknown-linux-gnu*' -D /tmp/tgrep-dl
tar xzf /tmp/tgrep-dl/tgrep-*-x86_64-unknown-linux-gnu.tar.gz -C ~/.local/bin

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
