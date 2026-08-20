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

    /// Filter files by type (e.g., rust, py, js). Use --type-list to see all.
    #[arg(short = 't', long = "type", global = true)]
    file_type: Option<String>,

    /// Print all supported file types.
    #[arg(long = "type-list", global = true)]
    type_list: bool,

    /// Ignore files larger than NUM bytes (suffixes K, M, G allowed).
    #[arg(long = "max-filesize", global = true, value_name = "NUM")]
    max_filesize: Option<String>,

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
}

/// Parse a `--max-filesize` value, accepting K/M/G suffixes like ripgrep.
fn parse_max_filesize(s: &str) -> Result<u64> {
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
        .map_err(|_| anyhow::anyhow!("invalid --max-filesize value: {s}"))?;
    n.checked_mul(mult)
        .ok_or_else(|| anyhow::anyhow!("--max-filesize value is too large: {s}"))
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
            .map(parse_max_filesize)
            .transpose()
    }

    fn build_search_opts(
        &self,
        pattern: String,
        max_filesize: Option<u64>,
    ) -> search::SearchOptions {
        let heading = if self.heading {
            Some(true)
        } else if self.no_heading {
            Some(false)
        } else {
            None
        };
        let color = ColorMode::from_str_opt(&self.color).unwrap_or(ColorMode::Auto);
        let no_ignore = self.no_ignore || self.unrestricted >= 1;
        let hidden = self.hidden || self.unrestricted >= 2;
        let text = self.text || self.unrestricted >= 3;

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
            stats: self.stats,
            no_index: self.no_index,
            glob: self.glob.clone(),
            iglob: self.iglob.clone(),
            glob_case_insensitive: self.glob_case_insensitive,
            file_type: self.file_type.clone(),
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
            no_line_number: self.no_line_number,
            text,
            max_filesize,
            follow: self.follow,
            no_messages: self.no_messages,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let no_ignore = cli.no_ignore || cli.unrestricted >= 1;

    // Handle --type-list
    if cli.type_list {
        tgrep_core::filetypes::print_type_list();
        process::exit(0);
    }

    // Validate up front: a bad --max-filesize must fail loudly rather than
    // silently searching with a different limit than the user asked for.
    let max_filesize = match cli.max_filesize_bytes() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("tgrep: {e}");
            process::exit(2);
        }
    };

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
        }) => run_search(&cli, pattern.clone(), paths, max_filesize),
        Some(Command::Status { path }) => status::run(&path, cli.index_path.as_deref()),
        Some(Command::CountFiles { path }) => walkcount::run(&path, cli.hidden, no_ignore),
        None => {
            if cli.list_files {
                let opts = cli.build_search_opts(String::new(), max_filesize);
                let paths = list_files_paths(&cli);
                list_files(&paths, &opts)
            } else if let Some(pattern) = cli.pattern.clone() {
                run_search(&cli, pattern, &cli.paths, max_filesize)
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

fn normalize_search_paths(paths: &[String]) -> Vec<PathBuf> {
    if paths.is_empty() {
        return vec![PathBuf::from(".")];
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
            PathBuf::from(trimmed)
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
    for path in normalize_search_paths(paths) {
        if !path.exists() {
            report_missing_path(&path, opts.no_messages);
            continue;
        }
        search::list_files(&path, opts)?;
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
    max_filesize: Option<u64>,
) -> anyhow::Result<()> {
    let opts = cli.build_search_opts(pattern, max_filesize);
    let mut had_matches = false;

    for path in normalize_search_paths(paths) {
        if !path.exists() {
            report_missing_path(&path, opts.no_messages);
            continue;
        }
        match search::run(&path, cli.index_path.as_deref(), &opts) {
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
