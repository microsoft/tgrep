/// `tgrep search` — search using the trigram index, with server delegation.
///
/// If a running server is detected (via serve.json), the search is delegated
/// over TCP. Otherwise, the on-disk index is loaded directly.
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use tgrep_core::builder;
use tgrep_core::filetypes;
use tgrep_core::meta::IndexMeta;
use tgrep_core::query::{self, QueryPlan};
use tgrep_core::reader::IndexReader;
use tgrep_core::walker;

use crate::matching::SearchMatcher;
use crate::output::{ColorMode, ContextLine, Match, OutputConfig, OutputFormat, OutputWriter};
use crate::serve::ServerInfo;

/// One `-e/--regexp` or `-f/--file` flag, in command-line order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternSource {
    Regexp(String),
    File(String),
}

#[derive(Clone)]
#[cfg_attr(test, derive(Default))]
pub struct SearchOptions {
    pub pattern: String,
    /// `-e`/`-f` flags in command-line order. When non-empty, they replace
    /// `pattern`.
    pub pattern_sources: Vec<PatternSource>,
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
    pub shared: bool,
    pub glob: Vec<String>,
    pub iglob: Vec<String>,
    pub glob_case_insensitive: bool,
    /// `-t`/`-T`/`--type-add`/`--type-clear` in command-line order.
    pub type_ops: Vec<filetypes::TypeOp>,
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
    /// `--content-eligibility`: whether the filename-extension heuristic may
    /// exclude content. Selects only matching shared views; scans walk the
    /// same eligible set.
    pub content_eligibility: tgrep_core::generations::ContentEligibility,
    /// How to rebuild printed paths from the search-root-relative ones. Set per
    /// path argument, since each argument is echoed back exactly as typed.
    pub path_display: crate::output::PathDisplay,
    pub max_filesize: Option<u64>,
    /// Whether `--max-filesize` was passed, as opposed to inheriting
    /// [`tgrep_core::walker::DEFAULT_MAX_FILE_SIZE`]. See
    /// [`exceeds_max_filesize`].
    pub max_filesize_requested: bool,
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

fn flush_before_stats(writer: &mut OutputWriter) -> Result<()> {
    match writer.flush() {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
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

    /// Sort index-backed entries by search-relative paths, looking up metadata
    /// at their actual locations. `PathBuf` keeps ordering component-wise, as
    /// in the filesystem walk, rather than comparing raw path strings.
    fn apply_indexed<T>(
        &self,
        entries: &mut [T],
        index_root: &Path,
        scope: &IndexScope,
        relative_path: impl Fn(&T) -> &str,
    ) -> Result<()> {
        let rooted = (self.key != SortKey::Path)
            .then(|| tgrep_core::rooted::RootedDir::open(index_root))
            .transpose()?;
        entries.sort_by_cached_key(|entry| {
            let rel = relative_path(entry);
            let time = rooted.as_ref().and_then(|rooted| {
                match rooted
                    .open_file(&scope.indexed_path(rel))
                    .and_then(|file| file.metadata())
                {
                    Ok(metadata) => metadata_time_key(&metadata, self.key),
                    Err(error) => {
                        eprintln!("warning: cannot stat indexed file {rel:?}: {error}");
                        None
                    }
                }
            });
            (time, PathBuf::from(rel))
        });
        if self.reverse {
            entries.reverse();
        }
        Ok(())
    }
}

/// Read the timestamp `--sort` selected, if the platform records it.
fn time_key(path: &Path, key: SortKey) -> Option<std::time::SystemTime> {
    let md = std::fs::metadata(path).ok()?;
    metadata_time_key(&md, key)
}

fn metadata_time_key(md: &std::fs::Metadata, key: SortKey) -> Option<std::time::SystemTime> {
    match key {
        SortKey::Modified => md.modified().ok(),
        SortKey::Accessed => md.accessed().ok(),
        SortKey::Created => md.created().ok(),
        SortKey::Path => None,
    }
}

impl SearchOptions {
    /// Resolve smart-case from literals in every supplied pattern.
    pub fn effective_case_insensitive(&self) -> Result<bool> {
        // `-s` wins over `-S`, matching ripgrep, but `-i` still wins over `-s`.
        if self.case_insensitive {
            return Ok(true);
        }
        if self.case_sensitive {
            return Ok(false);
        }
        if self.smart_case {
            return crate::matching::smart_case_insensitive(
                &self.all_patterns()?,
                self.fixed_string,
            );
        }
        Ok(false)
    }

    /// Whether `.` should match a newline. ripgrep keeps this separate from
    /// `-U`, so `-U` alone does not turn `.` into a line-crossing wildcard.
    pub fn dotall(&self) -> bool {
        self.multiline_dotall
    }

    /// Whether content eligibility ignores the filename-extension heuristic.
    pub fn content_only(&self) -> bool {
        self.content_eligibility == tgrep_core::generations::ContentEligibility::ContentOnly
    }

    /// Walk options shared by every non-indexed traversal.
    ///
    /// Unlike index building, searching has no size cap by default: ripgrep
    /// searches files of any size, so a cap here silently hides results.
    pub fn walk_options(&self) -> walker::WalkOptions {
        walker::WalkOptions {
            include_hidden: self.hidden,
            no_ignore: self.no_ignore,
            search_binary: self.text || self.binary || self.content_only(),
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

    /// Whether local matching must retain every span on each line.
    ///
    /// Server requests also use this as the default row-detail policy, but
    /// count-only replies consume per-file totals instead.
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
    /// JSON always reports absolute offsets, even without `--byte-offset`.
    fn wants_position_detail(&self) -> bool {
        self.json || self.byte_offset || self.max_columns.is_some()
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

    /// Whether matching lines get printed as text, rather than reduced to
    /// counts, file names, an exit code or JSON events.
    fn prints_text_lines(&self) -> bool {
        !(self.json
            || self.count
            || self.count_matches
            || self.files_only
            || self.files_without_match
            || self.quiet)
    }

    /// Whether `-U --replace` may rewrite whole blocks of lines, as ripgrep's
    /// multiline printer does when the pattern can match a line terminator.
    /// `-m0` prints no lines, so it has no block to rewrite.
    fn replace_blocks(&self) -> bool {
        self.multiline
            && self.replace.is_some()
            && !self.invert_match
            && self.prints_text_lines()
            && self.max_count != Some(0)
    }

    /// Whether the output of a `-U --replace` search depends on blocks.
    /// `FileMatches::find` only builds them when the pattern can match a line
    /// terminator; any other pattern prints line by line, just as a server
    /// that predates blocks does. A pattern that does not compile counts.
    fn needs_replace_blocks(&self, ci: bool) -> bool {
        self.replace_blocks()
            && self
                .matcher(ci)
                .map_or(true, |matcher| matcher.can_match_newline())
    }

    /// `--json --replace`: report each submatch's replacement beside it.
    /// Inverted searches print no submatches, not even on context lines, `-q`
    /// prints only the summary and `-m0` prints no lines, so none of them has
    /// a replacement to report.
    fn report_replacements(&self) -> bool {
        self.json
            && self.replace.is_some()
            && !self.invert_match
            && !self.quiet
            && self.max_count != Some(0)
    }

    /// Whether the result depends on ripgrep's filter order: type flags in
    /// command-line order, and a positive glob overriding `-t`/`-T`. Otherwise
    /// a server's legacy order, which applies every `-T` after every `-t` and
    /// requires a file to pass both the globs and the types, gives the same
    /// result. Without a `-t`/`-T`, the type filter admits every file however
    /// the definitions are ordered.
    fn needs_ordered_filters(&self) -> Result<bool> {
        Ok(self.selects_types()
            && (!filetypes::TypeOp::is_unordered_equivalent(&self.type_ops)
                || self.glob_filter()?.has_includes()))
    }

    /// Whether any `-t`/`-T` was given, the only flags that consume
    /// `--type-add`/`--type-clear` definitions.
    fn selects_types(&self) -> bool {
        self.type_ops.iter().any(|op| {
            matches!(
                op,
                filetypes::TypeOp::Select(_) | filetypes::TypeOp::Negate(_)
            )
        })
    }

    fn effective_passthru(&self) -> bool {
        // ripgrep treats `-m 0` as suppressing all search output, including
        // passthru context.
        self.passthru
            && self.max_count != Some(0)
            && !self.files_only
            && !self.files_without_match
            && !self.count
            && !self.count_matches
            && !self.quiet
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
            passthru: self.effective_passthru(),
            replace: self.replace.clone(),
            stop_on_nonmatch: self.stop_on_nonmatch,
            vimgrep: self.vimgrep,
            all_spans: self.wants_match_detail(),
            replace_blocks: self.replace_blocks(),
            report_replacements: self.report_replacements(),
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

    /// Compile the type flags into the `-t`/`-T` matcher. The server rebuilds
    /// it from the same ops, so both sides derive an identical filter.
    pub fn type_filter(&self) -> Result<filetypes::TypeFilter> {
        filetypes::build_type_filter(&self.type_ops)
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
            eager_file_separator: self.ripgrep_would_search_sequentially(),
            field_match_separator: self.field_match_separator.clone(),
            field_context_separator: self.field_context_separator.clone(),
            path_separator: self.path_separator.clone(),
            path_display: self.path_display.clone(),
            line_buffered: self.line_buffered,
        }
    }

    /// Whether ripgrep would search one file at a time, which changes how it
    /// separates files (see [`OutputConfig::eager_file_separator`]).
    ///
    /// ripgrep sorts on a single thread, and otherwise uses `-j` threads or,
    /// for `-j0` or no `-j`, one per available CPU.
    fn ripgrep_would_search_sequentially(&self) -> bool {
        self.sort.is_some()
            || match self.threads {
                Some(0) | None => std::thread::available_parallelism().map_or(1, |n| n.get()) == 1,
                Some(n) => n == 1,
            }
    }

    /// Snapshot pattern files once, before case analysis, planning or server dispatch.
    /// Returns whether any patterns were supplied.
    pub fn resolve_patterns(&mut self) -> Result<bool> {
        let patterns = self.all_patterns()?;
        self.pattern.clear();
        self.pattern_sources = patterns.into_iter().map(PatternSource::Regexp).collect();
        if self.pattern_sources.is_empty() {
            // Inversion still needs to search: an empty pattern set rejects every
            // line, unlike an empty pattern, which accepts every line.
            self.pattern = crate::matching::NEVER_MATCH_PATTERN.to_string();
            self.fixed_string = false;
            return Ok(false);
        }
        Ok(true)
    }

    /// Collect every pattern: the `-e`/`-f` flags in command-line order, or the
    /// positional pattern when neither was given. `-e`/`-f` move the positional
    /// argument into the path list, so it never contributes a pattern alongside
    /// them.
    fn all_patterns(&self) -> Result<Vec<String>> {
        if self.pattern_sources.is_empty() {
            return Ok(vec![self.pattern.clone()]);
        }
        let mut patterns = Vec::new();
        for source in &self.pattern_sources {
            match source {
                PatternSource::Regexp(pattern) => patterns.push(pattern.clone()),
                PatternSource::File(path) => {
                    let content = std::fs::read_to_string(path)?;
                    patterns.extend(content.lines().map(str::to_string));
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

fn server_backend_label(result: &serde_json::Value) -> &'static str {
    match result.get("backend").and_then(|value| value.as_str()) {
        Some("shared-v1") => "shared daemon v1",
        Some("shared-v2") => "shared daemon v2",
        _ => "server",
    }
}

/// List files that would be searched (`--files` mode).
pub fn list_files(root: &Path, index_path: Option<&Path>, opts: &SearchOptions) -> Result<()> {
    let start = Instant::now();
    let root = match std::fs::canonicalize(root) {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let glob_filter = opts.glob_filter()?;
    let type_filter = opts.type_filter()?;

    if root.is_file() {
        let rel_path = explicit_file_display_path(&root);

        if passes_filters(&rel_path, &glob_filter, &type_filter, true) {
            let mut writer = OutputWriter::new(opts.make_output_config());
            writer.write_file(&rel_path)?;
            writer.flush()?;
        }
        return Ok(());
    }

    let index_dir = index_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| builder::default_index_dir(&root));

    let shared = crate::serve::shared::Client::selected(&root, opts.shared, index_path);
    let shared_selected = !matches!(shared, Ok(false));
    if shared_selected && !opts.no_index {
        let result = (|| -> Result<_> {
            shared?;
            anyhow::ensure!(
                index_path.is_none() && shared_compatible(opts),
                "options require a filesystem scan"
            );
            let (client, view) = crate::serve::shared::Client::registered(&root)?;
            let scope = IndexScope::resolve(&view.root, &root)
                .ok_or_else(|| anyhow::anyhow!("search root outside shared worktree"))?;
            let result = client.view_request(
                "files", &view,
                serde_json::json!({"hidden":opts.hidden,"scope":scope.prefix(),"max_depth":opts.max_depth}),
            )?;
            let paths: Vec<String> = serde_json::from_value(
                result
                    .get("files")
                    .ok_or_else(|| anyhow::anyhow!("shared response omitted files"))?
                    .clone(),
            )?;
            Ok((view.root, scope, paths, server_backend_label(&result)))
        })();
        match result {
            Ok((index_root, scope, paths, backend)) => {
                write_indexed_file_paths(
                    &index_root,
                    &root,
                    &scope,
                    paths,
                    None,
                    &glob_filter,
                    &type_filter,
                    opts,
                )?;
                if opts.stats {
                    eprintln!("Filename search completed (via {backend})");
                }
                return Ok(());
            }
            Err(error) => warn_shared_fallback(&error, opts),
        }
    }

    if !shared_selected && filename_index_compatible(opts) {
        if let Ok(info) = ServerInfo::load(&index_dir)
            && let Some((index_root, scope)) = resolve_scope(&index_dir, &root)
            && let Ok(paths) = list_files_via_server(&info, &scope, opts)
        {
            write_indexed_file_paths(
                &index_root,
                &root,
                &scope,
                paths,
                None,
                &glob_filter,
                &type_filter,
                opts,
            )?;
            if opts.stats {
                eprintln!(
                    "Filename search completed in {:.1}ms (via server)",
                    start.elapsed().as_secs_f64() * 1000.0
                );
            }
            return Ok(());
        }

        match load_indexed_file_paths(&index_dir) {
            Ok(Some((paths, visibility))) => {
                if let Some((index_root, scope)) = resolve_scope(&index_dir, &root) {
                    write_indexed_file_paths(
                        &index_root,
                        &root,
                        &scope,
                        paths,
                        Some(&visibility),
                        &glob_filter,
                        &type_filter,
                        opts,
                    )?;
                    if opts.stats {
                        eprintln!(
                            "Filename search completed in {:.1}ms (via local index)",
                            start.elapsed().as_secs_f64() * 1000.0
                        );
                    }
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(error) => {
                if !opts.no_messages {
                    eprintln!("warning: filename index unavailable ({error}) - walking filesystem");
                }
            }
        }
    }

    // `--files` lists what would be searched without reading anything, so it
    // must not drop files on an extension guess the way a search does.
    let walk_opts = walker::WalkOptions {
        search_binary: true,
        exclude_paths: index_storage_exclusions(&index_dir)?,
        overrides: Some(glob_filter.walk_overrides(&root)?),
        ..opts.walk_options()
    };
    let walk = walker::walk_dir(&root, &walk_opts);
    let paths = walk.files.iter().map(|path| {
        path.strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    });
    write_indexed_file_paths(
        &root,
        &root,
        &IndexScope::Whole,
        paths,
        None,
        &glob_filter,
        &type_filter,
        opts,
    )?;
    if opts.stats {
        eprintln!(
            "Filename search completed in {:.1}ms (via filesystem walk)",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(())
}

/// Whether an existing index was built under traversal rules that can answer
/// this invocation without silently omitting files.
fn filename_index_compatible(opts: &SearchOptions) -> bool {
    !opts.no_index
        && !opts.no_ignore
        && !opts.no_ignore_dot
        && !opts.no_ignore_exclude
        && !opts.no_ignore_global
        && !opts.no_ignore_parent
        && !opts.no_ignore_vcs
        && !opts.no_require_git
        && !opts.follow
        && !opts.one_file_system
        && opts.ignore_files.is_empty()
        && !opts.ignore_file_case_insensitive
        && !opts.max_filesize_requested
}

fn shared_compatible(opts: &SearchOptions) -> bool {
    !opts.no_ignore
        && !opts.no_ignore_dot
        && !opts.no_ignore_exclude
        && !opts.no_ignore_global
        && !opts.no_ignore_parent
        && !opts.no_ignore_vcs
        && !opts.no_require_git
        && !opts.follow
        && !opts.one_file_system
        && opts.ignore_files.is_empty()
        && !opts.ignore_file_case_insensitive
        && !opts.encoding.may_differ_from_index()
        && !opts.text
        && !opts.binary
        && opts.max_filesize == Some(64 * 1024 * 1024)
}

fn warn_shared_fallback(error: &anyhow::Error, opts: &SearchOptions) {
    if !opts.no_messages {
        eprintln!(
            "warning: shared view unavailable ({error:#}) - scanning filesystem (not a legacy index)"
        );
    }
}

/// Load a complete local filename index, including the paths deliberately
/// omitted from the content index.
fn load_indexed_file_paths(
    index_dir: &Path,
) -> Result<Option<(Vec<String>, tgrep_core::visibility::PathVisibility)>> {
    if !index_dir.join("lookup.bin").exists() {
        return Ok(None);
    }
    let Some(filename_index) = tgrep_core::path_index::read_filename_index(index_dir)? else {
        return Ok(None);
    };
    let Some(visibility) = filename_index.visibility else {
        return Ok(None);
    };
    let mut extra_paths = filename_index.paths;
    let meta = IndexMeta::load(index_dir)?;
    if !meta.complete
        || !meta.hidden_complete
        || meta.version != tgrep_core::meta::INDEX_FORMAT_VERSION
        || !visibility.hidden_complete
    {
        return Ok(None);
    }

    let reader = IndexReader::open(index_dir)?;
    if !visibility.covers_index(&meta, reader.file_table_id()) {
        return Ok(None);
    }
    let mut paths = Vec::with_capacity(reader.num_files() + extra_paths.len());
    paths.extend(reader.all_paths().iter().cloned());
    paths.append(&mut extra_paths);
    Ok(Some((paths, visibility.paths)))
}

/// Apply search-root scoping and CLI filters to index-root-relative paths.
#[allow(clippy::too_many_arguments)]
fn write_indexed_file_paths(
    index_root: &Path,
    search_root: &Path,
    scope: &IndexScope,
    paths: impl IntoIterator<Item = String>,
    visibility: Option<&tgrep_core::visibility::PathVisibility>,
    glob_filter: &crate::glob_filter::GlobFilter,
    type_filter: &filetypes::TypeFilter,
    opts: &SearchOptions,
) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    let mut paths: Vec<String> = paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .filter(|path| {
            visibility
                .is_none_or(|visibility| visibility.is_visible(path, scope.prefix(), opts.hidden))
        })
        .filter_map(|path| scope.relativize(&path, search_root))
        .filter(|path| within_max_depth(path, opts))
        .filter(|path| passes_filters(path, glob_filter, type_filter, true))
        .collect();

    if let Some(sort) = opts.sort {
        sort.apply_indexed(&mut paths, index_root, scope, String::as_str)?;
    }

    let mut writer = OutputWriter::new(opts.make_output_config());
    for path in paths {
        writer.write_file(&path)?;
    }
    writer.flush()?;
    Ok(())
}

fn list_files_via_server(
    info: &ServerInfo,
    scope: &IndexScope,
    opts: &SearchOptions,
) -> Result<Vec<String>> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", info.port))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(300)))?;
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "files",
            "params": { "hidden": opts.hidden, "scope": scope.prefix() },
            "id": 1,
        })
    )?;
    stream.flush()?;

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let response: serde_json::Value = serde_json::from_str(&line)?;
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("server error: {message}");
    }

    let result = response
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("server response did not contain a result"))?;
    require_server_coverage(result)?;
    result
        .get("files")
        .and_then(|files| files.as_array())
        .ok_or_else(|| anyhow::anyhow!("server response did not contain files"))?
        .iter()
        .map(|path| {
            path.as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("server returned a non-string file path"))
        })
        .collect()
}

fn require_server_coverage(result: &serde_json::Value) -> Result<()> {
    if result
        .get("hidden_complete")
        .and_then(|value| value.as_bool())
        != Some(true)
    {
        anyhow::bail!("server does not support complete hidden-file coverage");
    }
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
    writer.reset_match_totals();
    let root = match std::fs::canonicalize(root) {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let index_dir = index_path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| builder::default_index_dir(&root));

    let ci = opts.effective_case_insensitive()?;
    // ripgrep rejects an invalid `--type-add` before searching, even one that
    // no `-t`/`-T` uses. A server is not sent definitions that no selection
    // consumes, so it cannot be left to reject them.
    opts.type_filter()?;

    let selected = crate::serve::shared::Client::selected(&root, opts.shared, index_path);
    if !matches!(selected, Ok(false)) {
        if !opts.no_index {
            let response = (|| -> Result<_> {
                selected?;
                anyhow::ensure!(
                    index_path.is_none()
                        && !root.is_file()
                        && shared_compatible(opts)
                        && !opts.effective_passthru()
                        && !opts.files_without_match
                        && !opts.include_zero,
                    "options require a filesystem scan"
                );
                let (client, view) = crate::serve::shared::Client::registered(&root)?;
                anyhow::ensure!(
                    view.generation.profile().eligibility == opts.content_eligibility,
                    "view content eligibility differs from the query"
                );
                let scope = IndexScope::resolve(&view.root, &root)
                    .ok_or_else(|| anyhow::anyhow!("search root outside shared worktree"))?;
                let request = server_search_request(&scope, opts, ci)?;
                let result = client.view_request("search", &view, request["params"].clone())?;
                require_server_coverage(&result)?;
                require_ordered_filters(&result, &request)?;
                require_replace_support(&result, &request)?;
                Ok((view.root, scope, request, result))
            })();
            match response {
                Ok((index_root, scope, request, result)) => {
                    return render_server_result(
                        &result,
                        &request,
                        &root,
                        &index_root,
                        &scope,
                        opts,
                        writer,
                    );
                }
                Err(error) => warn_shared_fallback(&error, opts),
            }
        }
        return brute_force_search(&root, &index_dir, opts, ci, writer);
    }

    // A non-default `--encoding` re-decodes files into text the index never
    // saw. Worse, a file the indexer classified as binary (BOM-less UTF-16 is
    // nothing but NUL-interleaved bytes) is absent from the index entirely, so
    // even a full-scan plan cannot reach it. The only way `-E` means the same
    // thing with and without an index is to walk the tree. `-a`/`--binary` are
    // the same story, as is explicitly disabling ignore rules.
    // Globs and --hidden only filter the indexed corpus. Use --no-index for
    // positive glob overrides that reinclude ignored files.
    //
    // A single named file is bypassed too. The index deliberately omits binary
    // and ignored files, but naming one explicitly is exactly how ripgrep asks
    // for it — and reading one file directly is cheaper than loading an index.
    let bypass_index = root.is_file()
        || opts.encoding.may_differ_from_index()
        || opts.text
        || opts.binary
        // Ordinary and local indexes apply the extension heuristic.
        || opts.content_only()
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
        && !opts.effective_passthru()
        && !opts.files_without_match
        && !opts.include_zero
        && let Ok(info) = ServerInfo::load(&index_dir)
        && let Some((index_root, scope)) = resolve_scope(&index_dir, &root)
    {
        match search_via_server(&info, &root, &index_root, &scope, opts, ci, writer) {
            Ok(had_matches) => return Ok(had_matches),
            Err(error) => eprintln!(
                "Server unavailable or incompatible ({error}), falling back to local index"
            ),
        }
    }

    // No server — use on-disk index directly (or brute force)
    if opts.no_index || bypass_index {
        return brute_force_search(&root, &index_dir, opts, ci, writer);
    }
    if !index_dir.join("lookup.bin").exists() {
        warn_missing_index(&index_dir, index_path.is_some(), opts.quiet);
        return brute_force_search(&root, &index_dir, opts, ci, writer);
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
    let request = server_search_request(scope, opts, ci)?;
    writeln!(stream, "{request}")?;
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
    render_server_result(result, &request, root, index_root, scope, opts, writer)
}

fn server_search_request(
    scope: &IndexScope,
    opts: &SearchOptions,
    ci: bool,
) -> Result<serde_json::Value> {
    // Resolve `-f/--file` here rather than sending the path: the server has a
    // different working directory and no `pattern_file` param, so forwarding
    // only `pattern`/`extra_patterns` would silently drop the file's patterns
    // and make results depend on whether a daemon happens to be running.
    let patterns = opts.all_patterns()?;
    let (primary, extras) = patterns
        .split_first()
        .map(|(p, rest)| (p.clone(), rest.to_vec()))
        .unwrap_or_else(|| (opts.pattern.clone(), Vec::new()));
    // Definitions that no `-t`/`-T` consumes cannot change the result, and a
    // server rebuilding them in its legacy order could reject a valid
    // sequence, so they are left out. `run` has already validated them.
    let selects = opts.selects_types();
    let (mut types, mut types_not, mut type_add, mut type_clear) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for op in opts.type_ops.iter().filter(|_| selects) {
        match op {
            filetypes::TypeOp::Select(name) => types.push(name),
            filetypes::TypeOp::Negate(name) => types_not.push(name),
            filetypes::TypeOp::Add(spec) => type_add.push(spec),
            filetypes::TypeOp::Clear(name) => type_clear.push(name),
        }
    }

    // Context lines are pure presentation; requesting them while counting would
    // stream rows the count path has to filter back out.
    let wants_context = !(opts.count || opts.count_matches || opts.files_only || opts.quiet);

    // Spans and columns are the bulk of a reply — a nested array for every
    // match — and most searches never read them. Asking for them only when
    // something will consume them keeps a 21k-hit query from building, sending
    // and parsing 21k arrays that are then dropped.
    // An older server may ignore `count`, so retain the detail it needs for
    // stats. New servers project counts without rendering those fields.
    let detail = !opts.count_matches && opts.wants_match_detail();
    let positions = opts.wants_position_detail();

    let mut request = serde_json::json!({
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
            "types": types,
            "types_not": types_not,
            "type_add": type_add,
            "type_clear": type_clear,
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
            "stats": opts.stats,
        },
        "id": 1,
    });
    request["params"]["hidden"] = serde_json::json!(opts.hidden);
    request["params"]["scope"] = serde_json::json!(scope.prefix());
    request["params"]["max_depth"] = serde_json::json!(opts.max_depth);
    if opts.json {
        request["params"]["search_stats"] = serde_json::json!(true);
    }
    // Older shared servers reject unknown fields, even when false. Leave
    // non-counting requests unchanged.
    if opts.count_matches {
        request["params"]["count_matches"] = serde_json::json!(true);
    } else if opts.count {
        request["params"]["count"] = serde_json::json!(true);
    }
    // The per-kind arrays above lose the order between kinds. Send the ordered
    // form only when that order changes the result.
    if selects && !filetypes::TypeOp::is_unordered_equivalent(&opts.type_ops) {
        request["params"]["type_ops"] = type_ops_to_json(&opts.type_ops);
    }
    // A server keeps its legacy filter order for clients that do not ask for
    // ripgrep's, and a server that predates it ignores the request. Ask only
    // when the result depends on it, so that reply is refused only then.
    if opts.needs_ordered_filters()? {
        request["params"]["ordered_filters"] = serde_json::json!(true);
    }
    // A server that predates a `--replace` rendering cannot honour it, and
    // its reply is then refused. Ask only when the output depends on it.
    if opts.needs_replace_blocks(ci) {
        request["params"]["multiline_replace"] = serde_json::json!(true);
    }
    if opts.report_replacements() {
        request["params"]["report_replacements"] = serde_json::json!(true);
    }
    // Only managed views with a matching profile accept a non-default value;
    // daemons that predate it reject the unknown field, so the client scans.
    if !opts.content_eligibility.is_default() {
        request["params"]["content_eligibility"] = serde_json::json!(opts.content_eligibility);
    }
    Ok(request)
}

/// Fail when the result comes from a server that predates the `--replace`
/// rendering `request` asked for. Such a server ignores the request and
/// rewrites `-U` matches line by line, or leaves `--json` submatches without
/// their `replacement`. Reading the request rather than the options keeps
/// this check in step with what `server_search_request` decided to ask for.
fn require_replace_support(result: &serde_json::Value, request: &serde_json::Value) -> Result<()> {
    let params = &request["params"];
    if flag_set(params, "multiline_replace") && !flag_set(result, "multiline_replace") {
        anyhow::bail!("server does not support multiline replacement blocks");
    }
    if flag_set(params, "report_replacements") && !flag_set(result, "report_replacements") {
        anyhow::bail!("server does not report JSON replacements");
    }
    Ok(())
}

/// Fail when the result comes from a server that predates the ordered
/// filtering `request` asked for. Such a server ignores the request, applies
/// every `-T` after every `-t` and lets `-t` veto a file that a positive glob
/// included.
fn require_ordered_filters(result: &serde_json::Value, request: &serde_json::Value) -> Result<()> {
    if flag_set(&request["params"], "ordered_filters") && !flag_set(result, "ordered_filters") {
        anyhow::bail!("server does not support ordered type and glob filters");
    }
    Ok(())
}

fn flag_set(value: &serde_json::Value, key: &str) -> bool {
    value.get(key).and_then(|v| v.as_bool()) == Some(true)
}

fn projected_line_count(row: &serde_json::Value) -> Result<(&str, usize)> {
    anyhow::ensure!(
        matches!(
            row.get("type").and_then(|t| t.as_str()),
            Some("count" | "binary")
        ),
        "server mixed projected counts with match rows"
    );
    let file = row
        .get("file")
        .and_then(|f| f.as_str())
        .context("server count is missing its file")?;
    let count = row
        .get("lines")
        .and_then(|l| l.as_u64())
        .context("server count is missing its line total")?;
    Ok((
        file,
        usize::try_from(count).context("server line count exceeds platform limits")?,
    ))
}

fn render_server_result(
    result: &serde_json::Value,
    request: &serde_json::Value,
    root: &Path,
    index_root: &Path,
    scope: &IndexScope,
    opts: &SearchOptions,
    writer: &mut OutputWriter,
) -> Result<bool> {
    // Older servers ignore unknown request fields. Validate before writing any
    // rows, so an old/partial server can never silently answer --hidden.
    require_server_coverage(result)?;
    require_ordered_filters(result, request)?;
    require_replace_support(result, request)?;

    let empty_vec = Vec::new();
    let matches = result
        .get("matches")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty_vec);
    let projected_counts = opts.count
        && !opts.count_matches
        && matches
            .iter()
            .any(|m| m.get("type").and_then(|t| t.as_str()) == Some("count"));
    if projected_counts {
        // Validate before scope/binary filtering can hide malformed rows.
        let mut seen = std::collections::HashSet::new();
        for row in matches {
            let (file, _) = projected_line_count(row)?;
            anyhow::ensure!(
                seen.insert(file),
                "server returned duplicate line counts for {file}"
            );
        }
    }

    // The server scopes candidates before searching but still reports paths
    // relative to the index root. Translate those paths for output and apply
    // the same scope defensively to all reply rows.
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
    let search_stats = if opts.json {
        Some(
            server_search_stats(result, matches)?
                .into_iter()
                .filter_map(|stats| {
                    if stats.binary_offset.is_some()
                        && !opts.binary
                        && !matches!(scope, IndexScope::File(_))
                    {
                        return None;
                    }
                    let rel = scope.relativize(&stats.file, root)?;
                    within_max_depth(&rel, opts).then_some((rel, stats.bytes_searched))
                })
                .collect::<std::collections::HashMap<_, _>>(),
        )
    } else {
        None
    };
    let file_stats = if opts.stats || opts.count_matches {
        result
            .get("file_stats")
            .map(|stats| -> Result<Vec<FileMatchStats>> {
                let stats: Vec<FileMatchStats> = serde_json::from_value(stats.clone())
                    .context("invalid server per-file match totals")?;
                Ok(stats
                    .into_iter()
                    .filter_map(|mut stats| {
                        if dropped_binary_files.contains(stats.file.as_str()) {
                            return None;
                        }
                        let rel = scope.relativize(&stats.file, root)?;
                        if !within_max_depth(&rel, opts) {
                            return None;
                        }
                        stats.file = rel;
                        Some(stats)
                    })
                    .collect())
            })
            .transpose()?
    } else {
        None
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
            let mut ranked = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for m in matches {
                let Some(rel) = m.get("file").and_then(|f| f.as_str()) else {
                    continue;
                };
                if !seen.insert(rel.to_string()) {
                    continue;
                }
                ranked.push(rel.to_string());
            }
            sort.apply_indexed(&mut ranked, index_root, scope, String::as_str)?;
            let rank: std::collections::HashMap<&str, usize> = ranked
                .iter()
                .enumerate()
                .map(|(i, file)| (file.as_str(), i))
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

    if let Some(stats) = &search_stats {
        for row in matches {
            let file = row["file"]
                .as_str()
                .context("server row is missing its file")?;
            anyhow::ensure!(
                stats.contains_key(file),
                "server JSON search statistics disagree with visible rows for {file}"
            );
        }
        for &bytes in stats.values() {
            writer.note_bytes_searched(bytes);
        }
    }

    if opts.quiet {
        // No output, but still report stats for the first matching file.
    } else if opts.files_only {
        let mut seen = std::collections::HashSet::new();
        for m in matches {
            if let Some(file) = m.get("file").and_then(|f| f.as_str())
                && seen.insert(file.to_string())
            {
                writer.write_file(file)?;
            }
        }
    } else if opts.count_matches {
        // Validate every file before writing any counts: a partial or old
        // response must not mix guessed counts with a local fallback.
        let counts = original_match_counts(matches, file_stats.as_deref(), opts.invert_match)?;
        for (file, count) in counts {
            writer.write_count(file, count)?;
        }
    } else if opts.count {
        // New servers project `FileMatches::matched_lines` into one count row.
        // Older servers send full rows: collapse repeated lines and ignore
        // context, preserving their existing count semantics.
        let mut order: Vec<String> = Vec::new();
        let mut lines: std::collections::HashMap<String, std::collections::HashSet<usize>> =
            std::collections::HashMap::new();
        // A binary file is summarised as a single `binary` row rather than one
        // row per match, so it carries its own line count. The local path still
        // prints a real count for it because `-c` returns before the binary
        // guard, and dropping these rows here would silently omit the file.
        let mut exact: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for m in matches {
            let mtype = m.get("type").and_then(|t| t.as_str()).unwrap_or("match");
            if mtype != "match" && mtype != "binary" && mtype != "count" {
                continue;
            }
            let Some(file) = m.get("file").and_then(|f| f.as_str()) else {
                continue;
            };
            if !lines.contains_key(file) && !exact.contains_key(file) {
                order.push(file.to_string());
            }
            if mtype == "count" || mtype == "binary" {
                let n = if projected_counts {
                    projected_line_count(m)?.1
                } else {
                    m.get("lines").and_then(|l| l.as_u64()).unwrap_or(0) as usize
                };
                exact.insert(file.to_string(), n);
            } else {
                let line = m.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as usize;
                lines.entry(file.to_string()).or_default().insert(line);
            }
        }
        // Emit in response order; iterating the maps would randomise the file
        // order on every run, unlike the local path which prints in walk order.
        for file in &order {
            let n = exact
                .get(file)
                .copied()
                .or_else(|| lines.get(file).map(|l| l.len()))
                .unwrap_or(0);
            writer.write_count(file, n)?;
        }
    } else {
        for m in matches {
            let file = m.get("file").and_then(|f| f.as_str()).unwrap_or("");
            if let Some(stats) = &search_stats {
                writer.set_file_bytes_searched(stats[file]);
            }
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
                // `-U --replace` rows carry the source lines of their block,
                // which is what decides context gaps.
                let block = m.get("block").and_then(|b| b.as_array()).and_then(|b| {
                    Some((b.first()?.as_u64()? as usize, b.get(1)?.as_u64()? as usize))
                });
                let replacements = m.get("replacements").and_then(|r| r.as_array()).map(|arr| {
                    arr.iter()
                        .map(|t| t.as_str().unwrap_or("").to_string())
                        .collect()
                });
                writer.write_context_separator(file, block.map_or(line, |b| b.0))?;
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
                    block,
                    hidden: m.get("hidden").and_then(|h| h.as_bool()).unwrap_or(false),
                    replacements,
                })?;
            }
        }
    }

    if opts.stats
        && let Some(elapsed) = result.get("elapsed_ms").and_then(|e| e.as_f64())
    {
        // Match output is buffered on stdout, while stats go straight to
        // stderr. Flush first so a combined stdout/stderr stream reports the
        // summary after the matches, as ripgrep does.
        flush_before_stats(writer)?;
        // Apply the same scope to per-file totals as to output rows. The
        // server's `num_matches` includes context rows, and
        // rendered spans can split or merge the original matches.
        let (num, lines) = if let Some(stats) = &file_stats {
            let first_file = matches
                .first()
                .and_then(|m| m.get("file"))
                .and_then(|f| f.as_str());
            stats
                .iter()
                .filter(|s| !opts.quiet || first_file == Some(s.file.as_str()))
                .fold((0, 0), |(m, l), s| (m + s.matches, l + s.matched_lines))
        } else {
            // Older servers only provide rendered rows.
            count_reported_matches(matches)
        };
        let mut narrowing_note = "";
        if let Some(stats) = result.get("index_stats")
            && let (Some(plan), Some(raw), Some(candidates), Some(total)) = (
                stats.get("query_plan").and_then(|v| v.as_str()),
                stats.get("raw_candidates").and_then(|v| v.as_u64()),
                stats.get("candidates").and_then(|v| v.as_u64()),
                stats.get("total_files").and_then(|v| v.as_u64()),
            )
        {
            print_index_stats(plan, raw, candidates, total);
            narrowing_note = index_narrowing_note(raw, total);
        }
        let backend = server_backend_label(result);
        eprintln!(
            "{num} matches ({lines} matched lines) in {elapsed:.1}ms (via {backend}){narrowing_note}"
        );
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
    let meta = match IndexMeta::load(index_dir) {
        Ok(meta) => meta,
        Err(error) => {
            if !opts.quiet && !opts.no_messages {
                eprintln!(
                    "warning: index coverage metadata unavailable ({error}) - scanning filesystem"
                );
            }
            return brute_force_search(root, index_dir, opts, ci, writer);
        }
    };
    if !meta.complete
        || !meta.hidden_complete
        || meta.version != tgrep_core::meta::INDEX_FORMAT_VERSION
    {
        if !opts.quiet && !opts.no_messages {
            eprintln!(
                "warning: index lacks complete hidden-file coverage - scanning filesystem; rebuild with `tgrep index` or upgrade with `tgrep serve`"
            );
        }
        return brute_force_search(root, index_dir, opts, ci, writer);
    }
    let reader = IndexReader::open(index_dir)?;
    let filename_visibility = match tgrep_core::path_index::read_filename_index(index_dir) {
        Ok(Some(index)) => index.visibility,
        Ok(None) | Err(_) => None,
    };
    let Some(filename_visibility) = filename_visibility else {
        if !opts.quiet && !opts.no_messages {
            eprintln!("warning: atomic index visibility unavailable - scanning filesystem");
        }
        return brute_force_search(root, index_dir, opts, ci, writer);
    };
    if !filename_visibility.covers_index(&meta, reader.file_table_id()) {
        if !opts.quiet && !opts.no_messages {
            eprintln!(
                "warning: index visibility, coverage, and path table differ - scanning filesystem"
            );
        }
        return brute_force_search(root, index_dir, opts, ci, writer);
    }
    let visibility = filename_visibility.paths;

    // Index paths are relative to the root the index was built for, which is
    // not necessarily the directory being searched. Without translating between
    // the two, `tgrep --index-path IDX foo src` looks for `src/src/lib.rs` and
    // silently reports nothing.
    let Some((index_root, scope)) = resolve_scope(index_dir, root) else {
        if !opts.quiet && !opts.no_messages {
            eprintln!("warning: index root unavailable or unrelated - scanning filesystem");
        }
        return brute_force_search(root, index_dir, opts, ci, writer);
    };
    let rooted = tgrep_core::rooted::RootedDir::open(root)?;

    let glob_filter = opts.glob_filter()?;
    let type_filter = opts.type_filter()?;

    let matcher = opts.matcher(ci)?;

    // Narrow candidates using every pattern, not just the primary one.
    // `--passthru` must visit files without any pattern trigrams, while a
    // non-default `--encoding` re-decodes files into text the index never saw,
    // so neither can use the trigram plan.
    //
    // A PCRE-style pattern is not parseable by `regex-syntax`, but relaxing it
    // (dropping lookarounds and the like) yields one that is, and that matches a
    // superset — so its trigrams remain mandatory for the original.
    let plan = if opts.effective_passthru() || opts.encoding.may_differ_from_index() {
        QueryPlan::MatchAll
    } else if matcher.is_standard() || opts.fixed_string {
        query::build_multi_pattern_plan(&opts.all_patterns()?, opts.fixed_string, ci)
            .map_err(|e| anyhow::anyhow!("{e}"))?
    } else {
        query::build_relaxed_multi_pattern_plan(&opts.all_patterns()?, ci)
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
    let mut candidates: Vec<String> = candidate_ids
        .iter()
        .filter_map(|&fid| {
            let indexed = reader.file_path(fid)?;
            visibility
                .is_visible(indexed, scope.prefix(), opts.hidden)
                .then_some(())?;
            let rel = scope.relativize(indexed, root)?;
            within_max_depth(&rel, opts).then_some(())?;
            passes_filters(&rel, &glob_filter, &type_filter, true).then_some(())?;
            Some(rel)
        })
        .collect();

    if let Some(sort) = opts.sort {
        sort.apply_indexed(&mut candidates, &index_root, &scope, String::as_str)?;
    }

    let mut had_matches = false;
    // A single-file search root is a file the user named on the command line,
    // which is what makes binary files visible in ripgrep.
    let explicit = matches!(scope, IndexScope::File(_));

    for paths in tgrep_core::rooted::sibling_batches(&candidates) {
        let parent = Path::new(&paths[0])
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let batch = match (paths.len() > 1)
            .then(|| rooted.read_batch(parent))
            .transpose()
        {
            Ok(batch) => batch,
            Err(error) => {
                if !opts.no_messages {
                    for rel_path in paths {
                        eprintln!("warning: cannot read indexed file {rel_path:?}: {error}");
                    }
                }
                continue;
            }
        };
        for rel_path in paths {
            let read = (|| -> std::io::Result<Option<FileRead>> {
                let file = match &batch {
                    Some(batch) => batch.open_file(Path::new(rel_path))?,
                    None => rooted.open_file(Path::new(rel_path))?,
                };
                read_open_text_lossy(file, opts.encoding, read_size_limit(opts, explicit))
            })();
            let read = match read {
                Ok(Some(read)) => read,
                Ok(None) => continue,
                Err(error) => {
                    if !opts.no_messages {
                        eprintln!("warning: cannot read indexed file {rel_path:?}: {error}");
                    }
                    continue;
                }
            };

            if search_file(&read, &matcher, rel_path, opts, writer, explicit)? {
                had_matches = true;
                if opts.quiet {
                    break;
                }
            }
        }
        if let Some(batch) = batch {
            batch
                .finish()
                .context("verifying indexed candidate ancestors")?;
        }
        if had_matches && opts.quiet {
            break;
        }
    }

    if opts.stats {
        let elapsed = start.elapsed();
        // Match output is buffered on stdout, while stats go straight to
        // stderr. Flush first so a combined stdout/stderr stream reports the
        // summary after the matches, as ripgrep does.
        flush_before_stats(writer)?;
        let raw = candidate_ids.len() as u64;
        let total = reader.num_files() as u64;
        print_index_stats(&plan_summary(&plan), raw, candidates.len() as u64, total);
        let narrowing_note = index_narrowing_note(raw, total);
        let (matches, matched_lines) = writer.match_totals();
        eprintln!(
            "Search completed in {:.1}ms: {matches} matches ({matched_lines} matched lines){narrowing_note}",
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
    fn prefix(&self) -> &str {
        match self {
            Self::Subtree(prefix) => prefix,
            Self::Whole | Self::File(_) => "",
        }
    }

    /// `None` when `search_root` lies outside the indexed tree, which means the
    /// index cannot answer the search at all.
    fn resolve(index_root: &Path, search_root: &Path) -> Option<Self> {
        let rel = search_root.strip_prefix(index_root).ok()?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() {
            Some(Self::Whole)
        } else if search_root.is_file() {
            Some(Self::File(rel))
        } else {
            Some(Self::Subtree(format!("{rel}/")))
        }
    }

    /// Re-express an index-relative path relative to the search root, or `None`
    /// when the file is outside it.
    fn relativize(&self, indexed: &str, search_root: &Path) -> Option<String> {
        match self {
            Self::Whole => Some(indexed.to_string()),
            Self::Subtree(prefix) => indexed.strip_prefix(prefix).map(str::to_string),
            // A named file prints as the user reached it, exactly as the
            // brute-force path does.
            Self::File(f) => (f == indexed).then(|| explicit_file_display_path(search_root)),
        }
    }

    fn indexed_path(&self, rel: &str) -> PathBuf {
        let indexed = match self {
            Self::Whole => rel.to_string(),
            Self::Subtree(prefix) => format!("{prefix}{rel}"),
            // `rel` is the path as the user typed it, so go back to the one the
            // index stores.
            Self::File(f) => f.clone(),
        };
        PathBuf::from(indexed.replace('/', std::path::MAIN_SEPARATOR_STR))
    }
}

/// The slice of the index at `index_dir` that covers `root`, with the absolute
/// root the index was built for.
fn resolve_scope(index_dir: &Path, root: &Path) -> Option<(PathBuf, IndexScope)> {
    let recorded_root = PathBuf::from(IndexMeta::load(index_dir).ok()?.root_path);
    if !recorded_root.is_absolute() {
        return None;
    }
    let index_root = std::fs::canonicalize(recorded_root).ok()?;
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

fn index_storage_exclusions(index_dir: &Path) -> Result<Vec<PathBuf>> {
    match std::fs::canonicalize(index_dir) {
        Ok(path) => Ok(vec![path]),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn brute_force_search(
    root: &Path,
    index_dir: &Path,
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
        if passes_filters(&rel_path, &glob_filter, &type_filter, true)
            && let Some(read) = read_text_lossy(root, opts.encoding, read_size_limit(opts, true))?
        {
            had_matches = search_file(&read, &matcher, &rel_path, opts, writer, true)?;
        }

        if opts.stats {
            let elapsed = start.elapsed();
            // Match output is buffered on stdout, while stats go straight to
            // stderr. Flush first so a combined stdout/stderr stream reports
            // the summary after the matches, as ripgrep does.
            flush_before_stats(writer)?;
            let (matches, matched_lines) = writer.match_totals();
            eprintln!(
                "Brute-force search completed in {:.1}ms (1 file): {matches} matches ({matched_lines} matched lines)",
                elapsed.as_secs_f64() * 1000.0,
            );
        }

        writer.flush()?;
        return Ok(had_matches);
    }

    let mut walk = walker::walk_dir(
        root,
        &walker::WalkOptions {
            exclude_paths: index_storage_exclusions(index_dir)?,
            overrides: Some(glob_filter.walk_overrides(root)?),
            ..opts.walk_options()
        },
    );
    if let Some(sort) = opts.sort {
        sort.apply(&mut walk.files);
    }

    for path in &walk.files {
        let rel_path = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        if !passes_filters(&rel_path, &glob_filter, &type_filter, true) {
            continue;
        }

        let read = match read_text_lossy(path, opts.encoding, opts.max_filesize) {
            Ok(Some(read)) => read,
            Ok(None) => continue,
            Err(error) => {
                if !opts.no_messages {
                    eprintln!("warning: cannot read file {}: {error}", path.display());
                }
                continue;
            }
        };

        if search_file(&read, &matcher, &rel_path, opts, writer, false)? {
            had_matches = true;
            if opts.quiet {
                break;
            }
        }
    }

    if opts.stats {
        let elapsed = start.elapsed();
        // Match output is buffered on stdout, while stats go straight to
        // stderr. Flush first so a combined stdout/stderr stream reports the
        // summary after the matches, as ripgrep does.
        flush_before_stats(writer)?;
        let (matches, matched_lines) = writer.match_totals();
        eprintln!(
            "Brute-force search completed in {:.1}ms ({} files): {matches} matches ({matched_lines} matched lines)",
            elapsed.as_secs_f64() * 1000.0,
            walk.files.len()
        );
    }

    writer.flush()?;
    Ok(had_matches)
}

/// Smallest file worth memory-mapping instead of reading.
///
/// Mapping costs a syscall pair and a page-table setup per file, which is a bad
/// trade for the small files that dominate a repository walk — a kernel tree is
/// ~94k files averaging well under 64 KB. Above this size the whole-file read is
/// the dominant cost and mapping wins outright, so the threshold buys the
/// large-file case without taxing the common one.
///
/// 1 MiB is far above the size of real source — only 110 of the kernel's 94,747
/// files reach it — so ordinary searches never map, while the large generated
/// files that actually cost memory always do.
const MMAP_MIN_BYTES: u64 = 1024 * 1024;

/// The text of a file to search, either owned on the heap or borrowed from a
/// memory map.
///
/// Reading a file with `fs::read` costs its full size in heap, per file in
/// flight. ripgrep searches a 192 MB file in under 10 MiB because its memory is
/// decoupled from file size; mapping large files gives the same property here,
/// since mapped pages are file-backed and the OS can evict them under pressure
/// instead of the process holding an allocation it cannot give back.
enum FileText {
    Owned(String),
    /// Mapped bytes verified as UTF-8 when the map was created.
    Mapped(memmap2::Mmap),
}

impl FileText {
    fn as_str(&self) -> &str {
        match self {
            Self::Owned(text) => text,
            // SAFETY: `Mapped` is only constructed by `try_map_text`, which
            // validates the entire mapping with `str::from_utf8` first, so the
            // bytes were UTF-8 when the map was taken. A concurrent writer
            // could still invalidate them, which is the same exposure ripgrep
            // accepts when it maps a file it is searching.
            Self::Mapped(map) => unsafe { std::str::from_utf8_unchecked(map) },
        }
    }
}

/// A file's text together with what the read already learned about it.
struct FileRead {
    text: FileText,
    fixups: tgrep_core::encoding::LossyFixups,
    /// Offset of the first NUL byte in `text`, which is what makes the file
    /// binary. Found during the pass that decoded or validated the bytes, not
    /// in a pass of its own — see [`validate_utf8_and_find_nul`].
    first_nul: Option<usize>,
}

/// Validate `bytes` as UTF-8 and locate the first NUL byte in one traversal.
///
/// Returns `None` if the bytes are not valid UTF-8, which is the caller's
/// signal to fall back to reading and repairing them.
///
/// Both questions need every byte, and asking them separately means walking the
/// file twice. That is nearly free while it stays in the page cache and is
/// anything but on a file larger than the cache, where the second walk is
/// another trip to disk: on a 13.4 GiB file, validating cost 8.7 s and a
/// separate `memchr` for the NUL cost another 6.2 s. Interleaving them at
/// chunk granularity keeps each page hot for both.
///
/// The chunking is what makes that possible, and it is why this cannot simply
/// call `str::from_utf8` per chunk: a multi-byte sequence can straddle a chunk
/// boundary, where `from_utf8` reports an incomplete sequence. That is not an
/// error mid-file — it means "resume here" — so the scan restarts at
/// `valid_up_to` and lets the next chunk carry the sequence. At the *last*
/// chunk the same report is a real truncation and does fail.
fn validate_utf8_and_find_nul(bytes: &[u8]) -> Option<Option<usize>> {
    /// Large enough that the ≤3 carried-over bytes are noise, small enough to
    /// stay in cache between the two scans of it.
    const CHUNK: usize = 1 << 20;

    let mut pos = 0;
    let mut first_nul = None;
    while pos < bytes.len() {
        let end = (pos + CHUNK).min(bytes.len());
        let chunk = &bytes[pos..end];
        let valid_len = match std::str::from_utf8(chunk) {
            Ok(_) => chunk.len(),
            Err(e) if e.error_len().is_none() && end < bytes.len() => e.valid_up_to(),
            Err(_) => return None,
        };
        // A chunk starts on a character boundary and a UTF-8 sequence is at
        // most 4 bytes, so a chunk this size always validates at least part of
        // itself unless it opens with a genuinely invalid byte. Bailing keeps
        // the loop from spinning if that reasoning ever stops holding.
        if valid_len == 0 {
            return None;
        }
        if first_nul.is_none()
            && let Some(i) = memchr::memchr(0, &chunk[..valid_len])
        {
            first_nul = Some(pos + i);
        }
        pos += valid_len;
    }
    Some(first_nul)
}

/// Map a file for searching, or `None` to fall back to reading it.
///
/// Declines whenever the mapped pages would not be exactly the bytes to search:
/// a file below the threshold, an encoding that transcodes or strips a BOM, or
/// content that is not already valid UTF-8 and so needs lossy repair.
fn try_map_text(
    file: &std::fs::File,
    encoding: tgrep_core::encoding::EncodingMode,
    approved_len: u64,
) -> Option<(FileText, Option<usize>)> {
    if approved_len < MMAP_MIN_BYTES {
        return None;
    }
    // Pin the approved extent even if the file grows before mapping.
    let length = usize::try_from(approved_len).ok()?;
    // SAFETY: the map is read-only and owned by the returned `FileText`.
    // Mapping is undefined behaviour if another process truncates the file
    // underneath us; that is inherent to searching by map and is the tradeoff
    // ripgrep makes.
    let map = unsafe { memmap2::MmapOptions::new().len(length).map(file).ok()? };
    if !tgrep_core::encoding::borrows_whole_input(&map, encoding) {
        return None;
    }
    let first_nul = validate_utf8_and_find_nul(&map)?;
    Some((FileText::Mapped(map), first_nul))
}

/// Read a file as text, applying `--encoding` and replacing invalid UTF-8
/// rather than failing. `None` omits a file exceeding the source-byte limit.
///
/// ripgrep searches files that are not valid UTF-8; refusing them makes
/// UTF-16, Latin-1 and mixed-encoding sources silently invisible.
fn read_text_lossy(
    path: &Path,
    encoding: tgrep_core::encoding::EncodingMode,
    limit: Option<u64>,
) -> std::io::Result<Option<FileRead>> {
    read_open_text_lossy(std::fs::File::open(path)?, encoding, limit)
}

fn read_open_text_lossy(
    mut file: std::fs::File,
    encoding: tgrep_core::encoding::EncodingMode,
    limit: Option<u64>,
) -> std::io::Result<Option<FileRead>> {
    let length = file.metadata()?.len();
    if limit.is_some_and(|limit| length > limit) {
        return Ok(None);
    }

    // A mapped file is always already-valid UTF-8, so it needs no fixups.
    if let Some((text, first_nul)) = try_map_text(&file, encoding, length) {
        if let Some(limit) = limit
            && file.metadata()?.len() > limit
        {
            return Ok(None);
        }
        return Ok(Some(FileRead {
            text,
            fixups: tgrep_core::encoding::LossyFixups::default(),
            first_nul,
        }));
    }
    let Some(bytes) = read_bytes_with_limit(&mut file, limit, length.min(1 << 20) as usize)? else {
        return Ok(None);
    };
    if let Some(limit) = limit
        && file.metadata()?.len() > limit
    {
        return Ok(None);
    }
    let (text, fixups) = tgrep_core::encoding::decode_owned_with_fixups(bytes, encoding);
    // The read path already walks these bytes at least once and they are under
    // the mapping threshold or repaired, so a separate scan here is cheap. It
    // has to run over the *repaired* text, since that is what offsets are
    // reported against.
    let first_nul = memchr::memchr(0, text.as_bytes());
    Ok(Some(FileRead {
        text: FileText::Owned(text),
        fixups,
        first_nul,
    }))
}

/// Read at most one byte beyond the limit; `None` rejects oversized content.
pub(crate) fn read_bytes_with_limit(
    mut reader: impl std::io::Read,
    limit: Option<u64>,
    capacity: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;

    let capacity = limit.map_or(capacity, |limit| {
        capacity.min(usize::try_from(limit).unwrap_or(usize::MAX))
    });
    let mut bytes = Vec::with_capacity(capacity);
    match limit {
        Some(limit) => {
            reader
                .take(limit.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > limit {
                return Ok(None);
            }
        }
        None => {
            reader.read_to_end(&mut bytes)?;
        }
    }
    Ok(Some(bytes))
}

/// The read limit, preserving the explicit-file exemption from the default cap.
///
/// `explicit` marks a file the user named on the command line. Those are exempt
/// from the *inherited* [`tgrep_core::walker::DEFAULT_MAX_FILE_SIZE`], because
/// naming a file is an unambiguous request to search it and answering "no
/// match" would be a lie. A limit the user actually passed still applies, since
/// then the limit is itself the request.
fn read_size_limit(opts: &SearchOptions, explicit: bool) -> Option<u64> {
    if explicit && !opts.max_filesize_requested {
        return None;
    }
    opts.max_filesize
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

/// Whether a searched file satisfies the requested mode. Skipped files are
/// never selected, even by `--files-without-match`.
fn search_file(
    read: &FileRead,
    matcher: &SearchMatcher,
    rel_path: &str,
    opts: &SearchOptions,
    writer: &mut OutputWriter,
    explicit: bool,
) -> Result<bool> {
    match search_decoded_file(read, matcher, rel_path, opts, writer, explicit)? {
        FileOutcome::Matched => Ok(!opts.files_without_match),
        FileOutcome::NoMatch if opts.files_without_match => {
            if !opts.quiet {
                writer.write_file(rel_path)?;
            }
            Ok(true)
        }
        FileOutcome::NoMatch | FileOutcome::Skipped => Ok(false),
    }
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
    read: &FileRead,
    matcher: &SearchMatcher,
    rel_path: &str,
    opts: &SearchOptions,
    writer: &mut OutputWriter,
    explicit: bool,
) -> Result<FileOutcome> {
    let FileRead {
        text,
        fixups,
        first_nul,
    } = read;
    let content = text.as_str();

    // ripgrep only surfaces a binary file when the user named it explicitly (or
    // passed `--binary`).
    //
    // `first_nul` was found by the pass that read the file, so this costs
    // nothing here. The NUL is located in the repaired text but reported in
    // terms of the file on disk, so it goes back through `fixups`: repairing
    // invalid UTF-8 ahead of the NUL widens every bad byte to three, which
    // would otherwise push the reported offset past where the byte actually is.
    let binary_offset = if opts.text {
        None
    } else {
        first_nul.map(|off| fixups.to_source_offset(off))
    };
    if binary_offset.is_some() && !explicit && !opts.binary {
        return Ok(FileOutcome::Skipped);
    }

    // Binary bytes_searched reports the detection offset, not a regex cutoff:
    // ripgrep with memory-mapped input can count matches beyond the NUL while
    // reporting that offset. Both byte counts refer to the source on disk.
    writer.note_bytes_searched(
        binary_offset.unwrap_or_else(|| fixups.to_source_offset(content.len())) as u64,
    );

    let match_opts = opts.match_options();
    let found = crate::matching::FileMatches::find(content, matcher, &match_opts)?;
    if opts.stats {
        let (matches, matched_lines) = found.match_totals();
        writer.note_matches(matches, matched_lines);
    }

    let has_matches = !found.is_empty();
    if !has_matches {
        // `--include-zero` still reports files that were searched but had no
        // match, which is the only way to tell them apart from files that were
        // never looked at.
        if opts.include_zero && (opts.count || opts.count_matches) && !opts.quiet {
            writer.write_count(rel_path, 0)?;
        }
        // Passthru still emits every line of a text file when none matched, but
        // ripgrep does not expose a binary file merely because passthru is set.
        if !match_opts.passthru || binary_offset.is_some() {
            return Ok(FileOutcome::NoMatch);
        }
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
                block,
                hidden,
                replacements,
            } => {
                // A rewritten block is a gap only if its source lines are.
                writer.write_context_separator(rel_path, block.map_or(line_number, |b| b.0))?;
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
                    block,
                    hidden,
                    replacements,
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

    Ok(if has_matches {
        FileOutcome::Matched
    } else {
        FileOutcome::NoMatch
    })
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

/// Whether a path survives the glob and type filters. As in ripgrep, a glob
/// that decides a file overrides `-t`/`-T`, which only apply to files no glob
/// decides. Without `glob_precedence`, as a server answers a client that
/// predates it, a file a positive glob includes must also pass `-t`/`-T`.
pub(crate) fn passes_filters(
    rel_path: &str,
    glob_filter: &crate::glob_filter::GlobFilter,
    type_filter: &filetypes::TypeFilter,
    glob_precedence: bool,
) -> bool {
    match glob_filter.decide(rel_path) {
        crate::glob_filter::GlobDecision::Exclude => false,
        crate::glob_filter::GlobDecision::Include if glob_precedence => true,
        crate::glob_filter::GlobDecision::Include | crate::glob_filter::GlobDecision::Unmatched => {
            type_filter.matches(rel_path)
        }
    }
}

/// Encode type ops for the `type_ops` request field, e.g.
/// `[["clear","rust"],["add","rust:*.txt"],["select","rust"]]`.
pub(crate) fn type_ops_to_json(ops: &[filetypes::TypeOp]) -> serde_json::Value {
    ops.iter()
        .map(|op| {
            let (kind, value) = match op {
                filetypes::TypeOp::Add(spec) => ("add", spec),
                filetypes::TypeOp::Clear(name) => ("clear", name),
                filetypes::TypeOp::Select(name) => ("select", name),
                filetypes::TypeOp::Negate(name) => ("negate", name),
            };
            serde_json::json!([kind, value])
        })
        .collect()
}

/// Decode the `type_ops` request field, rejecting anything malformed.
pub(crate) fn type_ops_from_json(
    value: &serde_json::Value,
) -> std::result::Result<Vec<filetypes::TypeOp>, String> {
    let malformed = || "type_ops must be an array of [kind, value] string pairs".to_string();
    value
        .as_array()
        .ok_or_else(malformed)?
        .iter()
        .map(|op| {
            let [kind, value] = op.as_array().map(Vec::as_slice).ok_or_else(malformed)? else {
                return Err(malformed());
            };
            let (Some(kind), Some(value)) = (kind.as_str(), value.as_str()) else {
                return Err(malformed());
            };
            let value = value.to_string();
            Ok(match kind {
                "add" => filetypes::TypeOp::Add(value),
                "clear" => filetypes::TypeOp::Clear(value),
                "select" => filetypes::TypeOp::Select(value),
                "negate" => filetypes::TypeOp::Negate(value),
                _ => return Err(format!("unknown type_ops kind: {kind}")),
            })
        })
        .collect()
}

/// Original per-file RPC totals for stats and counts, independent of rendered rows.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct FileMatchStats {
    pub file: String,
    pub matches: u64,
    pub matched_lines: u64,
}

/// Work performed on a candidate, even when it produced no output rows.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct FileSearchStats {
    pub file: String,
    pub bytes_searched: u64,
    pub binary_offset: Option<u64>,
}

fn server_search_stats(
    result: &serde_json::Value,
    rows: &[serde_json::Value],
) -> Result<Vec<FileSearchStats>> {
    let stats: Vec<FileSearchStats> = serde_json::from_value(
        result
            .get("search_stats")
            .context("server omitted JSON search statistics; upgrade the server")?
            .clone(),
    )
    .context("invalid server JSON search statistics")?;
    let mut seen = std::collections::HashSet::new();
    let mut bytes = 0_u64;
    for s in &stats {
        anyhow::ensure!(
            seen.insert(s.file.as_str()),
            "server returned duplicate search statistics for {}",
            s.file
        );
        bytes = bytes
            .checked_add(s.bytes_searched)
            .context("server JSON searched byte total overflows")?;
    }
    for row in rows {
        let file = row
            .get("file")
            .and_then(|value| value.as_str())
            .context("server row is missing its file")?;
        anyhow::ensure!(
            seen.contains(file),
            "server omitted JSON search statistics for {file}"
        );
    }
    Ok(stats)
}

fn original_match_counts<'a>(
    rows: &'a [serde_json::Value],
    stats: Option<&[FileMatchStats]>,
    invert_match: bool,
) -> Result<Vec<(&'a str, usize)>> {
    let stats = stats.context(
        "server does not provide per-file match totals required by --count-matches; upgrade the server",
    )?;
    let mut by_file = std::collections::HashMap::new();
    for s in stats {
        let count = if invert_match {
            s.matched_lines
        } else {
            s.matches
        };
        let count = usize::try_from(count).context("server match count exceeds platform limits")?;
        anyhow::ensure!(
            by_file.insert(s.file.as_str(), count).is_none(),
            "server returned duplicate match totals for {}",
            s.file
        );
    }
    let mut counts = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        if !matches!(
            row.get("type").and_then(|t| t.as_str()).unwrap_or("match"),
            "match" | "binary"
        ) {
            continue;
        }
        let file = row
            .get("file")
            .and_then(|f| f.as_str())
            .context("server match is missing its file")?;
        if seen.insert(file) {
            let count = by_file
                .remove(file)
                .with_context(|| format!("server omitted match totals for {file}"))?;
            counts.push((file, count));
        }
    }
    anyhow::ensure!(
        by_file.is_empty(),
        "server returned match totals without matching files"
    );
    Ok(counts)
}

/// Legacy match and matched-line totals for `--stats`, counted from the rows the
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

fn print_index_stats(plan: &str, raw: u64, candidates: u64, total: u64) {
    eprintln!(
        "Query plan: {plan} (candidates: {candidates}/{total}; raw candidates: {raw}/{total})"
    );
}

fn index_narrowing_note(raw: u64, total: u64) -> &'static str {
    if total > 0 && raw == total {
        " (no index narrowing)"
    } else {
        ""
    }
}

pub(crate) fn plan_summary(plan: &QueryPlan) -> String {
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

    #[test]
    fn backend_labels_distinguish_managed_and_legacy_shared_responses() {
        for (result, expected) in [
            (
                serde_json::json!({"backend":"shared-v1"}),
                "shared daemon v1",
            ),
            (
                serde_json::json!({"backend":"shared-v2"}),
                "shared daemon v2",
            ),
            (serde_json::json!({}), "server"),
            (serde_json::json!({"backend":"server"}), "server"),
        ] {
            assert_eq!(server_backend_label(&result), expected);
        }
    }

    #[test]
    fn pattern_file_lines_are_verbatim_except_for_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("patterns.txt");
        let opts = SearchOptions {
            pattern_sources: vec![PatternSource::File(path.to_str().unwrap().to_string())],
            ..Default::default()
        };
        for (contents, expected) in [
            ("", vec![]),
            ("\n", vec![""]),
            ("\r\n", vec![""]),
            ("alpha\n\n", vec!["alpha", ""]),
            ("alpha\r\nbeta\r\n", vec!["alpha", "beta"]),
            (" \ttest \t", vec![" \ttest \t"]),
            ("alpha\r", vec!["alpha\r"]),
        ] {
            std::fs::write(&path, contents).unwrap();
            assert_eq!(opts.all_patterns().unwrap(), expected, "{contents:?}");
        }
    }

    #[test]
    fn resolved_patterns_keep_empty_alternatives_and_do_not_reread_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("patterns.txt");
        std::fs::write(&path, "\n UPPER \n").unwrap();
        let mut opts = SearchOptions {
            pattern_sources: vec![
                PatternSource::Regexp("lower".into()),
                PatternSource::File(path.to_str().unwrap().to_string()),
            ],
            smart_case: true,
            ..Default::default()
        };
        assert!(opts.resolve_patterns().unwrap());
        std::fs::write(&path, "changed\n").unwrap();
        assert_eq!(opts.all_patterns().unwrap(), ["lower", "", " UPPER "]);
        assert!(!opts.effective_case_insensitive().unwrap());

        let mut empty = SearchOptions::default();
        assert!(empty.resolve_patterns().unwrap());
        assert_eq!(empty.all_patterns().unwrap(), [""]);
    }

    #[test]
    fn pattern_sources_keep_command_line_order_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        std::fs::write(&first, "one\ntwo\n").unwrap();
        std::fs::write(&second, "four\n").unwrap();
        let mut opts = SearchOptions {
            pattern: "ignored".into(),
            pattern_sources: vec![
                PatternSource::File(first.to_str().unwrap().to_string()),
                PatternSource::Regexp("three".into()),
                PatternSource::File(second.to_str().unwrap().to_string()),
                PatternSource::Regexp("five".into()),
            ],
            ..Default::default()
        };
        assert!(opts.resolve_patterns().unwrap());
        assert_eq!(
            opts.all_patterns().unwrap(),
            ["one", "two", "three", "four", "five"]
        );

        let empty_file = dir.path().join("empty.txt");
        std::fs::write(&empty_file, "").unwrap();
        let mut none = SearchOptions {
            pattern_sources: vec![
                PatternSource::File(empty_file.to_str().unwrap().to_string()),
                PatternSource::File(empty_file.to_str().unwrap().to_string()),
            ],
            ..Default::default()
        };
        assert!(!none.resolve_patterns().unwrap());
    }

    #[test]
    fn bounded_reads_stop_at_the_limit_plus_one_and_preserve_boundaries() {
        for (length, limit, accepted, consumed) in [
            (0, Some(0), true, 0),
            (1, Some(0), false, 1),
            (64, Some(64), true, 64),
            (4096, Some(64), false, 65),
            (4096, None, true, 4096),
            (64, Some(u64::MAX), true, 64),
        ] {
            let mut reader = std::io::Cursor::new(vec![b'a'; length]);
            // A stale capacity hint must not become a read or allocation bound.
            let result = read_bytes_with_limit(&mut reader, limit, 16).unwrap();
            assert_eq!(
                result.is_some(),
                accepted,
                "length={length}, limit={limit:?}"
            );
            assert_eq!(reader.position(), consumed);
            if let Some(bytes) = result {
                assert_eq!(bytes.len(), length);
            }
        }
    }

    #[test]
    fn local_reads_reject_growth_after_a_size_sample() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        for (index, bytes) in [
            b"small\n".to_vec(),
            vec![b'a'; MMAP_MIN_BYTES as usize],
            vec![0xff; MMAP_MIN_BYTES as usize],
            vec![0xff, 0xfe, b'a', 0, b'\n', 0],
        ]
        .into_iter()
        .enumerate()
        {
            let path = temp.path().join(format!("file{index}"));
            std::fs::write(&path, &bytes).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            let limit = file.metadata().unwrap().len();
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"grew beyond the approved size\n")
                .unwrap();
            assert!(
                read_open_text_lossy(file, tgrep_core::encoding::EncodingMode::Auto, Some(limit))
                    .unwrap()
                    .is_none()
            );
            assert!(
                read_text_lossy(&path, tgrep_core::encoding::EncodingMode::Auto, None)
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[test]
    fn mappings_never_extend_past_the_approved_length() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mapped.txt");
        std::fs::write(&path, vec![b'a'; MMAP_MIN_BYTES as usize]).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let approved = file.metadata().unwrap().len();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"not part of the approved mapping")
            .unwrap();
        let (mapped, _) =
            try_map_text(&file, tgrep_core::encoding::EncodingMode::Auto, approved).unwrap();
        assert_eq!(mapped.as_str().len() as u64, approved);
        assert!(!mapped.as_str().contains("not part"));
    }

    #[test]
    fn local_read_limits_count_source_bytes_before_decoding() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        for bytes in [
            Vec::new(),
            b"needle\n".to_vec(),
            vec![0xff, 0xfe, b'a', 0, b'\n', 0],
            vec![0xff; MMAP_MIN_BYTES as usize],
            vec![b'a'; MMAP_MIN_BYTES as usize],
        ] {
            std::fs::write(&path, &bytes).unwrap();
            let mode = tgrep_core::encoding::EncodingMode::Auto;
            assert!(
                read_text_lossy(&path, mode, Some(bytes.len() as u64))
                    .unwrap()
                    .is_some()
            );
            if !bytes.is_empty() {
                assert!(
                    read_text_lossy(&path, mode, Some(bytes.len() as u64 - 1))
                        .unwrap()
                        .is_none()
                );
            }
        }
    }

    #[test]
    fn indexed_reads_keep_the_open_handle_for_mapping_and_decoding() {
        let temp = tempfile::tempdir().unwrap();
        for (index, bytes) in [
            b"original small file\n".to_vec(),
            vec![b'a'; MMAP_MIN_BYTES as usize + 1],
            vec![0xff; MMAP_MIN_BYTES as usize + 1],
            vec![0xff, 0xfe, b'a', 0, b'\n', 0],
        ]
        .into_iter()
        .enumerate()
        {
            let path = temp.path().join(format!("file{index}"));
            std::fs::write(&path, &bytes).unwrap();
            let rooted = tgrep_core::rooted::RootedDir::open(temp.path()).unwrap();
            let file = rooted
                .open_file(path.file_name().unwrap().as_ref())
                .unwrap();
            std::fs::rename(&path, temp.path().join(format!("saved{index}"))).unwrap();
            std::fs::write(&path, b"replacement must not be read\n").unwrap();
            let read = read_open_text_lossy(file, tgrep_core::encoding::EncodingMode::Auto, None)
                .unwrap()
                .unwrap();
            let (expected, _) = tgrep_core::encoding::decode_owned_with_fixups(
                bytes,
                tgrep_core::encoding::EncodingMode::Auto,
            );
            assert_eq!(read.text.as_str(), expected);
        }
    }

    #[test]
    fn only_a_nonempty_full_corpus_gets_a_no_narrowing_note() {
        assert_eq!(index_narrowing_note(2, 2), " (no index narrowing)");
        assert_eq!(index_narrowing_note(1, 2), "");
        assert_eq!(index_narrowing_note(0, 2), "");
        assert_eq!(index_narrowing_note(0, 0), "");
    }

    #[test]
    fn indexed_sort_preserves_entries_and_component_order() {
        let entries = [
            (1, "src.rs"),
            (2, "src/lib.rs"),
            (3, "a.rs"),
            (4, "src/lib.rs"),
        ];
        for (reverse, expected) in [(false, [3, 2, 4, 1]), (true, [1, 4, 2, 3])] {
            let mut sorted = entries;
            SortMode {
                key: SortKey::Path,
                reverse,
            }
            .apply_indexed(
                &mut sorted,
                Path::new("unused-index-root"),
                &IndexScope::Whole,
                |(_, rel)| rel,
            )
            .unwrap();
            assert_eq!(sorted.map(|(id, _)| id), expected);
        }
    }

    #[test]
    fn indexed_sort_uses_scoped_timestamps_and_orders_missing_metadata() {
        let dir = tempfile::TempDir::new().unwrap();
        let subtree = dir.path().join("nested");
        std::fs::create_dir(&subtree).unwrap();
        for (name, seconds) in [("new.txt", 60), ("z-old.txt", 0), ("a-old.txt", 0)] {
            let time =
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + seconds);
            std::fs::File::create(subtree.join(name))
                .unwrap()
                .set_modified(time)
                .unwrap();
        }

        for reverse in [false, true] {
            let mut paths = ["new.txt", "z-old.txt", "missing.txt", "a-old.txt"];
            SortMode {
                key: SortKey::Modified,
                reverse,
            }
            .apply_indexed(
                &mut paths,
                dir.path(),
                &IndexScope::Subtree("nested/".to_string()),
                |path| path,
            )
            .unwrap();
            let mut expected = ["missing.txt", "a-old.txt", "z-old.txt", "new.txt"];
            if reverse {
                expected.reverse();
            }
            assert_eq!(paths, expected);
        }
    }

    #[test]
    fn quiet_file_selection_distinguishes_nonmatching_and_skipped_files() {
        let matcher = SearchMatcher::standard(regex::Regex::new("needle").unwrap(), false);
        for (content, explicit, binary, matched, without_match) in [
            ("needle\n", false, false, true, false),
            ("other\n", false, false, false, true),
            ("", false, false, false, true),
            ("needle\0\n", false, false, false, false),
            ("other\0\n", false, false, false, false),
            ("needle\0\n", true, false, true, false),
            ("other\0\n", true, false, false, true),
            ("needle\0\n", false, true, true, false),
            ("other\0\n", false, true, false, true),
        ] {
            let read = FileRead {
                text: FileText::Owned(content.to_string()),
                fixups: Default::default(),
                first_nul: content.find('\0'),
            };
            for files_without_match in [false, true] {
                let opts = SearchOptions {
                    quiet: true,
                    files_without_match,
                    binary,
                    ..Default::default()
                };
                let mut writer = new_writer(&opts);
                let selected =
                    search_file(&read, &matcher, "file.txt", &opts, &mut writer, explicit).unwrap();
                let expected = if files_without_match {
                    without_match
                } else {
                    matched
                };
                assert_eq!(
                    selected, expected,
                    "content={content:?}, explicit={explicit}, binary={binary}, \
                     files_without_match={files_without_match}"
                );
            }
        }
    }

    // --- fused UTF-8 validation and NUL scan --------------------------------
    //
    // `validate_utf8_and_find_nul` walks the file in chunks so that validating
    // it and finding the NUL that marks it binary share one traversal. The
    // chunking is the delicate part: a multi-byte sequence that straddles a
    // chunk boundary must be carried into the next chunk rather than reported
    // as invalid. These pin that against the whole-buffer answer it replaces.

    /// What the two separate passes used to produce, for differential testing.
    fn reference(bytes: &[u8]) -> Option<Option<usize>> {
        std::str::from_utf8(bytes).ok()?;
        Some(memchr::memchr(0, bytes))
    }

    fn check(bytes: &[u8]) {
        assert_eq!(
            validate_utf8_and_find_nul(bytes),
            reference(bytes),
            "disagreed with a whole-buffer scan on {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn fused_scan_handles_empty_and_ascii() {
        check(b"");
        check(b"hello world");
        check(b"hello\0world");
        check(b"\0");
    }

    #[test]
    fn fused_scan_rejects_invalid_utf8() {
        check(&[0xFF]);
        check(b"ok\xFFbad");
        // A truncated sequence at the very end is a real error, not a chunk
        // boundary to resume across.
        check(&[0xE2, 0x82]);
    }

    #[test]
    fn fused_scan_carries_a_sequence_across_a_chunk_boundary() {
        // Place every multi-byte width so that it straddles the boundary at
        // each possible split, which is exactly the case a per-chunk
        // `from_utf8` would reject.
        const CHUNK: usize = 1 << 20;
        for seq in ["\u{00E9}", "\u{20AC}", "\u{1F600}"] {
            for offset in 1..seq.len() {
                let mut bytes = vec![b'a'; CHUNK - offset];
                bytes.extend_from_slice(seq.as_bytes());
                bytes.extend_from_slice(b"tail");
                check(&bytes);
                // The same, with a NUL on the far side of the boundary, so the
                // reported offset has to survive the carry.
                let mut with_nul = bytes.clone();
                with_nul.push(0);
                with_nul.extend_from_slice(b"more");
                check(&with_nul);
            }
        }
    }

    #[test]
    fn fused_scan_reports_the_first_nul_not_a_later_one() {
        const CHUNK: usize = 1 << 20;
        // One NUL in the first chunk and one in the third: the scan must stop
        // updating after the first, even though later chunks also match.
        let mut bytes = vec![b'a'; CHUNK * 3];
        bytes[7] = 0;
        bytes[CHUNK * 2 + 11] = 0;
        check(&bytes);
        assert_eq!(validate_utf8_and_find_nul(&bytes), Some(Some(7)));
    }

    #[test]
    fn fused_scan_finds_a_nul_exactly_on_a_chunk_boundary() {
        const CHUNK: usize = 1 << 20;
        for at in [CHUNK - 1, CHUNK, CHUNK + 1] {
            let mut bytes = vec![b'a'; CHUNK * 2];
            bytes[at] = 0;
            check(&bytes);
            assert_eq!(validate_utf8_and_find_nul(&bytes), Some(Some(at)));
        }
    }

    #[test]
    fn fused_scan_spans_many_chunks_without_a_nul() {
        const CHUNK: usize = 1 << 20;
        // Multi-byte content throughout, so every chunk ends mid-sequence at
        // some point and the loop has to keep making progress.
        let unit = "héllo wörld €";
        let bytes = unit.repeat((CHUNK * 3) / unit.len()).into_bytes();
        check(&bytes);
        assert_eq!(validate_utf8_and_find_nul(&bytes), Some(None));
    }

    // --- `--stats` counting -------------------------------------------------
    //
    // These pin the reply-row semantics that `--stats` reports over the server.
    // The server counts every row it built, so
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
    fn json_server_requests_include_offsets_without_byte_offset_flag() {
        let mut opts = SearchOptions {
            pattern: "needle".into(),
            ..Default::default()
        };
        assert_eq!(
            server_search_request(&IndexScope::Whole, &opts, false).unwrap()["params"]["positions"],
            false
        );
        opts.json = true;
        let request = server_search_request(&IndexScope::Whole, &opts, false).unwrap();
        assert_eq!(request["params"]["positions"], true);
        assert_eq!(request["params"]["detail"], true);
    }

    #[test]
    fn count_matches_requests_totals_without_changing_legacy_queries() {
        for mut opts in [
            SearchOptions::default(),
            SearchOptions {
                color: ColorMode::Always,
                ..Default::default()
            },
            SearchOptions {
                json: true,
                ..Default::default()
            },
            SearchOptions {
                vimgrep: true,
                ..Default::default()
            },
            SearchOptions {
                column: true,
                ..Default::default()
            },
            SearchOptions {
                only_matching: true,
                ..Default::default()
            },
            SearchOptions {
                replace: Some("replacement".into()),
                ..Default::default()
            },
            SearchOptions {
                count: true,
                ..Default::default()
            },
            SearchOptions {
                files_only: true,
                ..Default::default()
            },
            SearchOptions {
                quiet: true,
                ..Default::default()
            },
        ] {
            for stats in [false, true] {
                opts.stats = stats;
                opts.count_matches = false;
                let request = server_search_request(&IndexScope::Whole, &opts, false).unwrap();
                assert!(request["params"].get("count_matches").is_none());
                assert_eq!(request["params"]["detail"], opts.wants_match_detail());
                assert_eq!(flag_set(&request["params"], "count"), opts.count);
                opts.count_matches = true;
                let request = server_search_request(&IndexScope::Whole, &opts, false).unwrap();
                assert_eq!(request["params"]["count_matches"], true);
                assert!(request["params"].get("count").is_none());
                assert_eq!(request["params"]["stats"], stats);
                assert_eq!(request["params"]["detail"], false);
                assert_eq!(request["params"]["positions"], opts.wants_position_detail());
                assert!(opts.match_options().all_spans);
            }
        }
    }

    #[test]
    fn count_requests_line_totals_without_changing_legacy_details() {
        for stats in [false, true] {
            let opts = SearchOptions {
                count: true,
                stats,
                column: true,
                byte_offset: true,
                only_matching: true,
                context: Some(3),
                replace: Some("replacement".into()),
                ..Default::default()
            };
            let request = server_search_request(&IndexScope::Whole, &opts, false).unwrap();
            assert_eq!(request["params"]["count"], true);
            assert!(request["params"].get("count_matches").is_none());
            assert_eq!(request["params"]["stats"], stats);
            assert_eq!(request["params"]["detail"], true);
            assert_eq!(request["params"]["positions"], true);
            assert_eq!(request["params"]["before_context"], 0);
            assert_eq!(request["params"]["after_context"], 0);
        }
    }

    #[test]
    fn count_matches_uses_totals_not_rows_and_preserves_inversion() {
        let rows = [
            row("match", "b.rs", 1, 1),
            row("match", "b.rs", 2, 1),
            row("context", "context.rs", 3, 0),
            row("match", "a.rs", 1, 3),
            serde_json::json!({"type":"binary","file":"binary.txt","lines":2}),
        ];
        let stats: Vec<FileMatchStats> = serde_json::from_value(serde_json::json!([
            {"file":"a.rs","matches":3,"matched_lines":1},
            {"file":"binary.txt","matches":4,"matched_lines":2},
            {"file":"b.rs","matches":1,"matched_lines":2},
        ]))
        .unwrap();
        assert_eq!(
            original_match_counts(&rows, Some(&stats), false).unwrap(),
            [("b.rs", 1), ("a.rs", 3), ("binary.txt", 4)]
        );
        assert_eq!(
            original_match_counts(&rows, Some(&stats), true).unwrap(),
            [("b.rs", 2), ("a.rs", 1), ("binary.txt", 2)]
        );
        assert!(
            original_match_counts(&[], Some(&[]), false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn count_matches_rejects_missing_partial_and_duplicate_totals() {
        let rows = [row("match", "a.rs", 1, 1), row("match", "b.rs", 1, 1)];
        assert!(original_match_counts(&rows, None, false).is_err());
        for metadata in [
            serde_json::json!([]),
            serde_json::json!([{"file":"a.rs","matches":1,"matched_lines":1}]),
            serde_json::json!([
                {"file":"a.rs","matches":1,"matched_lines":1},
                {"file":"a.rs","matches":1,"matched_lines":1},
                {"file":"b.rs","matches":1,"matched_lines":1},
            ]),
            serde_json::json!([
                {"file":"a.rs","matches":1,"matched_lines":1},
                {"file":"b.rs","matches":1,"matched_lines":1},
                {"file":"extra.rs","matches":1,"matched_lines":1},
            ]),
        ] {
            let stats: Vec<FileMatchStats> = serde_json::from_value(metadata).unwrap();
            assert!(original_match_counts(&rows, Some(&stats), false).is_err());
        }
    }

    #[test]
    fn json_search_stats_require_complete_receipts_not_output_totals() {
        let rows = [row("match", "a.rs", 1, 1)];
        for metadata in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!([{"file":"a.rs"}]),
            serde_json::json!([{"file":"a.rs","bytes_searched":-1}]),
            serde_json::json!([
                {"file":"a.rs","bytes_searched":8},
                {"file":"a.rs","bytes_searched":8}
            ]),
            serde_json::json!([
                {"file":"a.rs","bytes_searched":u64::MAX},
                {"file":"b.rs","bytes_searched":1}
            ]),
        ] {
            assert!(
                server_search_stats(&serde_json::json!({"search_stats":metadata}), &rows).is_err()
            );
        }
        assert!(server_search_stats(&serde_json::json!({}), &[]).is_err());
        assert!(
            server_search_stats(&serde_json::json!({"search_stats":[]}), &[])
                .unwrap()
                .is_empty()
        );
        let stats = server_search_stats(
            &serde_json::json!({"search_stats":[
                {"file":"a.rs","bytes_searched":8,"binary_offset":null},
                {"file":"no-output.rs","bytes_searched":30,"binary_offset":null}
            ]}),
            &rows,
        )
        .unwrap();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats.iter().map(|s| s.bytes_searched).sum::<u64>(), 38);
        for json in [false, true] {
            for stats in [false, true] {
                let opts = SearchOptions {
                    json,
                    stats,
                    ..Default::default()
                };
                let request = server_search_request(&IndexScope::Whole, &opts, false).unwrap();
                assert_eq!(request["params"].get("search_stats").is_some(), json);
                assert_eq!(request["params"]["stats"], stats);
            }
        }
    }

    #[test]
    fn server_response_requires_explicit_complete_hidden_coverage() {
        for response in [
            serde_json::json!({}),
            serde_json::json!({"hidden_complete": false}),
            serde_json::json!({"hidden_complete": null}),
        ] {
            assert!(require_server_coverage(&response).is_err());
        }
        assert!(require_server_coverage(&serde_json::json!({"hidden_complete": true})).is_ok());
    }

    #[test]
    fn replace_rendering_is_requested_only_where_it_changes_output() {
        let replace = SearchOptions {
            pattern: "a\\nb".into(),
            replace: Some("X".into()),
            ..Default::default()
        };
        let with = |f: fn(&mut SearchOptions)| {
            let mut opts = replace.clone();
            f(&mut opts);
            opts
        };
        let params = |opts: &SearchOptions| {
            server_search_request(&IndexScope::Whole, opts, false).unwrap()["params"].clone()
        };
        // Older servers refuse or ignore unknown fields, so leave them out of
        // every request whose output they would not change.
        for opts in [
            SearchOptions::default(),
            replace.clone(),
            with(|o| o.invert_match = true),
            with(|o| {
                o.multiline = true;
                o.invert_match = true;
            }),
            with(|o| {
                o.multiline = true;
                o.count = true;
            }),
            with(|o| {
                o.multiline = true;
                o.files_only = true;
            }),
            with(|o| {
                o.multiline = true;
                o.quiet = true;
            }),
            // A pattern that cannot match a line terminator prints line by
            // line, with or without blocks.
            with(|o| {
                o.multiline = true;
                o.pattern = "needle".into();
            }),
            with(|o| {
                o.multiline = true;
                o.pattern = "a.b".into();
            }),
            with(|o| {
                o.multiline = true;
                o.fixed_string = true;
            }),
            // Inverted searches print no submatches, and `-q` prints none.
            with(|o| {
                o.json = true;
                o.invert_match = true;
            }),
            with(|o| {
                o.json = true;
                o.quiet = true;
            }),
            // `-m0` prints no lines at all.
            with(|o| {
                o.multiline = true;
                o.max_count = Some(0);
            }),
            with(|o| {
                o.json = true;
                o.max_count = Some(0);
            }),
        ] {
            let request = server_search_request(&IndexScope::Whole, &opts, false).unwrap();
            let params = &request["params"];
            assert!(params.get("multiline_replace").is_none(), "{params}");
            assert!(params.get("report_replacements").is_none(), "{params}");
            assert!(require_replace_support(&serde_json::json!({}), &request).is_ok());
        }

        for opts in [
            with(|o| o.multiline = true),
            with(|o| {
                o.multiline = true;
                o.max_count = Some(1);
            }),
            with(|o| {
                o.multiline = true;
                o.only_matching = true;
            }),
            with(|o| {
                o.multiline = true;
                o.vimgrep = true;
            }),
            with(|o| {
                o.multiline = true;
                o.pattern = r"a\sb".into();
            }),
            with(|o| {
                o.multiline = true;
                o.multiline_dotall = true;
                o.pattern = "a.b".into();
            }),
            // Line anchors count, as they do in ripgrep, and the backtracking
            // engine is never analysed.
            with(|o| {
                o.multiline = true;
                o.pattern = "^a".into();
            }),
            with(|o| {
                o.multiline = true;
                o.pattern = "a(?=b)".into();
            }),
            with(|o| {
                o.multiline = true;
                o.pattern = "(".into();
            }),
        ] {
            let params = params(&opts);
            assert_eq!(params["multiline_replace"], true, "{params}");
            assert!(params.get("report_replacements").is_none());
            assert!(opts.match_options().replace_blocks);
        }

        // JSON keeps the source line and adds `replacement`, which a server
        // predating it would not, whatever the pattern can match.
        for opts in [
            with(|o| o.json = true),
            with(|o| {
                o.json = true;
                o.max_count = Some(1);
            }),
            with(|o| {
                o.json = true;
                o.multiline = true;
            }),
            with(|o| {
                o.json = true;
                o.pattern = "needle".into();
            }),
        ] {
            let params = params(&opts);
            assert!(params.get("multiline_replace").is_none());
            assert_eq!(params["report_replacements"], true);
            assert!(opts.match_options().report_replacements);
        }
    }

    #[test]
    fn replace_rendering_requires_a_server_that_acknowledges_it() {
        let request =
            |opts: SearchOptions| server_search_request(&IndexScope::Whole, &opts, false).unwrap();
        let blocks = request(SearchOptions {
            pattern: "a\\nb".into(),
            multiline: true,
            replace: Some("X".into()),
            ..Default::default()
        });
        let json = request(SearchOptions {
            json: true,
            replace: Some("X".into()),
            ..Default::default()
        });
        // A server predating the feature ignores the request and renders the
        // old way, so its reply must be refused rather than printed.
        for result in [
            serde_json::json!({}),
            serde_json::json!({"multiline_replace": false, "report_replacements": false}),
        ] {
            assert!(require_replace_support(&result, &blocks).is_err());
            assert!(require_replace_support(&result, &json).is_err());
        }
        assert!(
            require_replace_support(&serde_json::json!({"multiline_replace": true}), &json)
                .is_err()
        );
        assert!(
            require_replace_support(&serde_json::json!({"report_replacements": true}), &blocks)
                .is_err()
        );
        let acked = serde_json::json!({"multiline_replace": true, "report_replacements": true});
        assert!(require_replace_support(&acked, &blocks).is_ok());
        assert!(require_replace_support(&acked, &json).is_ok());
    }

    #[test]
    fn file_separators_follow_ripgreps_thread_count() {
        let eager = |sort: Option<SortMode>, threads: Option<usize>| {
            SearchOptions {
                sort,
                threads,
                ..Default::default()
            }
            .make_output_config()
            .eager_file_separator
        };
        let by_path = Some(SortMode {
            key: SortKey::Path,
            reverse: true,
        });
        assert!(eager(by_path, None));
        assert!(eager(by_path, Some(4)));
        assert!(eager(None, Some(1)));
        assert!(!eager(None, Some(2)));
        let one_cpu = std::thread::available_parallelism().map_or(1, |n| n.get()) == 1;
        assert_eq!(eager(None, None), one_cpu);
        assert_eq!(eager(None, Some(0)), one_cpu);
    }

    fn filter_options(type_ops: &[(&str, &str)], glob: &[&str], iglob: &[&str]) -> SearchOptions {
        SearchOptions {
            pattern: "needle".into(),
            type_ops: type_ops
                .iter()
                .map(|&(kind, value)| match kind {
                    "add" => filetypes::TypeOp::Add(value.into()),
                    "clear" => filetypes::TypeOp::Clear(value.into()),
                    "select" => filetypes::TypeOp::Select(value.into()),
                    "negate" => filetypes::TypeOp::Negate(value.into()),
                    _ => unreachable!("{kind}"),
                })
                .collect(),
            glob: glob.iter().map(|g| g.to_string()).collect(),
            iglob: iglob.iter().map(|g| g.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn ordered_filters_are_requested_only_where_they_change_output() {
        let params = |opts: &SearchOptions| {
            server_search_request(&IndexScope::Whole, opts, false).unwrap()["params"].clone()
        };
        // A server answers a request without the field in the legacy order,
        // which these cannot tell apart from ripgrep's: a glob that only
        // excludes, or one with no type selection for it to override.
        for opts in [
            filter_options(&[], &[], &[]),
            filter_options(&[("select", "rust")], &[], &[]),
            filter_options(&[("negate", "rust"), ("negate", "c")], &[], &[]),
            filter_options(&[("select", "rust"), ("negate", "rust")], &[], &[]),
            filter_options(&[("add", "foo:*.foo"), ("select", "foo")], &[], &[]),
            filter_options(&[], &["*.txt"], &[]),
            filter_options(&[("select", "rust")], &["!*.txt"], &[]),
            filter_options(&[("add", "foo:*.txt")], &["*.txt"], &[]),
        ] {
            let request = server_search_request(&IndexScope::Whole, &opts, false).unwrap();
            let params = &request["params"];
            assert!(params.get("ordered_filters").is_none(), "{params}");
            assert!(params.get("type_ops").is_none(), "{params}");
            assert!(require_ordered_filters(&serde_json::json!({}), &request).is_ok());
        }

        // Definitions that no `-t`/`-T` consumes cannot change the result in
        // any order, so a server is not sent them at all.
        for opts in [
            filter_options(&[("add", "rust:*.txt"), ("clear", "rust")], &[], &[]),
            filter_options(
                &[("add", "foo:include:rust"), ("clear", "rust")],
                &["*.txt"],
                &[],
            ),
            filter_options(&[("add", "foo:*.txt")], &["*.txt"], &[]),
        ] {
            let params = params(&opts);
            assert!(params.get("ordered_filters").is_none(), "{params}");
            assert!(params.get("type_ops").is_none(), "{params}");
            for name in ["types", "types_not", "type_add", "type_clear"] {
                assert_eq!(params[name], serde_json::json!([]), "{name}: {params}");
            }
        }
        // A `-t` or a `-T` keeps the definitions it may consume.
        for (kind, name) in [("select", "types"), ("negate", "types_not")] {
            let params = params(&filter_options(
                &[("clear", "rust"), ("add", "foo:*.foo"), (kind, "foo")],
                &[],
                &[],
            ));
            assert_eq!(params[name], serde_json::json!(["foo"]), "{params}");
            assert_eq!(
                params["type_add"],
                serde_json::json!(["foo:*.foo"]),
                "{params}"
            );
            assert_eq!(
                params["type_clear"],
                serde_json::json!(["rust"]),
                "{params}"
            );
        }

        // A positive glob overrides a selection, which a legacy server would
        // let veto the files that glob includes.
        for opts in [
            filter_options(&[("select", "rust")], &["*.txt"], &[]),
            filter_options(&[("negate", "txt")], &["*.txt"], &[]),
            filter_options(&[("select", "rust")], &[], &["*.TXT"]),
            filter_options(&[("select", "rust")], &["!*.md", "*.txt"], &[]),
        ] {
            let params = params(&opts);
            assert_eq!(params["ordered_filters"], true, "{params}");
            assert!(params.get("type_ops").is_none(), "{params}");
        }

        // Type flags whose order matters also need a server that honours it.
        for opts in [
            filter_options(&[("negate", "rust"), ("select", "rust")], &[], &[]),
            filter_options(
                &[
                    ("add", "foo:*.txt"),
                    ("clear", "foo"),
                    ("add", "foo:*.rs"),
                    ("select", "foo"),
                ],
                &[],
                &[],
            ),
        ] {
            let params = params(&opts);
            assert_eq!(params["ordered_filters"], true, "{params}");
            assert_eq!(
                params["type_ops"],
                type_ops_to_json(&opts.type_ops),
                "{params}"
            );
        }
    }

    #[test]
    fn ordered_filters_require_a_server_that_acknowledges_them() {
        let ordered = server_search_request(
            &IndexScope::Whole,
            &filter_options(&[("select", "rust")], &["*.txt"], &[]),
            false,
        )
        .unwrap();
        // A server predating ordered filters ignores the request and lets
        // `-t rust` veto `a.txt`, so its reply must be refused.
        for result in [
            serde_json::json!({}),
            serde_json::json!({"ordered_filters": false}),
            serde_json::json!({"ordered_filters": null}),
        ] {
            assert!(require_ordered_filters(&result, &ordered).is_err());
        }
        assert!(
            require_ordered_filters(&serde_json::json!({"ordered_filters": true}), &ordered)
                .is_ok()
        );
    }

    #[test]
    fn glob_precedence_lets_a_positive_glob_override_types() {
        let glob_filter = crate::glob_filter::GlobFilter::new(
            &["*.txt".to_string(), "!skip.txt".to_string()],
            &[],
            false,
        )
        .unwrap();
        let type_filter =
            filetypes::build_type_filter(&[filetypes::TypeOp::Select("rust".into())]).unwrap();
        // ripgrep's order: the glob decides every file it matches.
        assert!(passes_filters("a.txt", &glob_filter, &type_filter, true));
        assert!(!passes_filters(
            "skip.txt",
            &glob_filter,
            &type_filter,
            true
        ));
        assert!(!passes_filters("a.rs", &glob_filter, &type_filter, true));
        // The legacy order a server keeps for older clients: a file must pass
        // both, so `-t rust` vetoes `a.txt`. An exclusion still wins.
        assert!(!passes_filters("a.txt", &glob_filter, &type_filter, false));
        assert!(!passes_filters(
            "skip.txt",
            &glob_filter,
            &type_filter,
            false
        ));
        assert!(!passes_filters("a.rs", &glob_filter, &type_filter, false));

        let no_globs = crate::glob_filter::GlobFilter::new(&[], &[], false).unwrap();
        for glob_precedence in [true, false] {
            assert!(passes_filters(
                "a.rs",
                &no_globs,
                &type_filter,
                glob_precedence
            ));
            assert!(!passes_filters(
                "a.txt",
                &no_globs,
                &type_filter,
                glob_precedence
            ));
        }
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
            color: ColorMode::Never,
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
