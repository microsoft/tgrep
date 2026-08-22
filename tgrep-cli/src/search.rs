/// `tgrep search` — search using the trigram index, with server delegation.
///
/// If a running server is detected (via serve.json), the search is delegated
/// over TCP. Otherwise, the on-disk index is loaded directly.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use tgrep_core::builder;
use tgrep_core::filetypes;
use tgrep_core::meta::IndexMeta;
use tgrep_core::query::{self, QueryPlan};
use tgrep_core::reader::IndexReader;
use tgrep_core::walker;

use crate::matching::SearchMatcher;
use crate::output::{ColorMode, ContextLine, Match, OutputConfig, OutputFormat, OutputWriter};
use crate::serve::ServerInfo;

#[derive(Clone)]
#[cfg_attr(test, derive(Default))]
pub struct SearchOptions {
    pub pattern: String,
    pub extra_patterns: Vec<String>,
    pub pattern_file: Option<String>,
    pub case_insensitive: bool,
    pub case_sensitive: bool,
    pub smart_case: bool,
    pub fixed_string: bool,
    pub files_only: bool,
    pub files_without_match: bool,
    pub count: bool,
    pub word_boundary: bool,
    pub max_count: Option<usize>,
    pub json: bool,
    pub vimgrep: bool,
    pub stats: bool,
    pub no_index: bool,
    pub glob: Vec<String>,
    pub iglob: Vec<String>,
    pub glob_case_insensitive: bool,
    pub types: Vec<String>,
    pub types_not: Vec<String>,
    pub type_add: Vec<String>,
    pub type_clear: Vec<String>,
    pub invert_match: bool,
    pub only_matching: bool,
    pub after_context: Option<usize>,
    pub before_context: Option<usize>,
    pub context: Option<usize>,
    pub heading: Option<bool>,
    pub color: ColorMode,
    pub null: bool,
    pub trim: bool,
    pub multiline: bool,
    pub multiline_dotall: bool,
    pub no_ignore: bool,
    pub hidden: bool,
    pub quiet: bool,
    pub no_filename: bool,
    pub no_line_number: bool,
    pub text: bool,
    /// `--binary`: walk into files an extension check would reject, and report
    /// a "binary file matches" note for them instead of printing their lines.
    pub binary: bool,
    /// How to rebuild printed paths from the search-root-relative ones. Set per
    /// path argument, since each argument is echoed back exactly as typed.
    pub path_display: crate::output::PathDisplay,
    pub max_filesize: Option<u64>,
    pub encoding: tgrep_core::encoding::EncodingMode,
    pub follow: bool,
    pub no_messages: bool,
    /// `--no-ignore-messages`: suppress errors from unparseable ignore files.
    /// Independent of `--no-messages`, which suppresses them as well.
    pub no_ignore_messages: bool,
    // ── Matching ──
    pub line_regexp: bool,
    pub no_unicode: bool,
    pub engine: crate::matching::RegexEngine,
    pub regex_size_limit: Option<usize>,
    pub dfa_size_limit: Option<usize>,
    pub replace: Option<String>,
    pub passthru: bool,
    pub stop_on_nonmatch: bool,
    // ── Output detail ──
    pub column: bool,
    pub byte_offset: bool,
    pub max_columns: Option<usize>,
    pub max_columns_preview: bool,
    pub count_matches: bool,
    pub include_zero: bool,
    pub context_separator: Option<String>,
    pub field_match_separator: String,
    pub field_context_separator: String,
    pub path_separator: Option<String>,
    pub sort: Option<SortMode>,
    // ── File discovery ──
    pub max_depth: Option<usize>,
    pub one_file_system: bool,
    pub ignore_files: Vec<String>,
    pub ignore_file_case_insensitive: bool,
    pub no_ignore_dot: bool,
    pub no_ignore_exclude: bool,
    pub no_ignore_global: bool,
    pub no_ignore_parent: bool,
    pub no_ignore_vcs: bool,
    pub no_require_git: bool,
    pub threads: Option<usize>,
    pub line_buffered: bool,
}

/// What `--sort`/`--sortr` orders results by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortMode {
    pub key: SortKey,
    pub reverse: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Path,
    Modified,
    Accessed,
    Created,
}

impl SortKey {
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "path" => Some(Self::Path),
            "modified" => Some(Self::Modified),
            "accessed" => Some(Self::Accessed),
            "created" => Some(Self::Created),
            _ => None,
        }
    }
}

impl SortMode {
    /// Order paths for `--sort`/`--sortr`.
    ///
    /// Metadata reads can fail (races, permissions); those paths sort first
    /// ascending so a transient error can't drop a file from the results.
    pub fn apply(&self, paths: &mut [PathBuf]) {
        match self.key {
            SortKey::Path => paths.sort(),
            _ => {
                let key = self.key;
                // Cached: `sort_by_key` re-reads the key on every comparison,
                // which would be O(n log n) metadata syscalls over a large repo.
                paths.sort_by_cached_key(|p| (time_key(p, key), p.clone()));
            }
        }
        if self.reverse {
            paths.reverse();
        }
    }
}

/// Read the timestamp `--sort` selected, if the platform records it.
fn time_key(path: &Path, key: SortKey) -> Option<std::time::SystemTime> {
    let md = std::fs::metadata(path).ok()?;
    match key {
        SortKey::Modified => md.modified().ok(),
        SortKey::Accessed => md.accessed().ok(),
        SortKey::Created => md.created().ok(),
        SortKey::Path => None,
    }
}

impl SearchOptions {
    /// Resolve smart-case: case-insensitive if pattern is all lowercase.
    pub fn effective_case_insensitive(&self) -> bool {
        // `-s` wins over `-S`, matching ripgrep, but `-i` still wins over `-s`.
        if self.case_insensitive {
            return true;
        }
        if self.case_sensitive {
            return false;
        }
        if self.smart_case {
            return !self.pattern.chars().any(|c| c.is_uppercase());
        }
        false
    }

    /// Whether `.` should match a newline. ripgrep keeps this separate from
    /// `-U`, so `-U` alone does not turn `.` into a line-crossing wildcard.
    pub fn dotall(&self) -> bool {
        self.multiline_dotall
    }

    /// Walk options shared by every non-indexed traversal.
    ///
    /// Unlike index building, searching has no size cap by default: ripgrep
    /// searches files of any size, so a cap here silently hides results.
    pub fn walk_options(&self) -> walker::WalkOptions {
        walker::WalkOptions {
            include_hidden: self.hidden,
            no_ignore: self.no_ignore,
            search_binary: self.text || self.binary,
            follow_links: self.follow,
            max_file_size: self.max_filesize,
            max_depth: self.max_depth,
            same_file_system: self.one_file_system,
            ignore_files: self.ignore_files.iter().map(Into::into).collect(),
            ignore_files_case_insensitive: self.ignore_file_case_insensitive,
            no_ignore_dot: self.no_ignore_dot,
            no_ignore_exclude: self.no_ignore_exclude,
            no_ignore_global: self.no_ignore_global,
            no_ignore_parent: self.no_ignore_parent,
            no_ignore_vcs: self.no_ignore_vcs,
            no_require_git: self.no_require_git,
            // ripgrep gates ignore-file errors on both flags: `--no-messages`
            // suppresses them too, "regardless of" `--no-ignore-messages`.
            no_ignore_messages: self.no_messages || self.no_ignore_messages,
            threads: self.threads,
            ..Default::default()
        }
    }

