/// `tgrep search` — search using the trigram index, with server delegation.
///
/// If a running server is detected (via serve.json), the search is delegated
/// over TCP. Otherwise, the on-disk index is loaded directly.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use regex::RegexBuilder;
use tgrep_core::builder;
use tgrep_core::filetypes;
use tgrep_core::query::{self, QueryPlan};
use tgrep_core::reader::IndexReader;
use tgrep_core::walker;

use crate::output::{ColorMode, ContextLine, Match, OutputConfig, OutputWriter};
use crate::serve::ServerInfo;

pub struct SearchOptions {
    pub pattern: String,
    pub extra_patterns: Vec<String>,
    pub pattern_file: Option<String>,
    pub case_insensitive: bool,
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
    pub no_ignore: bool,
    pub hidden: bool,
    pub quiet: bool,
    pub no_filename: bool,
    pub no_line_number: bool,
}

impl SearchOptions {
    /// Resolve smart-case: case-insensitive if pattern is all lowercase.
    pub fn effective_case_insensitive(&self) -> bool {
        if self.case_insensitive {
            return true;
        }
        if self.smart_case {
            return !self.pattern.chars().any(|c| c.is_uppercase());
        }
        false
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
    let root = std::fs::canonicalize(root)?;
    let walk = walker::walk_dir(
        &root,
        &walker::WalkOptions {
            include_hidden: opts.hidden,
            no_ignore: opts.no_ignore,
            ..Default::default()
        },
    );
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
        if !passes_glob_filters(&opts.glob, &rel_path) {
            continue;
        }
        writer.write_file(&rel_path)?;
    }
    writer.flush()?;
    Ok(())
}

pub fn run(root: &Path, index_path: Option<&Path>, opts: &SearchOptions) -> Result<bool> {
    let root = std::fs::canonicalize(root)?;
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
    if opts.no_index || !index_dir.join("lookup.bin").exists() {
        return brute_force_search(&root, opts, ci);
    }

    search_local_index(&root, &index_dir, opts, ci)
}

