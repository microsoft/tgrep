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
   fresh as the last successful index publication. If a `serve` was interrupted
   during its first build, an index marked incomplete can remain on disk, and a
   search may still try to use it without warning even though it can be empty or
   partial. Rebuild with `tgrep index .` or resume `tgrep serve .` before an exhaustive search.
3. **No index**: scan every file, like grep. Correct but slow on large trees.
   tgrep prints a warning on stderr when this happens.

An agent never has to choose between these; the command is the same. Results
can differ, though. An on-disk index omits changes since its last build. A
server started with no index at all answers from an empty index, so every
search returns nothing, until the first build completes. A server resuming a
partial index answers from what it has so far. `tgrep status .` shows
`Indexing: complete` once the initial build is done. Do not read it as a
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
  `search`, `status`, `count-files` or `help` as a subcommand;
  `tgrep -- serve .` searches for the word. Everything after `--` is read as
  the pattern and paths, so all flags must come before it.
- **Prefer `-F`** when the query is a symbol or a string the user typed. It
  avoids regex-escaping mistakes.
- **Narrow with `-t` or `-g`** before adding `-m`. The index makes scoping
  cheap; `-m` only trims output.
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
{"data":{"binary_offset":null,"path":{"text":"tgrep-cli/build.rs"},"stats":{"bytes_printed":260,"bytes_searched":1775,"elapsed":{"human":"0.000010s","nanos":9709,"secs":0},"matched_lines":1,"matches":1,"searches":1,"searches_with_match":1}},"type":"end"}
{"data":{"elapsed_total":{"human":"0.000470s","nanos":469542,"secs":0},"stats":{"bytes_printed":515,"bytes_searched":1775,"elapsed":{"human":"0.000470s","nanos":469542,"secs":0},"matched_lines":1,"matches":1,"searches":1,"searches_with_match":1}},"type":"summary"}
```

Any parser written for `rg --json` works as is, with one exception: on a line
that is not valid UTF-8, ripgrep emits base64 `lines.bytes`, while tgrep
always emits `lines.text` with each bad byte replaced by U+FFFD. A consumer
that depends on the raw bytes of such lines will see repaired text instead.

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

These fall back to a full scan even with a server running, because they widen
the file set the index was built over:

- `--hidden`, `--no-ignore` and variants, `-u`/`-uu`/`-uuu`
- `-a`/`--text`, `--binary`, `-E`/`--encoding`
- `--no-index` (explicit)
- naming a single file instead of a directory

Avoid these on large repositories unless you need them.

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