    fn glob_filter(&self) -> Result<crate::glob_filter::GlobFilter> {
        crate::glob_filter::GlobFilter::new(&self.glob, &self.iglob, self.glob_case_insensitive)
    }

    /// The matching-relevant subset, shared with the server path.
    /// Whether the reply has to carry per-match spans and columns.
    ///
    /// Every consumer of that data is listed here; anything else prints the
    /// line as the server rendered it, so the arrays would be built, sent,
    /// parsed and dropped. The check is conservative — when in doubt the data
    /// is requested, because missing spans degrade output silently.
    fn wants_match_detail(&self) -> bool {
        self.color.is_enabled()      // highlighting brackets each span
            || self.json             // reports them as `submatches`
            || self.vimgrep          // one row per column
            || self.column           // leading column field
            || self.count_matches    // counts matches, not matching lines
            || self.only_matching    // prints the spans themselves
            || self.replace.is_some() // rewrites the matched text
            || self.stats // reports a match total, not a line total
    }

    /// Whether the reply has to carry per-row `offset` and `term`.
    ///
    /// Only these two flags read them, and both default off.
    fn wants_position_detail(&self) -> bool {
        self.byte_offset || self.max_columns.is_some()
    }

    /// `--only-matching` as the search should actually apply it.
    ///
    /// ripgrep implements `-o` in its standard printer only: the JSON printer
    /// always reports whole lines with one `submatch` per hit, so `--json -o`
    /// and `--json` produce byte-identical streams. Fanning a line out into one
    /// event per match here would both reshape the stream and inflate
    /// `matched_lines`, which counts distinct lines.
    fn effective_only_matching(&self) -> bool {
        self.only_matching && !self.json
    }

    fn match_options(&self) -> crate::matching::MatchOptions {
        crate::matching::MatchOptions {
            invert_match: self.invert_match,
            multiline: self.multiline,
            only_matching: self.effective_only_matching(),
            before_context: self.before_ctx(),
            after_context: self.after_ctx(),
            // `-q` and `--files-without-match` only need to know whether the
            // file matched at all, so stop at the first hit.
            max_count: if self.quiet || self.files_without_match {
                Some(1)
            } else {
                self.max_count
            },
            // `--passthru` prints the whole file, so it cannot also stop early
            // at a match limit, and it is meaningless when only file names or
            // counts are printed.
            passthru: self.passthru
                && !self.files_only
                && !self.files_without_match
                && !self.count
                && !self.count_matches
                && !self.quiet,
            replace: self.replace.clone(),
            stop_on_nonmatch: self.stop_on_nonmatch,
            vimgrep: self.vimgrep,
            all_spans: self.wants_match_detail(),
        }
    }

    fn matcher(&self, ci: bool) -> Result<SearchMatcher> {
        crate::matching::build_search_matcher(&self.all_patterns()?, &self.matcher_config(ci))
    }

    /// Pattern-compilation settings, shared with the server path.
    pub fn matcher_config(&self, ci: bool) -> crate::matching::MatcherConfig {
        crate::matching::MatcherConfig {
            case_insensitive: ci,
            fixed_string: self.fixed_string,
            word_boundary: self.word_boundary,
            line_regexp: self.line_regexp,
            multiline: self.multiline,
            dotall: self.dotall(),
            no_unicode: self.no_unicode,
            engine: self.engine,
            regex_size_limit: self.regex_size_limit,
            dfa_size_limit: self.dfa_size_limit,
        }
    }

    /// Compile `--type-add`/`--type-clear` into the effective definitions, then
    /// build the `-t`/`-T` matcher. Shared with the server via the same fields
    /// so both sides derive an identical filter.
    pub fn type_filter(&self) -> Result<filetypes::TypeFilter> {
        build_type_filter(
            &self.type_add,
            &self.type_clear,
            &self.types,
            &self.types_not,
        )
    }

    /// Effective after-context lines.
    pub fn after_ctx(&self) -> usize {
        self.after_context.or(self.context).unwrap_or(0)
    }
    /// Effective before-context lines.
    pub fn before_ctx(&self) -> usize {
        self.before_context.or(self.context).unwrap_or(0)
    }

    fn make_output_config(&self) -> OutputConfig {
        let format = if self.json {
            OutputFormat::Json
        } else if self.files_only {
            OutputFormat::FilesOnly
        } else if self.count || self.count_matches {
            OutputFormat::Count
        } else if self.vimgrep {
            OutputFormat::Vimgrep
        } else {
            // Heading vs flat is resolved in the writer.
            OutputFormat::Heading
        };
        OutputConfig {
            format,
            color: self.color,
            heading: self.heading,
            null: self.null,
            trim: self.trim,
            no_filename: self.no_filename,
            no_line_number: self.no_line_number,
            context: self.after_ctx() > 0 || self.before_ctx() > 0,
            column: self.column,
            byte_offset: self.byte_offset,
            max_columns: self.max_columns,
            max_columns_preview: self.max_columns_preview,
            context_separator: self.context_separator.clone(),
            field_match_separator: self.field_match_separator.clone(),
            field_context_separator: self.field_context_separator.clone(),
            path_separator: self.path_separator.clone(),
            path_display: self.path_display.clone(),
            line_buffered: self.line_buffered,
        }
    }

    /// Collect all patterns (primary + -e extras + -f file patterns).
    fn all_patterns(&self) -> Result<Vec<String>> {
        let has_extra = !self.extra_patterns.is_empty() || self.pattern_file.is_some();
        // `-e`/`-f` move the positional argument into the path list, leaving the
        // base pattern empty. Keeping it would contribute an empty alternative
        // that matches every line.
        let mut patterns = if has_extra && self.pattern.is_empty() {
            Vec::new()
        } else {
            vec![self.pattern.clone()]
        };
        patterns.extend(self.extra_patterns.iter().cloned());
        if let Some(ref path) = self.pattern_file {
            let content = std::fs::read_to_string(path)?;
            for line in content.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    patterns.push(line.to_string());
                }
            }
        }
        Ok(patterns)
    }
}

/// Build the [`OutputWriter`] that spans a whole invocation.
///
/// Kept here so `make_output_config` stays private to this module.
pub fn new_writer(opts: &SearchOptions) -> OutputWriter {
    OutputWriter::new(opts.make_output_config())
}

