/// tgrep — trigram-indexed grep with client/server architecture.
///
/// Usage:
///   tgrep index [path]           Build the trigram index
///   tgrep serve [path]           Start the search server
///   tgrep <pattern> [path]       Search (auto-delegates to server)
///   tgrep status [path]          Show index/server status
mod cpu;
mod glob_filter;
mod index;
mod matching;
mod mem;
mod output;
mod search;
mod serve;
mod status;
mod walkcount;

use std::path::PathBuf;
use std::process;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use clap::{Parser, Subcommand};
use output::ColorMode;
use tgrep_core::builder;

/// Set when anything is reported to stderr, mirroring ripgrep's `messages`
/// module, which drives the exit code.
static ERRORED: AtomicBool = AtomicBool::new(false);

#[derive(Parser)]
#[command(
    name = "tgrep",
    about = "Trigram-indexed grep — fast regex search for large codebases",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Search pattern (when not using a subcommand).
    #[arg(global = false)]
    pattern: Option<String>,

    /// Root directories or files to search.
    #[arg(global = false, value_name = "PATH", num_args = 0..)]
    paths: Vec<String>,

    // ── Matching ──────────────────────────────────────
    /// Case-insensitive matching.
    #[arg(short = 'i', long = "ignore-case", global = true)]
    ignore_case: bool,

    /// Force case-sensitive matching (overrides --smart-case).
    #[arg(short = 's', long = "case-sensitive", global = true)]
    case_sensitive: bool,

    /// Smart case: case-insensitive if pattern is all lowercase.
    #[arg(short = 'S', long = "smart-case", global = true)]
    smart_case: bool,

    /// Treat pattern as a literal string.
    #[arg(short = 'F', long = "fixed-strings", global = true)]
    fixed_strings: bool,

    /// Match whole words only.
    #[arg(short = 'w', long = "word-regexp", global = true)]
    word_regexp: bool,

    /// Invert match: show lines that do NOT match.
    #[arg(short = 'v', long = "invert-match", global = true)]
    invert_match: bool,

    /// Additional patterns (can be specified multiple times).
    #[arg(short = 'e', long = "regexp", global = true)]
    regexp: Vec<String>,

    /// Read patterns from a file (one per line).
    #[arg(short = 'f', long = "file", global = true)]
    pattern_file: Option<String>,

    /// Enable multiline matching (patterns may span line boundaries).
    #[arg(short = 'U', long = "multiline", global = true)]
    multiline: bool,

    /// Allow `.` to match a newline. Implies --multiline.
    #[arg(long = "multiline-dotall", global = true)]
    multiline_dotall: bool,

    // ── Output mode ──────────────────────────────────
    /// Print only filenames with matches.
    #[arg(short = 'l', long = "files-with-matches", global = true)]
    files_only: bool,

    /// Print files that do NOT match the pattern.
    #[arg(long = "files-without-match", global = true)]
    files_without_match: bool,

    /// Print match count per file.
    #[arg(short = 'c', long = "count", global = true)]
    count: bool,

    /// Print only the matched parts of a line.
    #[arg(short = 'o', long = "only-matching", global = true)]
    only_matching: bool,

    /// Limit matches per file.
    #[arg(short = 'm', long = "max-count", global = true)]
    max_count: Option<usize>,

    /// List files that would be searched (no search performed).
    #[arg(long = "files", global = true)]
    list_files: bool,

    /// Suppress all output; exit code only (0 = match found, 1 = no match).
    #[arg(short = 'q', long = "quiet", global = true)]
    quiet: bool,

    // ── Filtering ────────────────────────────────────
    /// Filter files by glob pattern (can be specified multiple times).
    #[arg(short = 'g', long = "glob", global = true, action = clap::ArgAction::Append)]
    glob: Vec<String>,

    /// Like --glob, but case-insensitive.
    #[arg(long = "iglob", global = true, action = clap::ArgAction::Append)]
    iglob: Vec<String>,

    /// Treat all --glob patterns as case-insensitive.
    #[arg(long = "glob-case-insensitive", global = true)]
    glob_case_insensitive: bool,

    /// Filter files by type (e.g., rust, py, js). Repeatable. Use --type-list to see all.
    #[arg(short = 't', long = "type", global = true, action = clap::ArgAction::Append)]
    file_type: Vec<String>,

    /// Exclude files matching a type. Repeatable; takes precedence over --type.
    #[arg(short = 'T', long = "type-not", global = true, action = clap::ArgAction::Append)]
    type_not: Vec<String>,

    /// Add or extend a type: `name:glob` or `name:include:type1,type2`.
    #[arg(long = "type-add", global = true, value_name = "SPEC", action = clap::ArgAction::Append)]
    type_add: Vec<String>,

    /// Remove all globs for a type before applying --type-add.
    #[arg(long = "type-clear", global = true, value_name = "NAME", action = clap::ArgAction::Append)]
    type_clear: Vec<String>,

    /// Print all supported file types.
    #[arg(long = "type-list", global = true)]
    type_list: bool,

    /// Ignore files larger than NUM bytes (suffixes K, M, G allowed).
    #[arg(long = "max-filesize", global = true, value_name = "NUM")]
    max_filesize: Option<String>,

    /// Text encoding to use: `auto` (BOM sniffing), `none`, or a label like `utf-16le`.
    #[arg(
        short = 'E',
        long = "encoding",
        global = true,
        value_name = "ENCODING",
        overrides_with = "no_encoding"
    )]
    encoding: Option<String>,

    /// Reset --encoding back to `auto`.
    #[arg(long = "no-encoding", global = true, overrides_with = "encoding")]
    no_encoding: bool,

    /// Search binary files as if they were text.
    #[arg(short = 'a', long = "text", global = true)]
    text: bool,

    // ── Context ──────────────────────────────────────
    /// Lines of context after each match.
    #[arg(short = 'A', long = "after-context", global = true)]
    after_context: Option<usize>,

    /// Lines of context before each match.
    #[arg(short = 'B', long = "before-context", global = true)]
    before_context: Option<usize>,

    /// Lines of context before and after each match.
    #[arg(short = 'C', long = "context", global = true)]
    context: Option<usize>,

    /// Print the file name for each match (default behavior, ripgrep compatibility).
    #[arg(short = 'H', long = "with-filename", global = true)]
    with_filename: bool,

    /// Suppress filenames in output.
    #[arg(short = 'I', long = "no-filename", global = true)]
    no_filename: bool,

    /// Show line numbers (default behavior, ripgrep compatibility).
    #[arg(short = 'n', long = "line-number", global = true)]
    line_number: bool,

    /// Suppress line numbers in output.
    #[arg(short = 'N', long = "no-line-number", global = true)]
    no_line_number: bool,

    // ── Output formatting ────────────────────────────
    /// Group matches by file with heading.
    #[arg(long = "heading", global = true)]
    heading: bool,

    /// Don't group matches; flat output.
    #[arg(long = "no-heading", global = true)]
    no_heading: bool,

    /// JSON output (one object per line).
    #[arg(long = "json", global = true)]
    json: bool,

    /// Output in vim-compatible format (file:line:col:content).
    #[arg(long = "vimgrep", global = true)]
    vimgrep: bool,

    /// Color mode: auto, always, or never.
    #[arg(long = "color", default_value = "auto", global = true)]
    color: String,

    /// Use NUL byte as filename separator (for xargs -0).
    #[arg(short = '0', long = "null", global = true)]
    null: bool,

    /// Trim leading/trailing whitespace from each line.
    #[arg(long = "trim", global = true)]
    trim: bool,

    // ── Index control ────────────────────────────────
    /// Print query plan and timing stats.
    #[arg(long = "stats", global = true)]
    stats: bool,

    /// Skip the index, grep all files directly.
    #[arg(long = "no-index", global = true)]
    no_index: bool,

    /// Custom index directory.
    #[arg(long = "index-path", global = true)]
    index_path: Option<PathBuf>,

    // ── File discovery ───────────────────────────────
    /// Include hidden files and directories.
    #[arg(short = '.', long = "hidden", global = true)]
    hidden: bool,

    /// Don't respect .gitignore or p4ignore.ini files.
    #[arg(long = "no-ignore", global = true)]
    no_ignore: bool,

    /// Follow symbolic links while searching.
    #[arg(short = 'L', long = "follow", global = true)]
    follow: bool,

    /// Suppress error messages about nonexistent or unreadable files.
    #[arg(long = "no-messages", global = true)]
    no_messages: bool,

    /// Unrestricted search. -u = no-ignore, -uu = +hidden, -uuu = +binary.
    #[arg(short = 'u', long = "unrestricted", action = clap::ArgAction::Count, global = true)]
    unrestricted: u8,

    /// Search binary files, reporting a note instead of printing their lines.
    #[arg(long = "binary", global = true)]
    binary: bool,

    // ── Matching ─────────────────────────────────────
    /// Only match when the whole line matches the pattern.
    #[arg(short = 'x', long = "line-regexp", global = true)]
    line_regexp: bool,

    /// Use the PCRE-style engine, enabling lookaround and backreferences.
    #[arg(short = 'P', long = "pcre2", global = true)]
    pcre2: bool,

    /// Regex engine to use: default, pcre2, or auto.
    #[arg(
        long = "engine",
        value_name = "ENGINE",
        default_value = "auto",
        global = true
    )]
    engine: String,

    /// Print the PCRE-style engine version and exit.
    #[arg(long = "pcre2-version", global = true)]
    pcre2_version: bool,

    /// Disable Unicode-aware matching.
    #[arg(long = "no-unicode", global = true)]
    no_unicode: bool,

    /// Upper size limit for the compiled regex (suffixes K, M, G allowed).
    #[arg(long = "regex-size-limit", value_name = "NUM", global = true)]
    regex_size_limit: Option<String>,

    /// Upper size limit for the regex DFA cache (suffixes K, M, G allowed).
    #[arg(long = "dfa-size-limit", value_name = "NUM", global = true)]
    dfa_size_limit: Option<String>,

    /// Replace each match with TEXT. Capture groups are available as $1, ${name}.
    #[arg(short = 'r', long = "replace", value_name = "TEXT", global = true)]
    replace: Option<String>,

    /// Print both matching and non-matching lines.
    #[arg(long = "passthru", global = true)]
    passthru: bool,

    /// Stop searching a file after a line that does not match.
    #[arg(long = "stop-on-nonmatch", global = true)]
    stop_on_nonmatch: bool,

    // ── Output detail ────────────────────────────────
    /// Show the column number of the first match on each line.
    #[arg(long = "column", global = true, overrides_with = "no_column")]
    column: bool,

    /// Don't show column numbers.
    #[arg(long = "no-column", global = true, overrides_with = "column")]
    no_column: bool,

    /// Print the 0-based byte offset of each output line.
    #[arg(short = 'b', long = "byte-offset", global = true)]
    byte_offset: bool,

    /// Don't print lines longer than NUM bytes.
    #[arg(short = 'M', long = "max-columns", value_name = "NUM", global = true)]
    max_columns: Option<usize>,

    /// Print a truncated preview instead of suppressing a long line entirely.
    #[arg(long = "max-columns-preview", global = true)]
    max_columns_preview: bool,

    /// Count individual matches instead of matching lines.
    #[arg(long = "count-matches", global = true)]
    count_matches: bool,

    /// Print a count of zero for files with no match.
    #[arg(long = "include-zero", global = true)]
    include_zero: bool,

    /// Alias for --color always --heading --line-number.
    #[arg(short = 'p', long = "pretty", global = true)]
    pretty: bool,

    /// String printed between non-contiguous context blocks.
    #[arg(long = "context-separator", value_name = "SEP", global = true)]
    context_separator: Option<String>,

    /// Never print a context separator.
    #[arg(long = "no-context-separator", global = true)]
    no_context_separator: bool,

    /// Separator between the path/line/column fields of a matching line.
    #[arg(long = "field-match-separator", value_name = "SEP", global = true)]
    field_match_separator: Option<String>,

    /// Separator between the path/line/column fields of a context line.
    #[arg(long = "field-context-separator", value_name = "SEP", global = true)]
    field_context_separator: Option<String>,

    /// Character to use as the path separator in output.
    #[arg(long = "path-separator", value_name = "SEP", global = true)]
    path_separator: Option<String>,

    /// Sort results. Choices: none, path, modified, accessed, created.
    #[arg(long = "sort", value_name = "SORTBY", global = true)]
    sort: Option<String>,

    /// Sort results in descending order. Same choices as --sort.
    #[arg(long = "sortr", value_name = "SORTBY", global = true)]
    sortr: Option<String>,

    /// Deprecated alias for --sort path.
    #[arg(long = "sort-files", global = true)]
    sort_files: bool,

    // ── File discovery (advanced) ────────────────────
    /// Descend at most NUM directories below each search path.
    #[arg(long = "max-depth", value_name = "NUM", global = true)]
    max_depth: Option<usize>,

    /// Don't cross file system boundaries.
    #[arg(long = "one-file-system", global = true)]
    one_file_system: bool,

    /// Read extra ignore globs from PATH. Repeatable; later files take precedence.
    #[arg(long = "ignore-file", value_name = "PATH", global = true, action = clap::ArgAction::Append)]
    ignore_file: Vec<String>,

    /// Match --ignore-file globs case-insensitively.
    #[arg(long = "ignore-file-case-insensitive", global = true)]
    ignore_file_case_insensitive: bool,

    /// Don't respect .ignore files.
    #[arg(long = "no-ignore-dot", global = true)]
    no_ignore_dot: bool,

    /// Don't respect .git/info/exclude.
    #[arg(long = "no-ignore-exclude", global = true)]
    no_ignore_exclude: bool,

    /// Don't respect --ignore-file arguments.
    #[arg(long = "no-ignore-files", global = true)]
    no_ignore_files: bool,

    /// Don't respect the global gitignore.
    #[arg(long = "no-ignore-global", global = true)]
    no_ignore_global: bool,

    /// Suppress messages about unparseable ignore files.
    #[arg(long = "no-ignore-messages", global = true)]
    no_ignore_messages: bool,

    /// Don't respect ignore files in parent directories.
    #[arg(long = "no-ignore-parent", global = true)]
    no_ignore_parent: bool,

    /// Don't respect .gitignore files.
    #[arg(long = "no-ignore-vcs", global = true)]
    no_ignore_vcs: bool,

    /// Respect .gitignore files even outside a git repository.
    #[arg(long = "no-require-git", global = true)]
    no_require_git: bool,

    // ── Accepted for ripgrep compatibility ───────────
    /// Number of threads to use. tgrep sizes its pool automatically.
    #[arg(short = 'j', long = "threads", value_name = "NUM", global = true)]
    threads: Option<usize>,

    /// Accepted for compatibility; tgrep always reads files directly.
    #[arg(long = "mmap", global = true, overrides_with = "no_mmap")]
    mmap: bool,

    /// Accepted for compatibility; tgrep always reads files directly.
    #[arg(long = "no-mmap", global = true, overrides_with = "mmap")]
    no_mmap: bool,

    /// Flush output on every line.
    #[arg(
        long = "line-buffered",
        global = true,
        overrides_with = "block_buffered"
    )]
    line_buffered: bool,

    /// Buffer output in blocks (the default when not writing to a terminal).
    #[arg(
        long = "block-buffered",
        global = true,
        overrides_with = "line_buffered"
    )]
    block_buffered: bool,

    /// Accepted for compatibility; tgrep reads no configuration file.
    #[arg(long = "no-config", global = true)]
    no_config: bool,

    /// Accepted for compatibility; tgrep's colors are not configurable yet.
    #[arg(long = "colors", value_name = "SPEC", global = true, action = clap::ArgAction::Append)]
    colors: Vec<String>,

    /// Print debug messages to stderr (implies `--stats`).
    #[arg(long = "debug", global = true)]
    debug: bool,

    /// Print verbose trace messages to stderr (implies `--stats`).
    #[arg(long = "trace", global = true)]
    trace: bool,

    /// Accepted for compatibility; tgrep always strips a trailing `\r`.
    #[arg(long = "crlf", global = true, overrides_with = "no_crlf")]
    crlf: bool,

    /// Accepted for compatibility; tgrep always strips a trailing `\r`.
    #[arg(long = "no-crlf", global = true, overrides_with = "crlf")]
    no_crlf: bool,

    /// Not supported: tgrep does not decompress archives before searching.
    #[arg(short = 'z', long = "search-zip", global = true)]
    search_zip: bool,
}

