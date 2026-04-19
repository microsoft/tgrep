/// tgrep — trigram-indexed grep with client/server architecture.
///
/// Usage:
///   tgrep index [path]           Build the trigram index
///   tgrep serve [path]           Start the search server
///   tgrep <pattern> [path]       Search (auto-delegates to server)
///   tgrep status [path]          Show index/server status
mod index;
mod output;
mod search;
mod serve;
mod status;
mod walkcount;

use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};
use output::ColorMode;

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

    /// Root directory to search.
    #[arg(global = false, default_value = ".")]
    path: PathBuf,

    // ── Matching ──────────────────────────────────────
    /// Case-insensitive matching.
    #[arg(short = 'i', long = "ignore-case", global = true)]
    ignore_case: bool,

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

    /// Enable multiline matching.
    #[arg(short = 'U', long = "multiline", global = true)]
    multiline: bool,

    // ── Output mode ──────────────────────────────────
    /// Print only filenames with matches.
    #[arg(short = 'l', long = "files-with-matches", global = true)]
    files_only: bool,

    /// Print files that do NOT match the pattern.
    #[arg(short = 'L', long = "files-without-match", global = true)]
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

    /// Filter files by type (e.g., rust, py, js). Use --type-list to see all.
    #[arg(short = 't', long = "type", global = true)]
    file_type: Option<String>,

    /// Print all supported file types.
    #[arg(long = "type-list", global = true)]
    type_list: bool,

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
    #[arg(long = "no-filename", global = true)]
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
    #[arg(long = "hidden", global = true)]
    hidden: bool,

    /// Don't respect .gitignore files.
    #[arg(long = "no-ignore", global = true)]
    no_ignore: bool,

    /// Unrestricted search. -u = no-ignore, -uu = +hidden, -uuu = +binary.
    #[arg(short = 'u', long = "unrestricted", action = clap::ArgAction::Count, global = true)]
    unrestricted: u8,
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
    },

    /// Start the persistent search server.
    Serve {
        /// Root directory to serve.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Disable the file system watcher (saves memory on large repos).
        #[arg(long)]
        no_watch: bool,

        /// Exclude directories from indexing (can be specified multiple times).
        #[arg(long = "exclude", action = clap::ArgAction::Append)]
        exclude: Vec<String>,
    },

    /// Search for a pattern.
    Search {
        /// The regex pattern to search for.
        pattern: String,

        /// Root directory to search.
        #[arg(default_value = ".")]
        path: PathBuf,
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
    fn build_search_opts(&self, pattern: String) -> search::SearchOptions {
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

        search::SearchOptions {
            pattern,
            extra_patterns: self.regexp.clone(),
            pattern_file: self.pattern_file.clone(),
            case_insensitive: self.ignore_case,
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
            multiline: self.multiline,
            no_ignore,
            hidden,
            quiet: self.quiet,
            no_filename: self.no_filename,
            no_line_number: self.no_line_number,
        }
    }
}

fn main() {
    let cli = Cli::parse();

    // Handle --type-list
    if cli.type_list {
        tgrep_core::filetypes::print_type_list();
        process::exit(0);
    }

    let result = match cli.command {
        Some(Command::Index { path, exclude, .. }) => {
            index::run(&path, cli.index_path.as_deref(), cli.hidden, &exclude)
        }
        Some(Command::Serve {
            path,
            no_watch,
            exclude,
        }) => serve::run(&path, cli.index_path.as_deref(), no_watch, &exclude),
        Some(Command::Search {
            ref pattern,
            ref path,
        }) => run_search(&cli, pattern.clone(), path),
        Some(Command::Status { path }) => status::run(&path, cli.index_path.as_deref()),
        Some(Command::CountFiles { path }) => walkcount::run(&path, cli.hidden, cli.no_ignore),
        None => {
            if cli.list_files {
                let opts = cli.build_search_opts(String::new());
                search::list_files(&cli.path, &opts)
            } else if let Some(pattern) = cli.pattern.clone() {
                run_search(&cli, pattern, &cli.path)
            } else {
                eprintln!("Usage: tgrep <pattern> [path]");
                eprintln!("       tgrep index [path]");
                eprintln!("       tgrep serve [path]");
                eprintln!("       tgrep status [path]");
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

fn run_search(cli: &Cli, pattern: String, path: &std::path::Path) -> anyhow::Result<()> {
    let opts = cli.build_search_opts(pattern);
    match search::run(path, cli.index_path.as_deref(), &opts) {
        Ok(true) => process::exit(0),
        Ok(false) => process::exit(1),
        Err(e) => {
            eprintln!("tgrep: {e}");
            process::exit(2);
        }
    }
}