/// List files that would be searched (--files mode).
pub fn list_files(root: &Path, opts: &SearchOptions) -> Result<()> {
    let root = match std::fs::canonicalize(root) {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let glob_filter = opts.glob_filter()?;
    let type_filter = opts.type_filter()?;

    // `--files` lists what would be searched without reading anything, so it
    // must not drop files on an extension guess the way a search does.
    let walk_opts = walker::WalkOptions {
        search_binary: true,
        ..opts.walk_options()
    };

    if root.is_file() {
        let rel_path = explicit_file_display_path(&root);

        if passes_filters(&rel_path, &glob_filter, &type_filter) {
            let mut writer = OutputWriter::new(opts.make_output_config());
            writer.write_file(&rel_path)?;
            writer.flush()?;
        }
        return Ok(());
    }

    let walk = walker::walk_dir(&root, &walk_opts);
    let mut writer = OutputWriter::new(opts.make_output_config());

    for path in &walk.files {
        let rel_path = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        if !passes_filters(&rel_path, &glob_filter, &type_filter) {
            continue;
        }
        writer.write_file(&rel_path)?;
    }
    writer.flush()?;
    Ok(())
}

/// Run one search path's worth of work, writing through the caller's `writer`.
///
/// The writer is owned by the caller and shared across every path argument:
/// ripgrep emits one JSON `summary` for the whole invocation, so stats have to
/// accumulate across paths and the summary is emitted by
/// [`OutputWriter::finish`] once the last path is done.
pub fn run(
    root: &Path,
    index_path: Option<&Path>,
    opts: &SearchOptions,
    writer: &mut OutputWriter,
) -> Result<bool> {
    let root = match std::fs::canonicalize(root) {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let index_dir = index_path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| builder::default_index_dir(&root));

    let ci = opts.effective_case_insensitive();

    // A non-default `--encoding` re-decodes files into text the index never
    // saw. Worse, a file the indexer classified as binary (BOM-less UTF-16 is
    // nothing but NUL-interleaved bytes) is absent from the index entirely, so
    // even a full-scan plan cannot reach it. The only way `-E` means the same
    // thing with and without an index is to walk the tree. `-a`/`--binary` are
    // the same story, as is anything that widens the walk: the index was built
    // skipping hidden and ignored files, so `--hidden`/`--no-ignore` can only
    // find them by walking.
    //
    // A single named file is bypassed too. The index deliberately omits binary
    // and ignored files, but naming one explicitly is exactly how ripgrep asks
    // for it — and reading one file directly is cheaper than loading an index.
    let bypass_index = root.is_file()
        || opts.encoding.may_differ_from_index()
        || opts.text
        || opts.binary
        || opts.hidden
        || opts.no_ignore
        || opts.no_ignore_dot
        || opts.no_ignore_exclude
        || opts.no_ignore_global
        || opts.no_ignore_parent
        || opts.no_ignore_vcs;

    // Try to delegate to a running server (skip for files_without_match
    // since the server only returns matching files). `--include-zero` needs
    // the same bypass: a zero-count row can only come from a file the server
    // never reports.
    if !opts.no_index
        && !bypass_index
        && !opts.files_without_match
        && !opts.include_zero
        && let Ok(info) = ServerInfo::load(&index_dir)
        && let Some((index_root, scope)) = resolve_scope(&index_dir, &root)
    {
        if let Ok(had_matches) =
            search_via_server(&info, &root, &index_root, &scope, opts, ci, writer)
        {
            return Ok(had_matches);
        }
        eprintln!("Server unreachable, falling back to local index");
    }

    // No server — use on-disk index directly (or brute force)
    if opts.no_index || bypass_index {
        return brute_force_search(&root, opts, ci, writer);
    }
    if !index_dir.join("lookup.bin").exists() {
        warn_missing_index(&index_dir, index_path.is_some(), opts.quiet);
        return brute_force_search(&root, opts, ci, writer);
    }

    search_local_index(&root, &index_dir, opts, ci, writer)
}

/// Render a path the way the user typed it.
///
/// `std::fs::canonicalize` returns Windows extended-length paths (`\\?\C:\...`,
/// or `\\?\UNC\server\share` for network paths). That prefix is an internal
/// Win32 detail and only confuses people when it shows up in a diagnostic.
fn display_path(p: &Path) -> String {
    strip_verbatim_prefix(&p.display().to_string())
}

/// Drop the Windows extended-length (`\\?\`) prefix from an already-rendered
/// path.
///
/// Kept separate from [`display_path`] because match output also needs it, and
/// there the string has already been through `strip_prefix`/`to_string_lossy`.
fn strip_verbatim_prefix(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

/// Warn when a search silently degrades to scanning every file.
///
/// This is the difference between a sub-second query and a multi-minute one on
/// a large tree, so it must never be silent. The most common cause is starting
/// `serve` with `--index-path` but omitting it on the search: the flag is
/// global, meaning it is accepted on every subcommand, not remembered between
/// invocations. The client then looks for `serve.json` in the default location,
/// finds nothing, and falls through to here without ever contacting the server.
fn warn_missing_index(index_dir: &Path, explicit_path: bool, quiet: bool) {
    if quiet {
        return;
    }
    let dir = display_path(index_dir);
    eprintln!("warning: no index at {dir} - scanning every file (slow on large trees)");
    if explicit_path {
        eprintln!("note: build one with `tgrep index --index-path {dir}`");
    } else {
        eprintln!(
            "note: if a server is running with `--index-path <dir>`, pass the same \
             --index-path here; otherwise build an index with `tgrep index`"
        );
    }
}

fn search_via_server(
    info: &ServerInfo,
    root: &Path,
    index_root: &Path,
    scope: &IndexScope,
    opts: &SearchOptions,
    ci: bool,
    writer: &mut OutputWriter,
) -> Result<bool> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", info.port))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(300)))?;

    // Resolve `-f/--file` here rather than sending the path: the server has a
    // different working directory and no `pattern_file` param, so forwarding
    // only `pattern`/`extra_patterns` would silently drop the file's patterns
    // and make results depend on whether a daemon happens to be running.
    let patterns = opts.all_patterns()?;
    let (primary, extras) = patterns
        .split_first()
        .map(|(p, rest)| (p.clone(), rest.to_vec()))
        .unwrap_or_else(|| (opts.pattern.clone(), Vec::new()));

    // Context lines are pure presentation; requesting them while counting would
    // stream rows the count path has to filter back out.
    let wants_context = !(opts.count || opts.count_matches || opts.files_only || opts.quiet);

    // Spans and columns are the bulk of a reply — a nested array for every
    // match — and most searches never read them. Asking for them only when
    // something will consume them keeps a 21k-hit query from building, sending
    // and parsing 21k arrays that are then dropped.
    let detail = opts.wants_match_detail();
    let positions = opts.wants_position_detail();

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "search",
        "params": {
            "pattern": primary,
            "extra_patterns": extras,
            "case_insensitive": ci,
            "fixed_string": opts.fixed_string,
            "files_only": opts.files_only,
            "word_boundary": opts.word_boundary,
            "max_count": opts.match_options().max_count,
            "glob": opts.glob,  // sent as JSON array
            "iglob": opts.iglob,
            "glob_case_insensitive": opts.glob_case_insensitive,
            "types": opts.types,
            "types_not": opts.types_not,
            "type_add": opts.type_add,
            "type_clear": opts.type_clear,
            "invert_match": opts.invert_match,
            "only_matching": opts.effective_only_matching(),
            "after_context": if wants_context { opts.after_ctx() } else { 0 },
            "before_context": if wants_context { opts.before_ctx() } else { 0 },
            "multiline": opts.multiline,
            "multiline_dotall": opts.dotall(),
            "text": opts.text,
            // JSON output reports binary matches the same way ripgrep does —
            // the matching lines plus a `binary_offset` — so ask the server to
            // send the lines alongside the marker. An older server ignores this
            // and returns the marker alone.
            "binary_lines": opts.json,
            "max_filesize": opts.max_filesize,
            "encoding": opts.encoding.label(),
            "line_regexp": opts.line_regexp,
            "no_unicode": opts.no_unicode,
            "engine": opts.engine.label(),
            "regex_size_limit": opts.regex_size_limit,
            "dfa_size_limit": opts.dfa_size_limit,
            "replace": opts.replace,
            "passthru": opts.match_options().passthru,
            "stop_on_nonmatch": opts.stop_on_nonmatch,
            "vimgrep": opts.vimgrep,
            "detail": detail,
            "positions": positions,
        },
        "id": 1,
    });
    writeln!(stream, "{}", request)?;
    stream.flush()?;

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let response: serde_json::Value = serde_json::from_str(&line)?;

    if let Some(error) = response.get("error") {
        let msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("server error: {msg}");
    }

    let result = response
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("no result in response"))?;

    let empty_vec = Vec::new();
    let matches = result
        .get("matches")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty_vec);

    // The server always searches its whole indexed tree and reports paths
    // relative to the index root, so a subdirectory argument and `--max-depth`
    // have to be applied to the reply.
    //
    // ripgrep only surfaces a binary file when the user named it explicitly, so
    // `binary` rows for anything reached by traversal are dropped here — along
    // with the match rows a `--json` client asked to be sent with them, which
    // would otherwise leak the file's raw contents.
    let drop_binary = !opts.binary
        && !matches!(scope, IndexScope::File(_))
        && matches
            .iter()
            .any(|m| m.get("type").and_then(|t| t.as_str()) == Some("binary"));
    let dropped_binary_files: std::collections::HashSet<&str> = if drop_binary {
        matches
            .iter()
            .filter(|m| m.get("type").and_then(|t| t.as_str()) == Some("binary"))
            .filter_map(|m| m.get("file").and_then(|f| f.as_str()))
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    let scoped;
    let matches = if matches!(scope, IndexScope::Whole) && opts.max_depth.is_none() && !drop_binary
    {
        matches
    } else {
        scoped = matches
            .iter()
            .filter_map(|m| {
                let file = m.get("file")?.as_str()?;
                if dropped_binary_files.contains(file) {
                    return None;
                }
                let rel = scope.relativize(file, root)?;
                within_max_depth(&rel, opts).then_some(())?;
                let mut m = m.clone();
                m["file"] = serde_json::Value::String(rel);
                Some(m)
            })
            .collect::<Vec<_>>();
        &scoped
    };

    // `--sort` reorders whole files. Ranking distinct files once (one metadata
    // read each) and sorting rows by rank keeps every file's rows in the
    // server's line order. The key is a `PathBuf` so the order matches the
    // brute-force walk, which compares paths component-wise.
    let sorted;
    let matches = match opts.sort {
        None => matches,
        Some(sort) => {
            let mut ranked: Vec<(Option<std::time::SystemTime>, PathBuf, String)> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for m in matches {
                let Some(rel) = m.get("file").and_then(|f| f.as_str()) else {
                    continue;
                };
                if !seen.insert(rel.to_string()) {
                    continue;
                }
                let t = if sort.key == SortKey::Path {
                    None
                } else {
                    time_key(&scope.full_path(index_root, rel), sort.key)
                };
                ranked.push((t, PathBuf::from(rel), rel.to_string()));
            }
            ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            if sort.reverse {
                ranked.reverse();
            }
            let rank: std::collections::HashMap<&str, usize> = ranked
                .iter()
                .enumerate()
                .map(|(i, (_, _, f))| (f.as_str(), i))
                .collect();

            let mut rows = matches.clone();
            rows.sort_by_key(|m| {
                m.get("file")
                    .and_then(|f| f.as_str())
                    .and_then(|f| rank.get(f).copied())
                    .unwrap_or(usize::MAX)
            });
            sorted = rows;
            &sorted
        }
    };

    let had_matches = !matches.is_empty();

    if opts.quiet {
        writer.flush()?;
        return Ok(had_matches);
    }

    if opts.files_only {
        let mut seen = std::collections::HashSet::new();
        for m in matches {
            if let Some(file) = m.get("file").and_then(|f| f.as_str())
                && seen.insert(file.to_string())
            {
                writer.write_file(file)?;
            }
        }
    } else if opts.count || opts.count_matches {
        // `-c` counts matching *lines*, so collapse rows that share a line
        // (`-o` emits one per match) and ignore context rows. This is what the
        // local path reports via `FileMatches::matched_lines`.
        // `--count-matches` instead sums the spans on each row, which is what
        // `FileMatches::match_count` reports locally.
        let mut order: Vec<String> = Vec::new();
        let mut lines: std::collections::HashMap<String, std::collections::HashSet<usize>> =
            std::collections::HashMap::new();
        let mut spans: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        // A binary file is summarised as a single `binary` row rather than one
        // row per match, so it carries its own line count. The local path still
        // prints a real count for it because `-c` returns before the binary
        // guard, and dropping these rows here would silently omit the file.
        let mut exact: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for m in matches {
            let mtype = m.get("type").and_then(|t| t.as_str()).unwrap_or("match");
            if mtype != "match" && mtype != "binary" {
                continue;
            }
            let Some(file) = m.get("file").and_then(|f| f.as_str()) else {
                continue;
            };
            if !lines.contains_key(file) && !exact.contains_key(file) {
                order.push(file.to_string());
            }
            if mtype == "binary" {
                let n = m.get("lines").and_then(|l| l.as_u64()).unwrap_or(0) as usize;
                exact.insert(file.to_string(), n);
            } else {
                let line = m.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as usize;
                lines.entry(file.to_string()).or_default().insert(line);
                // A row with no spans is still one match (`-v` reports inverted
                // lines with an empty span list).
                let n = m
                    .get("spans")
                    .and_then(|s| s.as_array())
                    .map(|s| s.len().max(1))
                    .unwrap_or(1);
                *spans.entry(file.to_string()).or_default() += n;
            }
        }
        // Emit in response order; iterating the maps would randomise the file
        // order on every run, unlike the local path which prints in walk order.
        for file in &order {
            let n = exact
                .get(file)
                .copied()
                .or_else(|| {
                    if opts.count_matches {
                        spans.get(file).copied()
                    } else {
                        lines.get(file).map(|l| l.len())
                    }
                })
                .unwrap_or(0);
            writer.write_count(file, n)?;
        }
    } else {
        for m in matches {
            let file = m.get("file").and_then(|f| f.as_str()).unwrap_or("");
            let line = m.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as usize;
            let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
            let mtype = m.get("type").and_then(|t| t.as_str()).unwrap_or("match");
            let offset = m.get("offset").and_then(|o| o.as_u64()).unwrap_or(0) as usize;
            // `-M/--max-columns` counts the line terminator, which only the
            // server knows; an older server omits it and simply measures the
            // visible text.
            let terminator_len = m.get("term").and_then(|t| t.as_u64()).unwrap_or(0) as usize;
            if mtype == "context" {
                writer.write_context_separator(file, line)?;
                writer.write_context_line(&ContextLine {
                    file: file.to_string(),
                    line_number: line,
                    content: content.to_string(),
                    absolute_offset: offset,
                    terminator_len,
                })?;
            } else if mtype == "binary" {
                writer.write_binary_note(file, offset)?;
            } else {
                let spans = m
                    .get("spans")
                    .and_then(|s| s.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|p| {
                                let p = p.as_array()?;
                                Some((p.first()?.as_u64()? as usize, p.get(1)?.as_u64()? as usize))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                writer.write_context_separator(file, line)?;
                writer.write_match(&Match {
                    file: file.to_string(),
                    line_number: line,
                    content: content.to_string(),
                    columns: m
                        .get("columns")
                        .and_then(|c| c.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|c| c.as_u64().map(|c| c as usize))
                                .collect()
                        })
                        .unwrap_or_default(),
                    spans,
                    absolute_offset: offset,
                    terminator_len,
                })?;
            }
        }
    }

    if opts.stats
        && let Some(elapsed) = result.get("elapsed_ms").and_then(|e| e.as_f64())
    {
        // Count the rows that survived scoping, not the server's `num_matches`.
        // The server searches the whole indexed tree and counts every row it
        // built, so that field includes files outside a subdirectory argument
        // and counts context lines as matches.
        let (num, lines) = count_reported_matches(matches);
        eprintln!("{num} matches ({lines} matched lines) in {elapsed:.1}ms (via server)");
    }

    writer.flush()?;
    Ok(had_matches)
}