/// Every argument that needs parsing and can fail, resolved once up front.
struct ResolvedArgs {
    max_filesize: Option<u64>,
    encoding: tgrep_core::encoding::EncodingMode,
    engine: matching::RegexEngine,
    regex_size_limit: Option<usize>,
    dfa_size_limit: Option<usize>,
    sort: Option<search::SortMode>,
}

/// Parse a `--sort`/`--sortr` value. `none` means "leave results unordered".
fn parse_sort(key: &str, reverse: bool) -> Result<Option<search::SortMode>> {
    if key == "none" {
        return Ok(None);
    }
    let key = search::SortKey::from_str_opt(key)
        .ok_or_else(|| anyhow::anyhow!("unrecognized sort criteria: {key}"))?;
    Ok(Some(search::SortMode { key, reverse }))
}

/// Parse a byte size, accepting K/M/G suffixes like ripgrep.
///
/// `flag` names the option in the error message so a bad `--regex-size-limit`
/// does not report a problem with `--max-filesize`.
fn parse_byte_size(flag: &str, s: &str) -> Result<u64> {
    let s = s.trim();
    let (digits, mult) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1024u64),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let n: u64 = digits
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid {flag} value: {s}"))?;
    n.checked_mul(mult)
        .ok_or_else(|| anyhow::anyhow!("{flag} value is too large: {s}"))
}

