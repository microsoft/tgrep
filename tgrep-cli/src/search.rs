/// `tgrep search` — search using the trigram index, with server delegation.
///
/// If a running server is detected (via serve.json), the search is delegated
/// over TCP. Otherwise, the on-disk index is loaded directly.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use tgrep_core::builder;
use tgrep_core::filetypes;
use tgrep_core::query::{self, QueryPlan};
use tgrep_core::reader::IndexReader;
use tgrep_core::walker;

use crate::matching::SearchMatcher;
use crate::output::{ColorMode, ContextLine, Match, OutputConfig, OutputWriter};
use crate::serve::ServerInfo;

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
    pub file_type: Option<String>,
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
    pub max_filesize: Option<u64>,
    pub follow: bool,
    pub no_messages: bool,
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
            search_binary: self.text,
            follow_links: self.follow,
            max_file_size: self.max_filesize,
            ..Default::default()
        }
    }

    fn glob_filter(&self) -> Result<crate::glob_filter::GlobFilter> {
        crate::glob_filter::GlobFilter::new(&self.glob, &self.iglob, self.glob_case_insensitive)
    }

    /// The matching-relevant subset, shared with the server path.
    fn match_options(&self) -> crate::matching::MatchOptions {
        crate::matching::MatchOptions {
            invert_match: self.invert_match,
            multiline: self.multiline,
            only_matching: self.only_matching,
            before_context: self.before_ctx(),
            after_context: self.after_ctx(),
            // `-q` and `--files-without-match` only need to know whether the
            // file matched at all, so stop at the first hit.
            max_count: if self.quiet || self.files_without_match {
                Some(1)
            } else {
                self.max_count
            },
        }
    }

    fn matcher(&self, ci: bool) -> Result<SearchMatcher> {
        crate::matching::build_search_matcher(
            &self.all_patterns()?,
            ci,
            self.fixed_string,
            self.word_boundary,
            self.multiline,
            self.dotall(),
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
        OutputConfig::from_flags(
            self.json,
            self.files_only,
            self.count,
            self.vimgrep,
            self.heading,
            self.color,
            self.null,
            self.trim,
            self.no_filename,
            self.no_line_number,
            self.after_ctx() > 0 || self.before_ctx() > 0,
        )
    }

    /// Collect all patterns (primary + -e extras + -f file patterns).
    fn all_patterns(&self) -> Result<Vec<String>> {
        let mut patterns = vec![self.pattern.clone()];
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

/// List files that would be searched (--files mode).
pub fn list_files(root: &Path, opts: &SearchOptions) -> Result<()> {
    let root = match std::fs::canonicalize(root) {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let glob_filter = opts.glob_filter()?;

    if root.is_file() {
        let rel_path = explicit_file_display_path(&root);

        if passes_filters(&rel_path, &glob_filter, &opts.file_type) {
            let mut writer = OutputWriter::new(opts.make_output_config());
            writer.write_file(&rel_path)?;
            writer.flush()?;
        }
        return Ok(());
    }

    let walk = walker::walk_dir(&root, &opts.walk_options());
    let mut writer = OutputWriter::new(opts.make_output_config());

    for path in &walk.files {
        let rel_path = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        if let Some(ref type_name) = opts.file_type
            && !filetypes::matches_type(&rel_path, type_name)
        {
            continue;
        }
        if !glob_filter.matches(&rel_path) {
            continue;
        }
        writer.write_file(&rel_path)?;
    }
    writer.flush()?;
    Ok(())
}

pub fn run(root: &Path, index_path: Option<&Path>, opts: &SearchOptions) -> Result<bool> {
    let root = match std::fs::canonicalize(root) {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let index_dir = index_path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| builder::default_index_dir(&root));

    let ci = opts.effective_case_insensitive();

    // Try to delegate to a running server (skip for files_without_match
    // since the server only returns matching files).
    if !opts.no_index
        && !opts.files_without_match
        && let Ok(info) = ServerInfo::load(&index_dir)
    {
        if let Ok(had_matches) = search_via_server(&info, opts, ci) {
            return Ok(had_matches);
        }
        eprintln!("Server unreachable, falling back to local index");
    }

    // No server — use on-disk index directly (or brute force)
    if opts.no_index {
        return brute_force_search(&root, opts, ci);
    }
    if !index_dir.join("lookup.bin").exists() {
        warn_missing_index(&index_dir, index_path.is_some(), opts.quiet);
        return brute_force_search(&root, opts, ci);
    }

    search_local_index(&root, &index_dir, opts, ci)
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

fn search_via_server(info: &ServerInfo, opts: &SearchOptions, ci: bool) -> Result<bool> {
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
    let wants_context = !(opts.count || opts.files_only || opts.quiet);

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
            "file_type": opts.file_type,
            "invert_match": opts.invert_match,
            "only_matching": opts.only_matching,
            "after_context": if wants_context { opts.after_ctx() } else { 0 },
            "before_context": if wants_context { opts.before_ctx() } else { 0 },
            "multiline": opts.multiline,
            "multiline_dotall": opts.dotall(),
            "text": opts.text,
            "max_filesize": opts.max_filesize,
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

    let mut writer = OutputWriter::new(opts.make_output_config());
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
    } else if opts.count {
        // `-c` counts matching *lines*, so collapse rows that share a line
        // (`-o` emits one per match) and ignore context rows. This is what the
        // local path reports via `FileMatches::matched_lines`.
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
            let line = m.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as usize;
            let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
            let mtype = m.get("type").and_then(|t| t.as_str()).unwrap_or("match");
            let offset = m.get("offset").and_then(|o| o.as_u64()).unwrap_or(0) as usize;
            if mtype == "context" {
                writer.write_context_separator(file, line)?;
                writer.write_context_line(&ContextLine {
                    file: file.to_string(),
                    line_number: line,
                    content: content.to_string(),
                    absolute_offset: offset,
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
                    column: m.get("column").and_then(|c| c.as_u64()).map(|c| c as usize),
                    spans,
                    absolute_offset: offset,
                })?;
            }
        }
    }

    if opts.stats
        && let Some(elapsed) = result.get("elapsed_ms").and_then(|e| e.as_f64())
    {
        let num = result
            .get("num_matches")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        eprintln!("{num} matches in {elapsed:.1}ms (via server)");
    }

    writer.flush()?;
    Ok(had_matches)
}

fn search_local_index(
    root: &Path,
    index_dir: &Path,
    opts: &SearchOptions,
    ci: bool,
) -> Result<bool> {
    let start = Instant::now();
    let reader = IndexReader::open(index_dir)?;
    let glob_filter = opts.glob_filter()?;

    let matcher = opts.matcher(ci)?;

    // Narrow candidates using every pattern, not just the primary one.
    let plan = if !matcher.is_standard() {
        QueryPlan::MatchAll
    } else {
        query::build_multi_pattern_plan(&opts.all_patterns()?, opts.fixed_string, ci)
            .map_err(|e| anyhow::anyhow!("{e}"))?
    };

    let is_match_all = plan.is_match_all();

    // `-v` inverts at the *line* level: a file matches when some line does not.
    // The trigram plan selects files that contain the pattern, which is exactly
    // the wrong filter, so it has to be bypassed. Same for `--files-without-match`.
    let candidates = if is_match_all || opts.files_without_match || opts.invert_match {
        reader.all_file_ids()
    } else {
        query::execute_plan_with_masks(&plan, &|tri| reader.lookup_trigram_with_masks(tri))
    };

    if opts.stats {
        eprintln!(
            "Query plan: {} (candidates: {}/{})",
            plan_summary(&plan),
            candidates.len(),
            reader.num_files()
        );
    }

    let mut writer = OutputWriter::new(opts.make_output_config());
    let mut had_matches = false;

    for &fid in &candidates {
        let rel_path = match reader.file_path(fid) {
            Some(p) => p.to_string(),
            None => continue,
        };

        if !passes_filters(&rel_path, &glob_filter, &opts.file_type) {
            continue;
        }

        let full_path = root.join(rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if exceeds_max_filesize(&full_path, opts) {
            continue;
        }
        let content = match read_text_lossy(&full_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let matched = search_file_content(&content, &matcher, &rel_path, opts, &mut writer)?;
        if matched {
            if !opts.files_without_match {
                had_matches = true;
                if opts.quiet {
                    break;
                }
            }
        } else if opts.files_without_match {
            if !opts.quiet {
                writer.write_file(&rel_path)?;
            }
            had_matches = true;
            if opts.quiet {
                break;
            }
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

fn brute_force_search(root: &Path, opts: &SearchOptions, ci: bool) -> Result<bool> {
    let start = Instant::now();
    let glob_filter = opts.glob_filter()?;

    let matcher = opts.matcher(ci)?;

    let mut writer = OutputWriter::new(opts.make_output_config());
    let mut had_matches = false;

    if root.is_file() {
        let rel_path = explicit_file_display_path(root);
        if passes_filters(&rel_path, &glob_filter, &opts.file_type)
            && !exceeds_max_filesize(root, opts)
        {
            let content = read_text_lossy(root)?;
            let matched = search_file_content(&content, &matcher, &rel_path, opts, &mut writer)?;
            if matched {
                if !opts.files_without_match {
                    had_matches = true;
                }
            } else if opts.files_without_match {
                if !opts.quiet {
                    writer.write_file(&rel_path)?;
                }
                had_matches = true;
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

    let walk = walker::walk_dir(root, &opts.walk_options());

    for path in &walk.files {
        let rel_path = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        if !passes_filters(&rel_path, &glob_filter, &opts.file_type) {
            continue;
        }

        let content = match read_text_lossy(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let matched = search_file_content(&content, &matcher, &rel_path, opts, &mut writer)?;
        if matched {
            if !opts.files_without_match {
                had_matches = true;
                if opts.quiet {
                    break;
                }
            }
        } else if opts.files_without_match {
            if !opts.quiet {
                writer.write_file(&rel_path)?;
            }
            had_matches = true;
            if opts.quiet {
                break;
            }
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

/// Read a file as text, replacing invalid UTF-8 rather than failing.
///
/// ripgrep searches files that are not valid UTF-8; refusing them makes
/// UTF-16, Latin-1 and mixed-encoding sources silently invisible.
fn read_text_lossy(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    })
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

/// Search a single file's content and write output.
/// Returns true if any matches were found.
fn search_file_content(
    content: &str,
    matcher: &SearchMatcher,
    rel_path: &str,
    opts: &SearchOptions,
    writer: &mut OutputWriter,
) -> Result<bool> {
    writer.note_bytes_searched(content.len() as u64);

    let match_opts = opts.match_options();
    let found = crate::matching::FileMatches::find(content, matcher, &match_opts)?;

    if found.is_empty() {
        return Ok(false);
    }

    // For quiet/files_without_match, we only need the boolean result.
    if opts.quiet || opts.files_without_match {
        return Ok(true);
    }

    if opts.files_only {
        writer.write_file(rel_path)?;
        return Ok(true);
    }

    if opts.count {
        writer.write_count(rel_path, found.matched_lines())?;
        return Ok(true);
    }

    // Never dump raw binary to a terminal; ripgrep reports a note instead.
    if !opts.text
        && let Some(off) = content.as_bytes().iter().position(|&b| b == 0)
    {
        writer.write_binary_note(rel_path, off)?;
        return Ok(true);
    }

    found.for_each(&match_opts, |emit| match emit {
        crate::matching::Emit::Match {
            line_number,
            content,
            column,
            spans,
            absolute_offset,
        } => {
            writer.write_context_separator(rel_path, line_number)?;
            writer.write_match(&Match {
                file: rel_path.to_string(),
                line_number,
                content: content.to_string(),
                column,
                spans,
                absolute_offset,
            })
        }
        crate::matching::Emit::Context {
            line_number,
            content,
            absolute_offset,
        } => {
            writer.write_context_separator(rel_path, line_number)?;
            writer.write_context_line(&ContextLine {
                file: rel_path.to_string(),
                line_number,
                content: content.to_string(),
                absolute_offset,
            })
        }
    })?;

    Ok(true)
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
    file_type: &Option<String>,
) -> bool {
    if let Some(type_name) = file_type
        && !filetypes::matches_type(rel_path, type_name)
    {
        return false;
    }
    glob_filter.matches(rel_path)
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