fn search_local_index(
    root: &Path,
    index_dir: &Path,
    opts: &SearchOptions,
    ci: bool,
    writer: &mut OutputWriter,
) -> Result<bool> {
    let start = Instant::now();
    let reader = IndexReader::open(index_dir)?;

    // Index paths are relative to the root the index was built for, which is
    // not necessarily the directory being searched. Without translating between
    // the two, `tgrep --index-path IDX foo src` looks for `src/src/lib.rs` and
    // silently reports nothing.
    let Some((index_root, scope)) = resolve_scope(index_dir, root) else {
        // The index covers an unrelated tree, so it cannot answer this search.
        return brute_force_search(root, opts, ci, writer);
    };

    let glob_filter = opts.glob_filter()?;
    let type_filter = opts.type_filter()?;

    let matcher = opts.matcher(ci)?;

    // Narrow candidates using every pattern, not just the primary one. A
    // non-default `--encoding` re-decodes files into text the index never saw,
    // so the trigram plan cannot be trusted to find them.
    let plan = if !matcher.is_standard() || opts.encoding.may_differ_from_index() {
        QueryPlan::MatchAll
    } else {
        query::build_multi_pattern_plan(&opts.all_patterns()?, opts.fixed_string, ci)
            .map_err(|e| anyhow::anyhow!("{e}"))?
    };

    let is_match_all = plan.is_match_all();

    // `-v` inverts at the *line* level: a file matches when some line does not.
    // The trigram plan selects files that contain the pattern, which is exactly
    // the wrong filter, so it has to be bypassed. Same for `--files-without-match`
    // and `--include-zero`, which both have to report files the plan excluded.
    let candidate_ids =
        if is_match_all || opts.files_without_match || opts.invert_match || opts.include_zero {
            reader.all_file_ids()
        } else {
            query::execute_plan_with_masks(&plan, &|tri| reader.lookup_trigram_with_masks(tri))
        };

    // Drop everything outside the searched subtree and re-express the survivors
    // relative to it, so globs, `--max-depth` and printed paths all agree with
    // what the brute-force walk of the same directory would produce.
    let mut candidates: Vec<(u32, String)> = candidate_ids
        .iter()
        .filter_map(|&fid| {
            let indexed = reader.file_path(fid)?;
            let rel = scope.relativize(indexed, root)?;
            within_max_depth(&rel, opts).then_some(())?;
            Some((fid, rel))
        })
        .collect();

    // `--sort` has to apply here too, otherwise it would silently do nothing on
    // the default (indexed) code path. The key is a `PathBuf` so the order
    // matches the brute-force walk, which compares paths component-wise; a raw
    // string compare would sort `src.rs` before `src/lib.rs`.
    if let Some(sort) = opts.sort {
        let mut keyed: Vec<_> = candidates
            .into_iter()
            .map(|(fid, rel)| {
                let t = if sort.key == SortKey::Path {
                    None
                } else {
                    time_key(&scope.full_path(&index_root, &rel), sort.key)
                };
                (t, PathBuf::from(&rel), fid, rel)
            })
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        if sort.reverse {
            keyed.reverse();
        }
        candidates = keyed
            .into_iter()
            .map(|(_, _, fid, rel)| (fid, rel))
            .collect();
    }

    if opts.stats {
        eprintln!(
            "Query plan: {} (candidates: {}/{})",
            plan_summary(&plan),
            candidates.len(),
            reader.num_files()
        );
    }

    let mut had_matches = false;
    // A single-file search root is a file the user named on the command line,
    // which is what makes binary files visible in ripgrep.
    let explicit = matches!(scope, IndexScope::File(_));

    for (_, rel_path) in &candidates {
        if !passes_filters(rel_path, &glob_filter, &type_filter) {
            continue;
        }

        let full_path = scope.full_path(&index_root, rel_path);
        if exceeds_max_filesize(&full_path, opts) {
            continue;
        }
        let (content, fixups) = match read_text_lossy(&full_path, opts.encoding) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let outcome = search_decoded_file(
            &content,
            &fixups,
            &matcher,
            rel_path,
            opts,
            &mut *writer,
            explicit,
        )?;
        match outcome {
            FileOutcome::Matched => {
                if !opts.files_without_match {
                    had_matches = true;
                    if opts.quiet {
                        break;
                    }
                }
            }
            FileOutcome::NoMatch if opts.files_without_match => {
                if !opts.quiet {
                    writer.write_file(rel_path)?;
                }
                had_matches = true;
                if opts.quiet {
                    break;
                }
            }
            FileOutcome::NoMatch | FileOutcome::Skipped => {}
        }
    }

    if opts.stats {
        let elapsed = start.elapsed();
        eprintln!(
            "Search completed in {:.1}ms",
            elapsed.as_secs_f64() * 1000.0
        );
    }

    writer.flush()?;
    Ok(had_matches)
}