/// Parse a size limit that the regex engines take as a `usize`.
///
/// ripgrep stores these limits as `usize` and rejects a value that does not
/// fit ("size is too big") instead of truncating it. Truncating would apply a
/// silently different limit than the one asked for on a 32-bit target, so
/// reject it here too.
fn parse_size_limit(flag: &str, s: &str) -> Result<usize> {
    let bytes = parse_byte_size(flag, s)?;
    usize::try_from(bytes)
        .map_err(|_| anyhow::anyhow!("{flag} value is too large for this platform: {s}"))
}

/// Posting accumulation strategy for `tgrep index`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum IndexStrategyArg {
    /// Accumulate all postings in memory and sort once.
    Memory,
    /// Bound peak memory with an external merge sort that spills to disk.
    External,
}

impl From<IndexStrategyArg> for builder::IndexStrategy {
    fn from(value: IndexStrategyArg) -> Self {
        match value {
            IndexStrategyArg::Memory => Self::InMemory,
            IndexStrategyArg::External => Self::External,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Build or rebuild the trigram index.
    Index {
        /// Root directory to index.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Force a full rebuild.
        #[arg(long)]
        force: bool,

        /// Exclude directories from indexing (can be specified multiple times).
        #[arg(long = "exclude", action = clap::ArgAction::Append)]
        exclude: Vec<String>,

        /// How postings are accumulated during the build.
        ///
        /// `external` (default) bounds peak memory with an external merge
        /// sort, spilling sorted segments to disk when the arena fills. If the
        /// arena never fills it is identical to `memory`, so small repos are
        /// unaffected. `memory` holds every posting in RAM and sorts once;
        /// peak memory then grows with repo size, unbounded.
        #[arg(
            long = "index-strategy",
            value_name = "STRATEGY",
            default_value = "external"
        )]
        strategy: IndexStrategyArg,

        /// Arena size in megabytes before `--index-strategy=external` spills to
        /// disk. Lower values reduce peak memory at the cost of more spill
        /// segments to merge. Ignored by `--index-strategy=memory`.
        #[arg(long = "index-buffer", value_name = "MB", value_parser = clap::value_parser!(u64).range(1..))]
        index_buffer_mb: Option<u64>,
    },