fn search_via_server(info: &ServerInfo, opts: &SearchOptions, ci: bool) -> Result<bool> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", info.port))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(300)))?;

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "search",
        "params": {
            "pattern": opts.pattern,
            "extra_patterns": opts.extra_patterns,
            "case_insensitive": ci,
            "fixed_string": opts.fixed_string,
            "files_only": opts.files_only,
            "word_boundary": opts.word_boundary,
            "max_count": opts.max_count,
            "glob": opts.glob,  // sent as JSON array
            "file_type": opts.file_type,
            "invert_match": opts.invert_match,
            "only_matching": opts.only_matching,
            "after_context": opts.after_ctx(),
            "before_context": opts.before_ctx(),
            "multiline": opts.multiline,
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
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for m in matches {
            if let Some(file) = m.get("file").and_then(|f| f.as_str()) {
                *counts.entry(file.to_string()).or_default() += 1;
            }
        }
        for (file, count) in &counts {
            writer.write_count(file, *count)?;
        }
    } else {
        for m in matches {
            let file = m.get("file").and_then(|f| f.as_str()).unwrap_or("");
            let line = m.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as usize;
            let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
            let mtype = m.get("type").and_then(|t| t.as_str()).unwrap_or("match");
            if mtype == "context" {
                writer.write_context_separator(file, line)?;
                writer.write_context_line(&ContextLine {
                    file: file.to_string(),
                    line_number: line,
                    content: content.to_string(),
                })?;
            } else {
                writer.write_context_separator(file, line)?;
                writer.write_match(&Match {
                    file: file.to_string(),
                    line_number: line,
                    content: content.to_string(),
                    column: m.get("column").and_then(|c| c.as_u64()).map(|c| c as usize),
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

    let all_patterns = opts.all_patterns()?;
    let re = build_combined_regex(
        &all_patterns,
        ci,
        opts.fixed_string,
        opts.word_boundary,
        opts.multiline,
    )?;

    // Build query plan from primary pattern
    let plan = if opts.fixed_string {
        query::build_literal_plan(&opts.pattern, ci)
    } else {
        query::build_query_plan(&opts.pattern, ci).map_err(|e| anyhow::anyhow!("{e}"))?
    };

    let is_match_all = plan.is_match_all();

    let candidates = if is_match_all || opts.files_without_match {
        reader.all_file_ids()
    } else {
        query::execute_plan(&plan, &|tri| reader.lookup_trigram(tri))
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

        if !passes_filters(&rel_path, &opts.glob, &opts.file_type) {
            continue;
        }

        let full_path = root.join(rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let matched = search_file_content(&content, &re, &rel_path, opts, &mut writer)?;
        if matched {
            if !opts.files_without_match {
                had_matches = true;
            }
            if opts.quiet {
                break;
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

    // If root is a file, walk its parent and filter to just that file
    let (walk_root, single_file) = if root.is_file() {
        let parent = root.parent().unwrap_or(root);
        (parent.to_path_buf(), Some(root.to_path_buf()))
    } else {
        (root.to_path_buf(), None)
    };

    let walk = walker::walk_dir(
        &walk_root,
        &walker::WalkOptions {
            include_hidden: opts.hidden,
            no_ignore: opts.no_ignore,
            ..Default::default()
        },
    );

    let all_patterns = opts.all_patterns()?;
    let re = build_combined_regex(
        &all_patterns,
        ci,
        opts.fixed_string,
        opts.word_boundary,
        opts.multiline,
    )?;

    let mut writer = OutputWriter::new(opts.make_output_config());
    let mut had_matches = false;

    for path in &walk.files {
        // If we're searching a single file, skip everything else
        if let Some(ref sf) = single_file
            && path != sf
        {
            continue;
        }

        let rel_path = path
            .strip_prefix(&walk_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        if !passes_filters(&rel_path, &opts.glob, &opts.file_type) {
            continue;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let matched = search_file_content(&content, &re, &rel_path, opts, &mut writer)?;
        if matched {
            if !opts.files_without_match {
                had_matches = true;
            }
            if opts.quiet {
                break;
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

/// Search a single file's content and write output.
/// Returns true if any matches were found.
fn search_file_content(
    content: &str,
    re: &regex::Regex,
    rel_path: &str,
    opts: &SearchOptions,
    writer: &mut OutputWriter,
) -> Result<bool> {
    let lines: Vec<&str> = content.lines().collect();
    let before = opts.before_ctx();
    let after = opts.after_ctx();
    let has_context = before > 0 || after > 0;

    // Find matching line indices
    let mut match_indices: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let is_match = re.is_match(line);
        let include = if opts.invert_match {
            !is_match
        } else {
            is_match
        };
        if include {
            match_indices.push(i);
            if opts.quiet || opts.files_without_match {
                break;
            }
            if let Some(max) = opts.max_count
                && match_indices.len() >= max
            {
                break;
            }
        }
    }

    if match_indices.is_empty() {
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
        writer.write_count(rel_path, match_indices.len())?;
        return Ok(true);
    }

    // Build set of lines to output (matches + context)
    if has_context {
        let mut printed: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        let mut is_match_line: std::collections::HashSet<usize> = std::collections::HashSet::new();

        for &mi in &match_indices {
            is_match_line.insert(mi);
            let ctx_start = mi.saturating_sub(before);
            let ctx_end = (mi + after + 1).min(lines.len());
            for j in ctx_start..ctx_end {
                printed.insert(j);
            }
        }

        let mut prev_line: Option<usize> = None;
        for &li in &printed {
            // Insert separator for gaps
            if let Some(prev) = prev_line
                && li > prev + 1
            {
                writer.write_context_separator(rel_path, li + 1)?;
            }

            if is_match_line.contains(&li) {
                let content = match_content(lines[li], re, opts.only_matching);
                let column = re.find(lines[li]).map(|m| m.start() + 1);
                writer.write_match(&Match {
                    file: rel_path.to_string(),
                    line_number: li + 1,
                    content,
                    column,
                })?;
            } else {
                writer.write_context_line(&ContextLine {
                    file: rel_path.to_string(),
                    line_number: li + 1,
                    content: lines[li].to_string(),
                })?;
            }
            prev_line = Some(li);
        }
    } else {
        for &mi in &match_indices {
            let content = match_content(lines[mi], re, opts.only_matching);
            let column = re.find(lines[mi]).map(|m| m.start() + 1);
            writer.write_match(&Match {
                file: rel_path.to_string(),
                line_number: mi + 1,
                content,
                column,
            })?;
        }
    }

    Ok(true)
}

/// Extract match content — full line or only the matched portion.
fn match_content(line: &str, re: &regex::Regex, only_matching: bool) -> String {
    if only_matching {
        re.find_iter(line)
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        line.to_string()
    }
}

fn build_combined_regex(
    patterns: &[String],
    case_insensitive: bool,
    fixed_string: bool,
    word_boundary: bool,
    multiline: bool,
) -> Result<regex::Regex> {
    let combined = if patterns.len() == 1 {
        let mut p = if fixed_string {
            regex::escape(&patterns[0])
        } else {
            patterns[0].clone()
        };
        if word_boundary {
            p = format!(r"\b(?:{p})\b");
        }
        p
    } else {
        let parts: Vec<String> = patterns
            .iter()
            .map(|p| {
                let mut p = if fixed_string {
                    regex::escape(p)
                } else {
                    p.clone()
                };
                if word_boundary {
                    p = format!(r"\b(?:{p})\b");
                }
                p
            })
            .collect();
        format!("(?:{})", parts.join("|"))
    };

    RegexBuilder::new(&combined)
        .case_insensitive(case_insensitive)
        .multi_line(multiline)
        .dot_matches_new_line(multiline)
        .build()
        .map_err(|e| anyhow::anyhow!("regex error: {e}"))
}

fn passes_filters(rel_path: &str, globs: &[String], file_type: &Option<String>) -> bool {
    if let Some(type_name) = file_type
        && !filetypes::matches_type(rel_path, type_name)
    {
        return false;
    }
    if !passes_glob_filters(globs, rel_path) {
        return false;
    }
    true
}

/// Check if a path passes all glob filters. If no globs are specified, all
/// paths pass. When multiple globs are given, the path must match at least one.
fn passes_glob_filters(globs: &[String], path: &str) -> bool {
    if globs.is_empty() {
        return true;
    }
    globs.iter().any(|g| glob_matches(g, path))
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace('.', r"\.");
    let pattern = pattern.replace("**", "§§");
    let pattern = pattern.replace('*', "[^/]*");
    let pattern = pattern.replace("§§", ".*");
    regex::Regex::new(&format!("(?i){pattern}$"))
        .map(|re| re.is_match(path))
        .unwrap_or(false)
}

fn plan_summary(plan: &QueryPlan) -> String {
    match plan {
        QueryPlan::And(tris) => format!("AND({} trigrams)", tris.len()),
        QueryPlan::Or(plans) => {
            let subs: Vec<String> = plans.iter().map(plan_summary).collect();
            format!("OR({})", subs.join(", "))
        }
        QueryPlan::MatchAll => "MatchAll (full scan)".to_string(),
    }
}