/// Which slice of an index a search root covers.
///
/// An index stores paths relative to the root it was built for. A search root
/// may be that same directory, a directory inside it, or a single file inside
/// it; each needs a different translation to and from index-relative paths.
enum IndexScope {
    Whole,
    Subtree(String),
    File(String),
}

impl IndexScope {
    /// `None` when `search_root` lies outside the indexed tree, which means the
    /// index cannot answer the search at all.
    fn resolve(index_root: &Path, search_root: &Path) -> Option<Self> {
        let rel = search_root.strip_prefix(index_root).ok()?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() {
            Some(IndexScope::Whole)
        } else if search_root.is_file() {
            Some(IndexScope::File(rel))
        } else {
            Some(IndexScope::Subtree(format!("{rel}/")))
        }
    }

    /// Re-express an index-relative path relative to the search root, or `None`
    /// when the file is outside it.
    fn relativize(&self, indexed: &str, search_root: &Path) -> Option<String> {
        match self {
            IndexScope::Whole => Some(indexed.to_string()),
            IndexScope::Subtree(prefix) => indexed.strip_prefix(prefix).map(str::to_string),
            // A named file prints as the user reached it, exactly as the
            // brute-force path does.
            IndexScope::File(f) => (f == indexed).then(|| explicit_file_display_path(search_root)),
        }
    }