    /// Start the persistent search server.
    Serve {
        /// Root directory to serve.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Disable the file system watcher (saves memory on large repos).
        #[arg(long)]
        no_watch: bool,

        /// Maximum memory budget in megabytes for the in-memory index built
        /// during the initial scan. When the indexer's working set exceeds
        /// this, it flushes to disk and continues, keeping peak memory bounded
        /// while still producing a complete index. Defaults to 50% of physical
        /// RAM (clamped between 512 MB and 16 GB).
        #[arg(long = "max-memory", value_name = "MB", value_parser = clap::value_parser!(u64).range(1..))]
        max_memory_mb: Option<u64>,

        /// Maximum CPU budget for the initial index build, as a percentage of
        /// logical cores (1-100). The parallel file-reading/trigram-extraction
        /// work is confined to this fraction of cores so the host stays
        /// responsive. Defaults to 50%.
        #[arg(long = "max-cpu", value_name = "PERCENT")]
        max_cpu_percent: Option<u8>,

        /// Exclude directories from indexing (can be specified multiple times).
        #[arg(long = "exclude", action = clap::ArgAction::Append)]
        exclude: Vec<String>,

        /// Number of accumulated index mutations that triggers a background
        /// save. Higher values reduce save frequency (and the pauses they
        /// cause during heavy churn) at the cost of more unsaved work if the
        /// process is killed. Defaults to 5000.
        #[arg(long = "auto-save-mutations", value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        auto_save_mutations: Option<u32>,

        /// Maximum number of filesystem events buffered between the OS
        /// watcher and the indexing worker. Raise it if bulk changes (branch
        /// switches, builds) log watcher queue overflows; each overflow costs
        /// a full stale check instead of incremental updates. Defaults to
        /// 16384.
        // Bounded by `usize::MAX` so the value always survives the conversion
        // at the call site. Without it, a value above `usize::MAX` would
        // truncate on a 32-bit target — and truncating to 0 turns the
        // watcher's `sync_channel` into a rendezvous channel, where every
        // `try_send` fails and no event is ever delivered. Plain `//` so the
        // rationale stays out of `--help`.
        #[arg(
            long = "watcher-queue-cap",
            value_name = "N",
            value_parser = clap::value_parser!(u64).range(1..=usize::MAX as u64)
        )]
        watcher_queue_cap: Option<u64>,
    },

    /// Search for a pattern.
    Search {
        /// The regex pattern to search for.
        pattern: String,

        /// Root directories or files to search.
        #[arg(value_name = "PATH", num_args = 0..)]
        paths: Vec<String>,
    },

    /// Show index and server status.
    Status {
        /// Root directory.
        #[arg(default_value = ".")]
        path: PathBuf,
    },

    /// Count text files in a directory (fast walker, no indexing).
    CountFiles {
        /// Root directory to scan.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

impl Cli {
    /// Resolve `--max-filesize` once, so a malformed value is reported instead
    /// of silently falling back to a different limit than the user asked for.
    fn max_filesize_bytes(&self) -> Result<Option<u64>> {
        self.max_filesize
            .as_deref()
            .map(|s| parse_byte_size("--max-filesize", s))
            .transpose()
    }

    /// Resolve `-E/--encoding`. `--no-encoding` means `auto`, and clap's
    /// mutual `overrides_with` makes whichever flag came last win, as ripgrep
    /// does.
    fn encoding_mode(&self) -> Result<tgrep_core::encoding::EncodingMode> {
        match self.encoding.as_deref() {
            Some(label) => tgrep_core::encoding::parse_encoding(label),
            None => Ok(tgrep_core::encoding::EncodingMode::Auto),
        }
    }

    /// Parse every argument that can fail, so a bad value exits with a usage
    /// error instead of silently searching with different settings.
    fn resolve(&self) -> Result<ResolvedArgs> {
        let engine = if self.pcre2 {
            matching::RegexEngine::Pcre2
        } else {
            matching::RegexEngine::from_str_opt(&self.engine)
                .ok_or_else(|| anyhow::anyhow!("unrecognized regex engine: {}", self.engine))?
        };

        let sort = match (&self.sort, &self.sortr, self.sort_files) {
            (Some(key), _, _) => parse_sort(key, false)?,
            (None, Some(key), _) => parse_sort(key, true)?,
            (None, None, true) => Some(search::SortMode {
                key: search::SortKey::Path,
                reverse: false,
            }),
            _ => None,
        };

        Ok(ResolvedArgs {
            max_filesize: self.max_filesize_bytes()?,
            encoding: self.encoding_mode()?,
            engine,
            regex_size_limit: self
                .regex_size_limit
                .as_deref()
                .map(|s| parse_size_limit("--regex-size-limit", s))
                .transpose()?,
            dfa_size_limit: self
                .dfa_size_limit
                .as_deref()
                .map(|s| parse_size_limit("--dfa-size-limit", s))
                .transpose()?,
            sort,
        })
    }

    /// Split the positional arguments into a pattern and a list of paths.
    ///
    /// ripgrep only treats the first positional as the pattern when no pattern
    /// was supplied another way. Once `-e` or `-f` is present every positional
    /// is a path, so `rg -e foo src` searches `src` rather than also searching
    /// for the literal string `src`.
    fn split_pattern_and_paths(
        &self,
        positional: Option<&String>,
        rest: &[String],
    ) -> (String, Vec<String>) {
        if self.regexp.is_empty() && self.pattern_file.is_none() {
            return (positional.cloned().unwrap_or_default(), rest.to_vec());
        }
        let mut paths = Vec::with_capacity(rest.len() + 1);
        paths.extend(positional.cloned());
        paths.extend(rest.iter().cloned());
        (String::new(), paths)
    }

    fn build_search_opts(&self, pattern: String, resolved: &ResolvedArgs) -> search::SearchOptions {
        // `-p/--pretty` is ripgrep's alias for --color always --heading -n.
        let heading = if self.heading || self.pretty {
            Some(true)
        } else if self.no_heading {
            Some(false)
        } else {
            None
        };
        let color = if self.pretty {
            ColorMode::Always
        } else {
            ColorMode::from_str_opt(&self.color).unwrap_or(ColorMode::Auto)
        };
        let no_ignore = self.no_ignore || self.unrestricted >= 1;
        let hidden = self.hidden || self.unrestricted >= 2;
        // ripgrep's third `-u` is `--binary`, not `-a/--text`: binary files
        // become visible and are summarised with a note, rather than having
        // their lines dumped as text.
        let binary = self.binary || self.unrestricted >= 3;
        let text = self.text;

        search::SearchOptions {
            pattern,
            extra_patterns: self.regexp.clone(),
            pattern_file: self.pattern_file.clone(),
            case_insensitive: self.ignore_case,
            case_sensitive: self.case_sensitive,
            smart_case: self.smart_case,
            fixed_string: self.fixed_strings,
            files_only: self.files_only,
            files_without_match: self.files_without_match,
            count: self.count,
            word_boundary: self.word_regexp,
            max_count: self.max_count,
            json: self.json,
            vimgrep: self.vimgrep,
            stats: self.stats || self.debug || self.trace,
            no_index: self.no_index,
            glob: self.glob.clone(),
            iglob: self.iglob.clone(),
            glob_case_insensitive: self.glob_case_insensitive,
            types: self.file_type.clone(),
            types_not: self.type_not.clone(),
            type_add: self.type_add.clone(),
            type_clear: self.type_clear.clone(),
            invert_match: self.invert_match,
            only_matching: self.only_matching,
            after_context: self.after_context,
            before_context: self.before_context,
            context: self.context,
            heading,
            color,
            null: self.null,
            trim: self.trim,
            // --multiline-dotall implies --multiline, as in ripgrep.
            multiline: self.multiline || self.multiline_dotall,
            multiline_dotall: self.multiline_dotall,
            no_ignore,
            hidden,
            quiet: self.quiet,
            no_filename: self.no_filename,
            // ripgrep only turns line numbers on for a terminal, so a piped
            // stream stays `path:content` for whatever parses it. `--column`,
            // `--vimgrep` and `-p` all ask for the line number explicitly.
            no_line_number: self.no_line_number
                || !(self.line_number
                    || self.column
                    || self.vimgrep
                    || self.pretty
                    || crate::output::atty_check()),
            text,
            binary,
            // Replaced per path argument in `run_search`.
            path_display: crate::output::PathDisplay::Bare,
            max_filesize: resolved.max_filesize,
            encoding: resolved.encoding,
            follow: self.follow,
            no_messages: self.no_messages,
            no_ignore_messages: self.no_ignore_messages,
            line_regexp: self.line_regexp,
            no_unicode: self.no_unicode,
            engine: resolved.engine,
            regex_size_limit: resolved.regex_size_limit,
            dfa_size_limit: resolved.dfa_size_limit,
            replace: self.replace.clone(),
            passthru: self.passthru,
            stop_on_nonmatch: self.stop_on_nonmatch,
            column: self.column || self.vimgrep,
            byte_offset: self.byte_offset,
            max_columns: self.max_columns,
            max_columns_preview: self.max_columns_preview,
            count_matches: self.count_matches,
            include_zero: self.include_zero,
            context_separator: if self.no_context_separator {
                None
            } else {
                Some(
                    self.context_separator
                        .clone()
                        .unwrap_or_else(|| "--".to_string()),
                )
            },
            field_match_separator: self
                .field_match_separator
                .clone()
                .unwrap_or_else(|| ":".to_string()),
            field_context_separator: self
                .field_context_separator
                .clone()
                .unwrap_or_else(|| "-".to_string()),
            path_separator: self.path_separator.clone(),
            sort: resolved.sort,
            max_depth: self.max_depth,
            one_file_system: self.one_file_system,
            ignore_files: if self.no_ignore_files {
                Vec::new()
            } else {
                self.ignore_file.clone()
            },
            ignore_file_case_insensitive: self.ignore_file_case_insensitive,
            no_ignore_dot: self.no_ignore_dot,
            no_ignore_exclude: self.no_ignore_exclude,
            no_ignore_global: self.no_ignore_global,
            no_ignore_parent: self.no_ignore_parent,
            no_ignore_vcs: self.no_ignore_vcs,
            no_require_git: self.no_require_git,
            threads: self.threads,
            line_buffered: self.line_buffered,
        }
    }
}

fn main() {
    // clap's derive builds every argument in a single generated stack frame,
    // and tgrep declares enough flags that an unoptimised build overflows the
    // default main-thread stack before parsing finishes. Run the real work on a
    // thread with room to spare so debug builds behave like release ones.
    let worker = std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(run_cli)
        .expect("failed to spawn worker thread");
    if worker.join().is_err() {
        process::exit(2);
    }
}

fn run_cli() {
    let cli = Cli::parse();
    let no_ignore = cli.no_ignore || cli.unrestricted >= 1;

    // Handle --type-list. It reflects --type-add/--type-clear so users can
    // verify a custom definition without running a search.
    if cli.type_list {
        let mut defs = tgrep_core::filetypes::TypeDefs::builtin();
        for name in &cli.type_clear {
            defs.clear(name);
        }
        for spec in &cli.type_add {
            if let Err(e) = defs.add(spec) {
                eprintln!("tgrep: {e}");
                process::exit(2);
            }
        }
        defs.print_list();
        process::exit(0);
    }

    // `-z/--search-zip` would need transparent decompression. Failing loudly is
    // the only safe answer: silently ignoring it makes an archive full of hits
    // look like a repo with none.
    if cli.search_zip {
        eprintln!("tgrep: -z/--search-zip is not supported; decompress the files first");
        process::exit(2);
    }

    // `--pcre2-version` reports the backtracking engine and exits, like ripgrep.
    // tgrep links fancy-regex rather than PCRE2 itself, so say so instead of
    // printing a PCRE2 version number that no PCRE2 is behind.
    if cli.pcre2_version {
        println!("fancy-regex (PCRE2-compatible backtracking engine; PCRE2 is not linked)");
        process::exit(0);
    }

    // Validate every parseable argument up front: a bad value must fail loudly
    // rather than silently searching with different settings than were asked
    // for, which is indistinguishable from "no matches".
    let resolved = match cli.resolve() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("tgrep: {e}");
            process::exit(2);
        }
    };
    let max_filesize = resolved.max_filesize;

    let result = match cli.command {
        Some(Command::Index {
            path,
            exclude,
            strategy,
            index_buffer_mb,
            ..
        }) => index::run(index::RunOptions {
            root: &path,
            index_path: cli.index_path.as_deref(),
            include_hidden: cli.hidden,
            no_ignore,
            exclude_dirs: &exclude,
            strategy: strategy.into(),
            index_buffer_mb,
            // Indexing keeps a size cap by default; searching does not.
            max_file_size: max_filesize.or(Some(tgrep_core::walker::DEFAULT_MAX_FILE_SIZE)),
        }),
        Some(Command::Serve {
            path,
            no_watch,
            max_memory_mb,
            max_cpu_percent,
            exclude,
            auto_save_mutations,
            watcher_queue_cap,
        }) => {
            let memory_cap = max_memory_mb
                .map(|mb| mb.saturating_mul(1024 * 1024))
                .unwrap_or_else(mem::default_memory_cap_bytes);
            let index_threads = cpu::index_thread_count(max_cpu_percent.unwrap_or(50));
            serve::run(
                &path,
                cli.index_path.as_deref(),
                serve::ServeOptions {
                    no_watch,
                    exclude_dirs: &exclude,
                    memory_cap_bytes: memory_cap,
                    index_threads,
                    no_ignore,
                    auto_save_mutations,
                    // Clap's range bound guarantees this fits; saturating keeps
                    // the conversion total, and errs toward a large cap rather
                    // than a zero-length (rendezvous) queue.
                    watcher_queue_cap: watcher_queue_cap
                        .map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
                },
            )
        }
        Some(Command::Search {
            ref pattern,
            ref paths,
        }) => {
            let (pattern, paths) = cli.split_pattern_and_paths(Some(pattern), paths);
            run_search(&cli, pattern, &paths, &resolved)
        }
        Some(Command::Status { path }) => status::run(&path, cli.index_path.as_deref()),
        Some(Command::CountFiles { path }) => walkcount::run(&path, cli.hidden, no_ignore),
        None => {
            if cli.list_files {
                let opts = cli.build_search_opts(String::new(), &resolved);
                let paths = list_files_paths(&cli);
                list_files(&paths, &opts)
            } else if cli.pattern.is_some() || !cli.regexp.is_empty() || cli.pattern_file.is_some()
            {
                let (pattern, paths) =
                    cli.split_pattern_and_paths(cli.pattern.as_ref(), &cli.paths);
                run_search(&cli, pattern, &paths, &resolved)
            } else {
                eprintln!("Usage: tgrep <pattern> [PATH ...]");
                eprintln!("       tgrep index [path]");
                eprintln!("       tgrep serve [path]");
                eprintln!("       tgrep status [path]");
                eprintln!("Search defaults to the current directory when no path is provided.");
                eprintln!("Run `tgrep --help` for full usage.");
                process::exit(2);
            }
        }
    };

    if let Err(e) = result {
        eprintln!("tgrep: {e}");
        process::exit(2);
    }
}