    fn full_path(&self, index_root: &Path, rel: &str) -> PathBuf {
        let indexed = match self {
            IndexScope::Whole => rel.to_string(),
            IndexScope::Subtree(prefix) => format!("{prefix}{rel}"),
            // `rel` is the path as the user typed it, so go back to the one the
            // index stores.
            IndexScope::File(f) => f.clone(),
        };
        index_root.join(indexed.replace('/', std::path::MAIN_SEPARATOR_STR))
    }
}

/// The slice of the index at `index_dir` that covers `root`, with the absolute
/// root the index was built for.
fn resolve_scope(index_dir: &Path, root: &Path) -> Option<(PathBuf, IndexScope)> {
    let index_root = IndexMeta::load(index_dir)
        .ok()
        .and_then(|m| std::fs::canonicalize(m.root_path).ok())
        .unwrap_or_else(|| root.to_path_buf());
    let scope = IndexScope::resolve(&index_root, root)?;
    Some((index_root, scope))
}

/// `true` when the path is shallow enough to keep under `--max-depth`.
///
/// The walker applies this while descending; the indexed paths never went
/// through it, so it has to be re-applied by counting components.
fn within_max_depth(rel: &str, opts: &SearchOptions) -> bool {
    match opts.max_depth {
        // `rel` is search-root-relative, so its separator count is the number
        // of directory levels below the root; a depth of 1 means "directly in
        // the root", matching ripgrep's `--max-depth`.
        Some(max) => rel.matches('/').count() < max,
        None => true,
    }
}

fn brute_force_search(
    root: &Path,
    opts: &SearchOptions,
    ci: bool,
    writer: &mut OutputWriter,
) -> Result<bool> {
    let start = Instant::now();
    let glob_filter = opts.glob_filter()?;
    let type_filter = opts.type_filter()?;

    let matcher = opts.matcher(ci)?;

    let mut had_matches = false;

    if root.is_file() {
        let rel_path = explicit_file_display_path(root);
        if passes_filters(&rel_path, &glob_filter, &type_filter)
            && !exceeds_max_filesize(root, opts)
        {
            let (content, fixups) = read_text_lossy(root, opts.encoding)?;
            let outcome = search_decoded_file(
                &content,
                &fixups,
                &matcher,
                &rel_path,
                opts,
                &mut *writer,
                true,
            )?;
            match outcome {
                FileOutcome::Matched => {
                    if !opts.files_without_match {
                        had_matches = true;
                    }
                }
                FileOutcome::NoMatch if opts.files_without_match => {
                    if !opts.quiet {
                        writer.write_file(&rel_path)?;
                    }
                    had_matches = true;
                }
                FileOutcome::NoMatch | FileOutcome::Skipped => {}
            }
        }

        if opts.stats {
            let elapsed = start.elapsed();
            eprintln!(
                "Brute-force search completed in {:.1}ms (1 files)",
                elapsed.as_secs_f64() * 1000.0,
            );
        }

        writer.flush()?;
        return Ok(had_matches);
    }

    let mut walk = walker::walk_dir(root, &opts.walk_options());
    if let Some(sort) = opts.sort {
        sort.apply(&mut walk.files);
    }

    for path in &walk.files {
        let rel_path = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        if !passes_filters(&rel_path, &glob_filter, &type_filter) {
            continue;
        }

        let (content, fixups) = match read_text_lossy(path, opts.encoding) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let outcome = search_decoded_file(
            &content,
            &fixups,
            &matcher,
            &rel_path,
            opts,
            &mut *writer,
            false,
        )?;
        match outcome {
            FileOutcome::Matched => {
                if !opts.files_without_match {
                    had_matches = true;
                    if opts.quiet {
                        break;
                    }
                }
            }
            FileOutcome::NoMatch if opts.files_without_match => {
                if !opts.quiet {
                    writer.write_file(&rel_path)?;
                }
                had_matches = true;
                if opts.quiet {
                    break;
                }
            }
            FileOutcome::NoMatch | FileOutcome::Skipped => {}
        }
    }

    if opts.stats {
        let elapsed = start.elapsed();
        eprintln!(
            "Brute-force search completed in {:.1}ms ({} files)",
            elapsed.as_secs_f64() * 1000.0,
            walk.files.len()
        );
    }

    writer.flush()?;
    Ok(had_matches)
}

/// Read a file as text, applying `--encoding` and replacing invalid UTF-8
/// rather than failing.
///
/// ripgrep searches files that are not valid UTF-8; refusing them makes
/// UTF-16, Latin-1 and mixed-encoding sources silently invisible.
fn read_text_lossy(
    path: &Path,
    encoding: tgrep_core::encoding::EncodingMode,
) -> std::io::Result<(String, tgrep_core::encoding::LossyFixups)> {
    let bytes = std::fs::read(path)?;
    Ok(tgrep_core::encoding::decode_owned_with_fixups(
        bytes, encoding,
    ))
}

/// Whether `--max-filesize` excludes this file.
///
/// The brute-force walk applies the limit while walking, but the indexed path
/// takes candidates straight from the index and the explicit-file path skips
/// the walk entirely. Both check here so the flag means the same thing on every
/// path instead of silently doing nothing on the default (indexed) one.
fn exceeds_max_filesize(path: &Path, opts: &SearchOptions) -> bool {
    let Some(limit) = opts.max_filesize else {
        return false;
    };
    std::fs::metadata(path).is_ok_and(|md| md.len() > limit)
}

/// Map line-relative columns from decoded text to the source bytes.
///
/// Columns count bytes from the start of the line, so both ends have to be
/// mapped before subtracting.
pub(crate) fn to_source_columns(
    columns: &[usize],
    line_offset: usize,
    fixups: &tgrep_core::encoding::LossyFixups,
) -> Vec<usize> {
    if fixups.is_empty() {
        return columns.to_vec();
    }
    let line = fixups.to_source_offset(line_offset);
    columns
        .iter()
        .map(|&c| fixups.to_source_offset(line_offset + c - 1) - line + 1)
        .collect()
}

/// Map one emitted match's columns and absolute offset onto the bytes on disk.
///
/// Shared by the brute-force/index path and the server so the two can never
/// disagree about coordinates. Columns arrive indexing the decoded source line
/// and are mapped back through `fixups`; `--replace` then moves each of them by
/// its entry in `column_shifts`, because ripgrep reports positions in the
/// rewritten line rather than in the original one.
///
/// The shifts are measured on the decoded text, so they are exact unless a
/// *replaced* match itself covered bytes that failed to decode — a case with no
/// ripgrep equivalent, since ripgrep searches raw bytes and never matches the
/// `U+FFFD` that repairing them produces. Columns are clamped to stay 1-based.
pub(crate) fn to_source_positions(
    columns: &[usize],
    column_shifts: &[isize],
    absolute_offset: usize,
    offset_shift: isize,
    line_offset: usize,
    fixups: &tgrep_core::encoding::LossyFixups,
) -> (Vec<usize>, usize) {
    let mut columns = to_source_columns(columns, line_offset, fixups);
    for (col, &shift) in columns.iter_mut().zip(column_shifts) {
        *col = col.saturating_add_signed(shift).max(1);
    }
    let offset = fixups
        .to_source_offset(absolute_offset)
        .saturating_add_signed(offset_shift);
    (columns, offset)
}

/// What happened to one file during a search.
///
/// `Skipped` exists because ripgrep hides binary files found by traversal
/// entirely: they must not surface as "no match" either, or they would leak
/// into `--files-without-match`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FileOutcome {
    Matched,
    NoMatch,
    Skipped,
}

/// Search one file's decoded text, mapping reported offsets back to the bytes
/// on disk using `fixups`.
///
/// `explicit` is true when the user named this file directly on the command
/// line. ripgrep only surfaces binary files that were named explicitly (or
/// when `--binary` is given); binary files reached by directory traversal are
/// skipped silently, so they appear in neither `-l`, `-c`, `-L`, nor the plain
/// output.
fn search_decoded_file(
    content: &str,
    fixups: &tgrep_core::encoding::LossyFixups,
    matcher: &SearchMatcher,
    rel_path: &str,
    opts: &SearchOptions,
    writer: &mut OutputWriter,
    explicit: bool,
) -> Result<FileOutcome> {
    // ripgrep only surfaces a binary file when the user named it explicitly (or
    // passed `--binary`).
    //
    // The NUL is located in the repaired text but reported in terms of the file
    // on disk, so it goes back through `fixups`: repairing invalid UTF-8 ahead
    // of the NUL widens every bad byte to three, which would otherwise push the
    // reported offset past where the byte actually is.
    let binary_offset = if opts.text {
        None
    } else {
        content
            .as_bytes()
            .iter()
            .position(|&b| b == 0)
            .map(|off| fixups.to_source_offset(off))
    };
    if binary_offset.is_some() && !explicit && !opts.binary {
        return Ok(FileOutcome::Skipped);
    }

    // ripgrep's searcher quits at the NUL byte, so it reports only the bytes it
    // got through rather than the file's full length. Both are counted on disk,
    // so the whole-file case needs the same mapping the offset just got.
    writer.note_bytes_searched(
        binary_offset.unwrap_or_else(|| fixups.to_source_offset(content.len())) as u64,
    );

    let match_opts = opts.match_options();
    let found = crate::matching::FileMatches::find(content, matcher, &match_opts)?;

    if found.is_empty() {
        // `--include-zero` still reports files that were searched but had no
        // match, which is the only way to tell them apart from files that were
        // never looked at.
        if opts.include_zero && (opts.count || opts.count_matches) && !opts.quiet {
            writer.write_count(rel_path, 0)?;
        }
        return Ok(FileOutcome::NoMatch);
    }

    // For quiet/files_without_match, we only need the outcome.
    if opts.quiet || opts.files_without_match {
        return Ok(FileOutcome::Matched);
    }

    if opts.files_only {
        writer.write_file(rel_path)?;
        return Ok(FileOutcome::Matched);
    }

    if opts.count || opts.count_matches {
        let n = if opts.count_matches {
            found.match_count()
        } else {
            found.matched_lines()
        };
        writer.write_count(rel_path, n)?;
        return Ok(FileOutcome::Matched);
    }

    // Never dump raw binary to a terminal; ripgrep reports a note instead.
    // JSON has no such note — the matches are emitted normally and the offset
    // rides along on the `end` message — so only stop early for text output.
    if let Some(off) = binary_offset {
        writer.write_binary_note(rel_path, off)?;
        if !writer.is_json() {
            return Ok(FileOutcome::Matched);
        }
    }

    found.for_each(&match_opts, matcher, |emit| -> Result<()> {
        match emit {
            crate::matching::Emit::Match {
                line_number,
                content,
                columns,
                spans,
                absolute_offset,
                line_offset,
                column_shifts,
                offset_shift,
                terminator_len,
            } => {
                writer.write_context_separator(rel_path, line_number)?;
                let (columns, absolute_offset) = to_source_positions(
                    &columns,
                    &column_shifts,
                    absolute_offset,
                    offset_shift,
                    line_offset,
                    fixups,
                );
                writer.write_match(&Match {
                    file: rel_path.to_string(),
                    line_number,
                    content: content.into_owned(),
                    columns,
                    spans,
                    absolute_offset,
                    terminator_len,
                })?;
            }
            crate::matching::Emit::Context {
                line_number,
                content,
                absolute_offset,
                terminator_len,
            } => {
                writer.write_context_separator(rel_path, line_number)?;
                writer.write_context_line(&ContextLine {
                    file: rel_path.to_string(),
                    line_number,
                    content: content.to_string(),
                    absolute_offset: fixups.to_source_offset(absolute_offset),
                    terminator_len,
                })?;
            }
        }
        Ok(())
    })?;

    Ok(FileOutcome::Matched)
}

/// Path to print for a file the user named directly on the command line.
///
/// Relative to the cwd when the file lives under it, otherwise absolute. Either
/// way the `\\?\` prefix is stripped first: `--vimgrep` and `--json` consumers
/// feed these straight back to an editor, and no editor opens `//?/C:/...`.
fn explicit_file_display_path(path: &Path) -> String {
    let display_path = std::env::current_dir()
        .ok()
        .and_then(|cwd| std::fs::canonicalize(cwd).ok())
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| path.to_path_buf());

    strip_verbatim_prefix(&display_path.to_string_lossy()).replace('\\', "/")
}

fn passes_filters(
    rel_path: &str,
    glob_filter: &crate::glob_filter::GlobFilter,
    type_filter: &filetypes::TypeFilter,
) -> bool {
    type_filter.matches(rel_path) && glob_filter.matches(rel_path)
}

/// Apply `--type-clear` then `--type-add` to the built-in definitions and
/// compile the `-t`/`-T` selections. Lives here rather than on `SearchOptions`
/// so the server can reuse it verbatim from its own request fields.
pub fn build_type_filter(
    type_add: &[String],
    type_clear: &[String],
    types: &[String],
    types_not: &[String],
) -> Result<filetypes::TypeFilter> {
    if type_add.is_empty() && type_clear.is_empty() && types.is_empty() && types_not.is_empty() {
        return Ok(filetypes::TypeFilter::default());
    }
    let mut defs = filetypes::TypeDefs::builtin();
    // Clears run first so `--type-clear foo --type-add foo:*.x` redefines a
    // type from scratch, which is the documented ripgrep idiom.
    for name in type_clear {
        defs.clear(name);
    }
    for spec in type_add {
        defs.add(spec)?;
    }
    defs.build_filter(types, types_not)
}