/// A path argument, kept alongside the exact text the user typed.
///
/// ripgrep echoes the argument verbatim into every path it prints, so the
/// original spelling (`.`, `./src`, `src\`, an absolute path) has to survive
/// past the point where the path is canonicalised for the actual walk.
struct SearchTarget {
    path: PathBuf,
    typed: String,
}

impl SearchTarget {
    /// How paths found under this argument should be printed.
    fn display(&self) -> output::PathDisplay {
        if self.typed.is_empty() {
            return output::PathDisplay::Bare;
        }
        if self.path.is_file() {
            return output::PathDisplay::Exact(self.typed.clone());
        }
        // A trailing separator the user typed is kept as typed; otherwise
        // ripgrep joins with the platform separator.
        let mut prefix = self.typed.clone();
        if !prefix.ends_with(['/', '\\']) {
            prefix.push(std::path::MAIN_SEPARATOR);
        }
        output::PathDisplay::Prefix(prefix)
    }
}

fn normalize_search_paths(paths: &[String]) -> Vec<SearchTarget> {
    if paths.is_empty() {
        return vec![SearchTarget {
            path: PathBuf::from("."),
            typed: String::new(),
        }];
    }

    paths
        .iter()
        .map(|path| {
            let mut trimmed = path.trim();
            loop {
                let unquoted = trimmed
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .or_else(|| {
                        trimmed
                            .strip_prefix('\'')
                            .and_then(|s| s.strip_suffix('\''))
                    });
                match unquoted {
                    Some(inner) => trimmed = inner.trim(),
                    None => break,
                }
            }
            SearchTarget {
                path: PathBuf::from(trimmed),
                typed: trimmed.to_string(),
            }
        })
        .collect()
}