/// Match and matched-line totals for `--stats`, counted from the rows the
/// client is actually going to print.
///
/// ripgrep reports these two numbers separately and neither of them counts
/// context lines, so `-C 2` must not change either total. `-o` makes the server
/// emit one row per match, so matches are summed from each row's spans while
/// matched lines are de-duplicated by file and line.
///
/// An inverted row (`-v`) carries an empty span list: ripgrep reports those as
/// matched lines but as zero matches, because no part of the line matched.
/// `--stats` implies `wants_match_detail`, so the spans are always present here
/// and an absent list means the row genuinely had none.
fn count_reported_matches(rows: &[serde_json::Value]) -> (u64, u64) {
    let mut matches = 0u64;
    let mut lines: std::collections::HashSet<(&str, u64)> = std::collections::HashSet::new();
    let mut binary_lines = 0u64;

    for row in rows {
        let file = row.get("file").and_then(|f| f.as_str()).unwrap_or("");
        match row.get("type").and_then(|t| t.as_str()).unwrap_or("match") {
            // A binary file is summarised as one row carrying its own count
            // rather than one row per match, which is what `-c` reports for it.
            "binary" => {
                let n = row.get("lines").and_then(|l| l.as_u64()).unwrap_or(0);
                matches += n;
                binary_lines += n;
            }
            "match" => {
                matches += row
                    .get("spans")
                    .and_then(|s| s.as_array())
                    .map_or(0, |s| s.len() as u64);
                lines.insert((file, row.get("line").and_then(|l| l.as_u64()).unwrap_or(0)));
            }
            // "context" and anything else is not a match.
            _ => {}
        }
    }

    (matches, lines.len() as u64 + binary_lines)
}

fn plan_summary(plan: &QueryPlan) -> String {
    match plan {
        QueryPlan::And(queries) => format!("AND({} trigrams)", queries.len()),
        QueryPlan::Or(plans) => {
            let subs: Vec<String> = plans.iter().map(plan_summary).collect();
            format!("OR({})", subs.join(", "))
        }
        QueryPlan::MatchAll => "MatchAll (full scan)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- `--stats` counting -------------------------------------------------
    //
    // These pin the reply-row semantics that `--stats` reports over the server.
    // The server counts every row it built across the whole indexed tree, so
    // reporting its `num_matches` made `--stats` disagree with both the printed
    // output and ripgrep. Verified against ripgrep 15.2.0.

    fn row(kind: &str, file: &str, line: u64, spans: usize) -> serde_json::Value {
        serde_json::json!({
            "type": kind,
            "file": file,
            "line": line,
            "spans": (0..spans).map(|i| serde_json::json!([i, i + 1])).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn stats_counts_one_match_per_span() {
        // `rg --stats` on a line with 3 hits: "3 matches / 1 matched lines".
        let rows = vec![row("match", "a.rs", 1, 3)];
        assert_eq!(count_reported_matches(&rows), (3, 1));
    }

    #[test]
    fn stats_ignores_context_rows() {
        // `-C 2` must not change either total; counting raw rows made a
        // 90-match search report 445.
        let rows = vec![
            row("context", "a.rs", 1, 0),
            row("match", "a.rs", 2, 1),
            row("context", "a.rs", 3, 0),
        ];
        assert_eq!(count_reported_matches(&rows), (1, 1));
    }

    #[test]
    fn stats_deduplicates_lines_across_rows() {
        // `-o` emits one row per match, so the same line arrives twice; ripgrep
        // still reports it as a single matched line.
        let rows = vec![row("match", "a.rs", 7, 1), row("match", "a.rs", 7, 1)];
        assert_eq!(count_reported_matches(&rows), (2, 1));
    }

    #[test]
    fn stats_keeps_same_line_number_in_different_files_apart() {
        let rows = vec![row("match", "a.rs", 4, 1), row("match", "b.rs", 4, 1)];
        assert_eq!(count_reported_matches(&rows), (2, 2));
    }

    #[test]
    fn stats_reports_no_matches_for_inverted_rows() {
        // `rg -v --stats` reports "0 matches" with a non-zero matched-line
        // count, because no part of an inverted line matched.
        let rows = vec![row("match", "a.rs", 1, 0), row("match", "a.rs", 2, 0)];
        assert_eq!(count_reported_matches(&rows), (0, 2));
    }

    #[test]
    fn stats_uses_the_line_count_a_binary_row_carries() {
        // A binary file is summarised as one row rather than one row per match.
        let rows = vec![serde_json::json!({
            "type": "binary", "file": "a.bin", "lines": 5,
        })];
        assert_eq!(count_reported_matches(&rows), (5, 5));
    }

    #[test]
    fn stats_on_no_rows_is_zero() {
        assert_eq!(count_reported_matches(&[]), (0, 0));
    }

    #[test]
    fn stats_treats_a_row_without_spans_as_no_matches() {
        // `--stats` implies `wants_match_detail`, so a missing span list means
        // the row genuinely had none rather than that detail was suppressed.
        let rows = vec![serde_json::json!({
            "type": "match", "file": "a.rs", "line": 1,
        })];
        assert_eq!(count_reported_matches(&rows), (0, 1));
    }

    #[test]
    fn stats_requests_match_detail_so_spans_are_available_to_count() {
        let mut opts = SearchOptions {
            stats: true,
            ..Default::default()
        };
        assert!(
            opts.wants_match_detail(),
            "--stats must request spans, or the match total silently reads 0"
        );
        opts.stats = false;
        assert!(!opts.wants_match_detail());
    }

    #[test]
    fn display_path_strips_extended_length_prefix() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\src\repo\.tgrep")),
            r"C:\src\repo\.tgrep"
        );
    }

    #[test]
    fn display_path_restores_unc_paths() {
        // Naively stripping `\\?\` would leave a bogus `UNC\server\...` path.
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share\idx")),
            r"\\server\share\idx"
        );
    }

    #[test]
    fn display_path_leaves_ordinary_paths_alone() {
        assert_eq!(display_path(Path::new(r"C:\src\repo")), r"C:\src\repo");
        assert_eq!(
            display_path(Path::new("/home/user/repo")),
            "/home/user/repo"
        );
    }

    #[test]
    fn explicit_file_path_outside_cwd_is_editor_openable() {
        // A file outside the cwd keeps its absolute path, but must not leak
        // `\\?\` — rendered with forward slashes that becomes `//?/C:/...`,
        // which no editor or `xargs` consumer can open.
        let rendered = explicit_file_display_path(Path::new(r"\\?\D:\other\tree\main.rs"));
        assert_eq!(rendered, "D:/other/tree/main.rs");
        assert!(!rendered.contains("//?/"), "got: {rendered}");
    }

    #[test]
    fn explicit_file_path_keeps_unc_shares_usable() {
        assert_eq!(
            explicit_file_display_path(Path::new(r"\\?\UNC\server\share\main.rs")),
            "//server/share/main.rs"
        );
    }
}