fn list_files_paths(cli: &Cli) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(pattern) = cli.pattern.clone() {
        paths.push(pattern);
    }
    paths.extend(cli.paths.iter().cloned());
    paths
}

fn list_files(paths: &[String], opts: &search::SearchOptions) -> anyhow::Result<()> {
    let mut opts = opts.clone();
    for target in normalize_search_paths(paths) {
        if !target.path.exists() {
            report_missing_path(&target.path, opts.no_messages);
            continue;
        }
        opts.path_display = target.display();
        search::list_files(&target.path, &opts)?;
    }
    Ok(())
}

/// ripgrep reports unreadable paths on stderr and finishes the remaining
/// paths, then exits 2. Silently skipping them hides typos and broken globs.
fn report_missing_path(path: &std::path::Path, no_messages: bool) {
    ERRORED.store(true, std::sync::atomic::Ordering::SeqCst);
    if !no_messages {
        eprintln!(
            "tgrep: {}: The system cannot find the path specified. (os error 3)",
            path.display()
        );
    }
}

fn run_search(
    cli: &Cli,
    pattern: String,
    paths: &[String],
    resolved: &ResolvedArgs,
) -> anyhow::Result<()> {
    let mut opts = cli.build_search_opts(pattern, resolved);
    let mut had_matches = false;

    // ripgrep decides these once, from the whole argument list: file names are
    // shown unless exactly one file was named, and line numbers only when
    // writing to a terminal.
    let targets = normalize_search_paths(paths);
    opts.no_filename = cli.no_filename
        || (!cli.with_filename && !cli.vimgrep && targets.len() == 1 && targets[0].path.is_file());

    for target in targets {
        if !target.path.exists() {
            report_missing_path(&target.path, opts.no_messages);
            continue;
        }
        opts.path_display = target.display();
        match search::run(&target.path, cli.index_path.as_deref(), &opts) {
            Ok(true) => {
                had_matches = true;
                if opts.quiet {
                    process::exit(0);
                }
            }
            Ok(false) => {}
            Err(e) => {
                if !opts.no_messages {
                    eprintln!("tgrep: {e}");
                }
                process::exit(2);
            }
        }
    }

    // ripgrep's exit code: 0 on a match with no errors, 2 if anything was
    // reported to stderr, otherwise 1 for "no matches".
    let errored = ERRORED.load(std::sync::atomic::Ordering::SeqCst);
    if had_matches && (opts.quiet || !errored) {
        process::exit(0);
    }
    if errored {
        process::exit(2);
    }
    process::exit(1);
}
