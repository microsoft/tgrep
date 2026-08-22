//! Shared matching logic used by both the server (serve.rs) and the local
//! search path (search.rs). Owns pattern compilation, line/offset mapping and
//! the match-and-context traversal so the two callers cannot drift apart —
//! they differ only in how they render the resulting events.

use std::borrow::Cow;
use std::collections::BTreeMap;

use anyhow::Result;
use regex::RegexBuilder;

/// Byte offsets of the start of every line in a buffer.
///
/// Search results are produced as byte ranges over the whole file so a single
/// match can span lines (`--multiline`). This maps those ranges back to line
/// numbers and columns.
pub struct LineIndex {
    starts: Vec<usize>,
    len: usize,
}

impl LineIndex {
    pub fn new(content: &str) -> Self {
        let mut starts = Vec::new();
        if !content.is_empty() {
            starts.push(0);
            for (i, b) in content.bytes().enumerate() {
                if b == b'\n' {
                    starts.push(i + 1);
                }
            }
            // A trailing newline opens a final empty line that `str::lines`
            // does not yield. Drop it so line counts agree with the rest of
            // the pipeline, which is still line-oriented.
            if starts.len() > 1 && starts[starts.len() - 1] == content.len() {
                starts.pop();
            }
        }
        Self {
            starts,
            len: content.len(),
        }
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// 0-based index of the line containing `offset`.
    pub fn line_of(&self, offset: usize) -> usize {
        if self.starts.is_empty() {
            return 0;
        }
        match self.starts.binary_search(&offset) {
            Ok(i) => i,
            // `starts[0]` is always 0, so a miss can never land at index 0.
            Err(i) => i - 1,
        }
    }

    pub fn line_start(&self, idx: usize) -> usize {
        self.starts.get(idx).copied().unwrap_or(self.len)
    }

    /// End offset of line `idx`, excluding its `\n` or `\r\n` terminator.
    pub fn line_end(&self, content: &str, idx: usize) -> usize {
        let start = self.line_start(idx);
        let mut end = self.starts.get(idx + 1).copied().unwrap_or(self.len);
        let bytes = content.as_bytes();
        if end > start && bytes[end - 1] == b'\n' {
            end -= 1;
        }
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        end
    }

    /// Byte length of line `idx`'s terminator in `content`: 2 for `\r\n`, 1 for
    /// a bare `\n`, 0 for a final line that the file does not terminate.
    ///
    /// `-M/--max-columns` measures the line *including* its terminator, so this
    /// is needed to decide whether a line is over the limit.
    pub fn line_terminator_len(&self, content: &str, idx: usize) -> usize {
        let end = self.starts.get(idx + 1).copied().unwrap_or(self.len);
        end - self.line_end(content, idx)
    }

    pub fn line_text<'a>(&self, content: &'a str, idx: usize) -> &'a str {
        &content[self.line_start(idx)..self.line_end(content, idx)]
    }
}

/// One line of output together with the match ranges inside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineHit {
    /// 0-based line index.
    pub idx: usize,
    /// Match ranges relative to the start of this line's text. Sorted and
    /// non-overlapping.
    pub spans: Vec<(usize, usize)>,
}

/// Keep only the spans falling in the first `max` contiguous blocks of lines.
///
/// A block is the run of lines covered by one or more matches that touch or
/// overlap one another. This is the unit ripgrep's multiline searcher counts
/// for `--max-count`, so several matches sharing a line spend one unit between
/// them, and a match straddling two lines also spends just one.
fn limit_to_line_blocks(
    index: &LineIndex,
    spans: &[(usize, usize)],
    max: Option<usize>,
) -> Vec<(usize, usize)> {
    let Some(max) = max else {
        return spans.to_vec();
    };
    let mut kept = Vec::with_capacity(spans.len().min(max));
    let mut blocks = 0usize;
    let mut block_end: Option<usize> = None;
    for &(s, e) in spans {
        let first = index.line_of(s);
        // An empty match sits entirely on its start line.
        let last = if e > s { index.line_of(e - 1) } else { first };
        match block_end {
            Some(end) if first <= end => block_end = Some(end.max(last)),
            _ => {
                if blocks == max {
                    break;
                }
                blocks += 1;
                block_end = Some(last);
            }
        }
        kept.push((s, e));
    }
    kept
}

/// Trim each span to the end of the line it starts on.
///
/// `--vimgrep` reports one row per match, so a multiline match has to stay on a
/// single line. ripgrep keeps the line the match *starts* on, which is the
/// position an editor should jump to.
fn clip_spans_to_start_line(
    content: &str,
    index: &LineIndex,
    spans: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    spans
        .iter()
        .map(|&(s, e)| {
            let line_end = index.line_end(content, index.line_of(s));
            (s, e.min(line_end))
        })
        .collect()
}

/// Clip absolute match ranges onto the lines they cover.
///
/// Under `--multiline` a single match can cover several lines, and every line
/// it touches has to be printed. Overlapping ranges on the same line are
/// merged so highlighting never emits nested escape sequences.
pub fn group_spans_by_line(
    content: &str,
    index: &LineIndex,
    spans: &[(usize, usize)],
) -> Vec<LineHit> {
    let mut by_line: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();

    for &(s, e) in spans {
        let first = index.line_of(s);
        // An empty match sits entirely on its start line.
        let last = if e > s { index.line_of(e - 1) } else { first };
        for li in first..=last.min(index.line_count().saturating_sub(1)) {
            let ls = index.line_start(li);
            let le = index.line_end(content, li);
            let cs = s.clamp(ls, le) - ls;
            let ce = e.clamp(ls, le) - ls;
            by_line.entry(li).or_default().push((cs, ce));
        }
    }

    by_line
        .into_iter()
        .map(|(idx, mut spans)| {
            spans.sort_unstable();
            let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
            for span in spans {
                match merged.last_mut() {
                    Some(last) if span.0 <= last.1 => last.1 = last.1.max(span.1),
                    _ => merged.push(span),
                }
            }
            LineHit { idx, spans: merged }
        })
        .collect()
}

/// Expand match indices into the full set of line indices to display,
/// including context lines. Returns a sorted, deduplicated set.
pub fn expand_context_window(
    match_indices: &[usize],
    total_lines: usize,
    before: usize,
    after: usize,
) -> std::collections::BTreeSet<usize> {
    let mut printed = std::collections::BTreeSet::new();
    for &mi in match_indices {
        let start = mi.saturating_sub(before);
        let end = (mi + after + 1).min(total_lines);
        for j in start..end {
            printed.insert(j);
        }
    }
    printed
}

/// The compiled pattern. `regex` handles almost everything and is linear-time;
/// `fancy_regex` is the fallback for backreferences and lookaround.
pub enum SearchMatcher {
    Standard(regex::Regex),
    Fancy(fancy_regex::Regex),
}

impl SearchMatcher {
    pub fn is_standard(&self) -> bool {
        matches!(self, SearchMatcher::Standard(_))
    }

    pub fn is_match(&self, hay: &str) -> Result<bool> {
        match self {
            SearchMatcher::Standard(re) => Ok(re.is_match(hay)),
            SearchMatcher::Fancy(re) => re
                .is_match(hay)
                .map_err(|e| anyhow::anyhow!("regex match error: {e}")),
        }
    }

    /// All non-overlapping match ranges in `hay`, as byte offsets.
    pub fn find_spans(&self, hay: &str) -> Result<Vec<(usize, usize)>> {
        let mut spans: Vec<(usize, usize)> = Vec::new();
        match self {
            SearchMatcher::Standard(re) => {
                for m in re.find_iter(hay) {
                    spans.push((m.start(), m.end()));
                }
            }
            SearchMatcher::Fancy(re) => {
                for m in re.find_iter(hay) {
                    let m = m.map_err(|e| anyhow::anyhow!("regex match error: {e}"))?;
                    spans.push((m.start(), m.end()));
                }
            }
        }
        Ok(spans)
    }

    /// The first match range in `hay`, or none.
    ///
    /// The cheap counterpart to [`find_spans`](Self::find_spans) for callers
    /// that only need to know whether the line matched and where it starts.
    pub fn find_first_span(&self, hay: &str) -> Result<Option<(usize, usize)>> {
        match self {
            SearchMatcher::Standard(re) => Ok(re.find(hay).map(|m| (m.start(), m.end()))),
            SearchMatcher::Fancy(re) => re
                .find(hay)
                .map(|m| m.map(|m| (m.start(), m.end())))
                .map_err(|e| anyhow::anyhow!("regex match error: {e}")),
        }
    }

    /// Rewrite every match in `hay` using `replacement`, which may reference
    /// capture groups as `$1` or `${name}`.
    ///
    /// Returns the rewritten text plus the byte ranges the replacements now
    /// occupy, so `--replace` still highlights and reports columns correctly.
    pub fn replace_all(
        &self,
        hay: &str,
        replacement: &str,
    ) -> Result<(String, Vec<(usize, usize)>)> {
        let mut out = String::with_capacity(hay.len());
        let mut spans = Vec::new();
        let mut last = 0usize;
        for (start, end, expanded) in self.expansions(hay, replacement)? {
            out.push_str(&hay[last..start]);
            let span_start = out.len();
            out.push_str(&expanded);
            spans.push((span_start, out.len()));
            last = end;
        }
        out.push_str(&hay[last..]);
        Ok((out, spans))
    }

    /// The expanded replacement for each match, with the match's byte range.
    fn expansions(&self, hay: &str, replacement: &str) -> Result<Vec<(usize, usize, String)>> {
        let mut out = Vec::new();
        match self {
            SearchMatcher::Standard(re) => {
                for caps in re.captures_iter(hay) {
                    let m = caps.get(0).expect("group 0 always participates");
                    let mut dst = String::new();
                    caps.expand(replacement, &mut dst);
                    out.push((m.start(), m.end(), dst));
                }
            }
            SearchMatcher::Fancy(re) => {
                for caps in re.captures_iter(hay) {
                    let caps = caps.map_err(|e| anyhow::anyhow!("regex match error: {e}"))?;
                    let m = caps.get(0).expect("group 0 always participates");
                    let mut dst = String::new();
                    caps.expand(replacement, &mut dst);
                    out.push((m.start(), m.end(), dst));
                }
            }
        }
        Ok(out)
    }
}

/// Which regex engine to use, mirroring ripgrep's `--engine`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RegexEngine {
    /// Rust's `regex`, falling back to the PCRE-style engine when a pattern
    /// uses backreferences or lookaround.
    #[default]
    Auto,
    /// Rust's `regex` only; an unsupported pattern is an error.
    Default,
    /// Always use the PCRE-style engine (`-P/--pcre2`).
    Pcre2,
}

impl RegexEngine {
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "pcre2" => Some(Self::Pcre2),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    /// The canonical name, used to transport the choice over RPC.
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Pcre2 => "pcre2",
            Self::Auto => "auto",
        }
    }
}

/// Everything that affects how the pattern itself is compiled.
#[derive(Clone, Debug, Default)]
pub struct MatcherConfig {
    pub case_insensitive: bool,
    pub fixed_string: bool,
    pub word_boundary: bool,
    /// `-x/--line-regexp`: the pattern must match a whole line.
    pub line_regexp: bool,
    pub multiline: bool,
    pub dotall: bool,
    /// `--no-unicode`: disable Unicode-aware character classes.
    pub no_unicode: bool,
    pub engine: RegexEngine,
    pub regex_size_limit: Option<usize>,
    pub dfa_size_limit: Option<usize>,
}

/// Compile `patterns` into a single matcher.
///
/// `multiline` lets a match cross line boundaries; `dotall` is kept separate
/// so `-U` alone does not silently turn `.` into a line-crossing wildcard,
/// matching ripgrep's split between `--multiline` and `--multiline-dotall`.
pub fn build_search_matcher(patterns: &[String], cfg: &MatcherConfig) -> Result<SearchMatcher> {
    let combined = combine_patterns(
        patterns,
        cfg.fixed_string,
        cfg.word_boundary,
        cfg.line_regexp,
    );

    if cfg.engine == RegexEngine::Pcre2 {
        return build_fancy(&combined, cfg).map_err(|e| {
            anyhow::anyhow!("regex error: PCRE-style engine rejected the pattern: {e}")
        });
    }

    match build_regex(&combined, cfg) {
        Ok(re) => Ok(SearchMatcher::Standard(re)),
        // Exceeding `--regex-size-limit`/`--dfa-size-limit` is a resource error,
        // not an unsupported construct. Retrying on the backtracking engine
        // would silently ignore the limit the user asked for.
        Err(e @ regex::Error::CompiledTooBig(_)) => Err(anyhow::anyhow!("regex error: {e}")),
        Err(regex_err) if !cfg.fixed_string && cfg.engine == RegexEngine::Auto => {
            build_fancy(&combined, cfg).map_err(|fancy_err| {
                anyhow::anyhow!("regex error: {regex_err}; PCRE-style fallback failed: {fancy_err}")
            })
        }
        Err(regex_err) => Err(anyhow::anyhow!("regex error: {regex_err}")),
    }
}

fn build_fancy(combined: &str, cfg: &MatcherConfig) -> Result<SearchMatcher> {
    let mut flags = String::new();
    if cfg.case_insensitive {
        flags.push('i');
    }
    if cfg.multiline {
        flags.push('m');
    }
    if cfg.dotall {
        flags.push('s');
    }
    let pattern = if flags.is_empty() {
        combined.to_string()
    } else {
        format!("(?{flags}:{combined})")
    };
    fancy_regex::Regex::new(&pattern)
        .map(SearchMatcher::Fancy)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn combine_patterns(
    patterns: &[String],
    fixed_string: bool,
    word_boundary: bool,
    line_regexp: bool,
) -> String {
    let wrap = |p: &String| {
        let mut p = if fixed_string {
            regex::escape(p)
        } else {
            p.clone()
        };
        // `-x` beats `-w`: a whole-line match is already at word boundaries,
        // and applying both would reject lines starting with punctuation.
        if line_regexp {
            p = format!(r"^(?:{p})$");
        } else if word_boundary {
            p = format!(r"\b(?:{p})\b");
        }
        p
    };

    if patterns.len() == 1 {
        wrap(&patterns[0])
    } else {
        let parts: Vec<String> = patterns.iter().map(wrap).collect();
        format!("(?:{})", parts.join("|"))
    }
}

fn build_regex(
    combined: &str,
    cfg: &MatcherConfig,
) -> std::result::Result<regex::Regex, regex::Error> {
    RegexBuilder::new(combined)
        .case_insensitive(cfg.case_insensitive)
        .multi_line(cfg.multiline)
        .dot_matches_new_line(cfg.dotall)
        .unicode(!cfg.no_unicode)
        .size_limit(cfg.regex_size_limit.unwrap_or(100 * (1 << 20)))
        .dfa_size_limit(cfg.dfa_size_limit.unwrap_or(1000 * (1 << 20)))
        .nest_limit(250)
        .build()
}

/// The subset of search options that affects which lines match and which are
/// emitted. Shared so the server and the local search path cannot drift.
#[derive(Clone, Default)]
pub struct MatchOptions {
    pub invert_match: bool,
    pub multiline: bool,
    pub only_matching: bool,
    pub before_context: usize,
    pub after_context: usize,
    pub max_count: Option<usize>,
    /// `--passthru`: emit every line, matching or not.
    pub passthru: bool,
    /// `-r/--replace`: rewrite each match before printing.
    pub replace: Option<String>,
    /// `--stop-on-nonmatch`: stop at the first non-matching line that follows
    /// a matching one.
    pub stop_on_nonmatch: bool,
    /// `--vimgrep`: report one row per match. Under `--multiline` this collapses
    /// a match spanning several lines onto the line it starts on, as ripgrep
    /// does, so editors get one jump target per match.
    pub vimgrep: bool,
    /// Whether every match on a line has to be located, or just the first.
    ///
    /// Only highlighting, `--vimgrep`, `--json`, `-o`, `-r` and
    /// `--count-matches` care where the later matches on a line are; a plain
    /// search only needs to know the line matched at all. Scanning the rest of
    /// each matching line is pure overhead in that case, so this turns it off.
    pub all_spans: bool,
}

/// One unit of output produced by searching a single file.
pub enum Emit<'a> {
    Match {
        line_number: usize,
        content: Cow<'a, str>,
        /// 1-based byte columns of each match on the line, in order.
        columns: Vec<usize>,
        spans: Vec<(usize, usize)>,
        absolute_offset: usize,
        /// Offset of the start of the line. `absolute_offset` points at the
        /// match itself under `-o`, so a caller translating offsets back to the
        /// source encoding needs this separately.
        line_offset: usize,
        /// Bytes each column moves by once `--replace` has rewritten the line,
        /// parallel to `columns`. Empty when nothing was replaced.
        ///
        /// `columns` always indexes the *decoded source* line, so it can be
        /// mapped back to the bytes on disk; ripgrep then reports the position
        /// in the rewritten line, which is that mapped column plus the length
        /// delta of every replacement before it.
        column_shifts: Vec<isize>,
        /// The same correction for `absolute_offset`. Non-zero only under `-o`,
        /// where the offset points at an individual replacement.
        offset_shift: isize,
        /// Bytes of the line terminator that `content` was stripped of, which
        /// `-M/--max-columns` counts towards the line's length. Zero under `-o`,
        /// where ripgrep measures the matched text on its own.
        terminator_len: usize,
    },
    Context {
        line_number: usize,
        content: &'a str,
        absolute_offset: usize,
        /// See [`Emit::Match::terminator_len`].
        terminator_len: usize,
    },
}

/// Result of matching one file: the line index plus every matching line.
pub struct FileMatches<'a> {
    content: &'a str,
    index: LineIndex,
    hits: Vec<LineHit>,
}

impl<'a> FileMatches<'a> {
    pub fn find(content: &'a str, matcher: &SearchMatcher, opts: &MatchOptions) -> Result<Self> {
        let index = LineIndex::new(content);
        let hits = if opts.max_count == Some(0) {
            Vec::new()
        } else {
            collect_hits(content, &index, matcher, opts)?
        };
        Ok(Self {
            content,
            index,
            hits,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// Number of matching lines, which is what `-c/--count` reports.
    pub fn matched_lines(&self) -> usize {
        self.hits.len()
    }

    /// Total number of individual matches, which is what `--count-matches`
    /// reports. Inverted matches have no spans but still count as one.
    pub fn match_count(&self) -> usize {
        self.hits
            .iter()
            .map(|h| h.spans.len().max(1))
            .sum::<usize>()
    }

    /// Walk the output in line order, expanding context and fanning
    /// `--only-matching` out to one event per match.
    pub fn for_each<E>(
        &self,
        opts: &MatchOptions,
        matcher: &SearchMatcher,
        mut on_emit: impl FnMut(Emit<'a>) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E>
    where
        E: From<anyhow::Error>,
    {
        let emit_hit = |hit: &LineHit,
                        on_emit: &mut dyn FnMut(Emit<'a>) -> std::result::Result<(), E>|
         -> std::result::Result<(), E> {
            let line = self.index.line_text(self.content, hit.idx);
            let offset = self.index.line_start(hit.idx);

            if opts.only_matching && !hit.spans.is_empty() {
                // With `-r`, `-o` prints the replacement of each match rather
                // than the matched text itself.
                if let Some(rep) = &opts.replace {
                    // ripgrep rewrites the line and reports offsets into the
                    // result, so each match moves by the length delta of the
                    // ones before it. Columns stay in source terms here and are
                    // corrected by `column_shifts` after the caller has mapped
                    // them back through any lossy-decoding repairs.
                    let mut shift: isize = 0;
                    for (start, end, text) in matcher.expansions(line, rep).map_err(E::from)? {
                        let len = text.len();
                        on_emit(Emit::Match {
                            line_number: hit.idx + 1,
                            content: Cow::Owned(text),
                            columns: vec![start + 1],
                            spans: vec![(0, len)],
                            absolute_offset: offset + start,
                            line_offset: offset,
                            column_shifts: vec![shift],
                            offset_shift: shift,
                            terminator_len: 0,
                        })?;
                        shift += len as isize - (end - start) as isize;
                    }
                    return Ok(());
                }
                for &(s, e) in &hit.spans {
                    let text = &line[s..e];
                    on_emit(Emit::Match {
                        line_number: hit.idx + 1,
                        content: Cow::Borrowed(text),
                        columns: vec![s + 1],
                        spans: vec![(0, text.len())],
                        absolute_offset: offset + s,
                        line_offset: offset,
                        column_shifts: Vec::new(),
                        offset_shift: 0,
                        terminator_len: 0,
                    })?;
                }
                return Ok(());
            }

            let (content, spans, columns, column_shifts) = match &opts.replace {
                Some(rep) if !hit.spans.is_empty() => {
                    let (text, spans) = matcher.replace_all(line, rep).map_err(E::from)?;
                    // `spans` locate each replacement in the rewritten line;
                    // `hit.spans` locate the matches they stand in for. The
                    // difference is exactly how far that column moved.
                    let columns = hit.spans.iter().map(|&(s, _)| s + 1).collect();
                    let shifts = hit
                        .spans
                        .iter()
                        .enumerate()
                        .map(|(i, &(s, _))| {
                            spans.get(i).map_or(0, |&(rs, _)| rs as isize - s as isize)
                        })
                        .collect();
                    (Cow::Owned(text), spans, columns, shifts)
                }
                _ => (
                    Cow::Borrowed(line),
                    hit.spans.clone(),
                    hit.spans.iter().map(|&(s, _)| s + 1).collect(),
                    Vec::new(),
                ),
            };
            on_emit(Emit::Match {
                line_number: hit.idx + 1,
                content,
                columns,
                spans,
                absolute_offset: offset,
                line_offset: offset,
                column_shifts,
                offset_shift: 0,
                terminator_len: self.index.line_terminator_len(self.content, hit.idx),
            })
        };

        // `--passthru` prints the whole file, so every non-matching line is a
        // context line no matter what `-A`/`-B`/`-C` said.
        if opts.passthru {
            let by_line: std::collections::HashMap<usize, &LineHit> =
                self.hits.iter().map(|h| (h.idx, h)).collect();
            for li in 0..self.index.line_count() {
                match by_line.get(&li) {
                    Some(hit) => emit_hit(hit, &mut on_emit)?,
                    None => on_emit(Emit::Context {
                        line_number: li + 1,
                        content: self.index.line_text(self.content, li),
                        absolute_offset: self.index.line_start(li),
                        terminator_len: self.index.line_terminator_len(self.content, li),
                    })?,
                }
            }
            return Ok(());
        }

        if opts.before_context == 0 && opts.after_context == 0 {
            for hit in &self.hits {
                emit_hit(hit, &mut on_emit)?;
            }
            return Ok(());
        }

        let match_indices: Vec<usize> = self.hits.iter().map(|h| h.idx).collect();
        let printed = expand_context_window(
            &match_indices,
            self.index.line_count(),
            opts.before_context,
            opts.after_context,
        );
        let by_line: std::collections::HashMap<usize, &LineHit> =
            self.hits.iter().map(|h| (h.idx, h)).collect();

        for &li in &printed {
            match by_line.get(&li) {
                Some(hit) => emit_hit(hit, &mut on_emit)?,
                None => on_emit(Emit::Context {
                    line_number: li + 1,
                    content: self.index.line_text(self.content, li),
                    absolute_offset: self.index.line_start(li),
                    terminator_len: self.index.line_terminator_len(self.content, li),
                })?,
            }
        }
        Ok(())
    }
}

/// Find every matching line together with the match ranges inside it.
fn collect_hits(
    content: &str,
    index: &LineIndex,
    matcher: &SearchMatcher,
    opts: &MatchOptions,
) -> Result<Vec<LineHit>> {
    let max = opts.max_count;

    if opts.invert_match {
        let mut out = Vec::new();
        for i in 0..index.line_count() {
            if !matcher.is_match(index.line_text(content, i))? {
                out.push(LineHit {
                    idx: i,
                    spans: Vec::new(),
                });
                if max.is_some_and(|m| out.len() >= m) {
                    break;
                }
            } else if opts.stop_on_nonmatch && !out.is_empty() {
                // Under `-v` a *matching* line is the non-matching case.
                break;
            }
        }
        return Ok(out);
    }

    if opts.multiline {
        // Match against the whole buffer so a single match can cross lines.
        //
        // ripgrep counts *matching lines* here rather than match spans: its
        // multiline searcher reports one unit per contiguous block of lines
        // that matches cover, and everything inside that block comes with it.
        // So three matches on one line are a single unit under `-m 1` (all
        // three are reported, which `--vimgrep` shows as three rows), and a
        // match spanning two lines is also one unit (both lines print).
        //
        // Limiting the spans themselves would under-report the first case, and
        // truncating the grouped lines would print a partial match — output
        // that doesn't actually match the pattern, with its spans clipped.
        let spans = matcher.find_spans(content)?;
        let spans = limit_to_line_blocks(index, &spans, max);
        if opts.vimgrep {
            // `--vimgrep` wants one jump target per match, so a match that runs
            // past the end of its line is reported only on the line it starts
            // on rather than once per line it touches.
            return Ok(group_spans_by_line(
                content,
                index,
                &clip_spans_to_start_line(content, index, &spans),
            ));
        }
        return Ok(group_spans_by_line(content, index, &spans));
    }

    // Line-oriented mode: match each line separately so `^` and `$` anchor
    // per line, as ripgrep does.
    let mut out = Vec::new();
    for i in 0..index.line_count() {
        let spans = if opts.all_spans {
            matcher.find_spans(index.line_text(content, i))?
        } else {
            matcher
                .find_first_span(index.line_text(content, i))?
                .into_iter()
                .collect()
        };
        if !spans.is_empty() {
            out.push(LineHit { idx: i, spans });
            if max.is_some_and(|m| out.len() >= m) {
                break;
            }
        } else if opts.stop_on_nonmatch && !out.is_empty() {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_blocks_merge_matches_that_share_a_line() {
        let content = "foo foo foo\nfoo bar\n";
        let index = LineIndex::new(content);
        let spans = vec![(0, 3), (4, 7), (8, 11), (12, 15)];

        // All three matches on line 0 are one unit, so `-m 1` keeps them all.
        assert_eq!(
            limit_to_line_blocks(&index, &spans, Some(1)),
            vec![(0, 3), (4, 7), (8, 11)]
        );
        // Line 1 starts a second unit.
        assert_eq!(limit_to_line_blocks(&index, &spans, Some(2)), spans);
        assert_eq!(limit_to_line_blocks(&index, &spans, None), spans);
        assert!(limit_to_line_blocks(&index, &spans, Some(0)).is_empty());
    }

    #[test]
    fn line_blocks_keep_a_span_that_crosses_lines_whole() {
        let content = "a foo\nbar foo\nbaz foo\n";
        let index = LineIndex::new(content);
        // One match covering lines 0-1, then one on line 2.
        let spans = vec![(2, 13), (18, 21)];

        assert_eq!(limit_to_line_blocks(&index, &spans, Some(1)), vec![(2, 13)]);
        assert_eq!(limit_to_line_blocks(&index, &spans, Some(2)), spans);
    }

    #[test]
    fn line_blocks_treat_an_empty_match_as_one_line() {
        let content = "ab\ncd\n";
        let index = LineIndex::new(content);
        let spans = vec![(0, 0), (1, 1), (3, 3)];

        assert_eq!(
            limit_to_line_blocks(&index, &spans, Some(1)),
            vec![(0, 0), (1, 1)],
            "both empty matches sit on line 0"
        );
    }

    #[test]
    fn line_index_matches_str_lines() {
        for content in ["", "a", "a\n", "a\nb", "a\n\nb", "\n", "a\r\nb\r\n"] {
            let index = LineIndex::new(content);
            let expected: Vec<&str> = content.lines().collect();
            assert_eq!(index.line_count(), expected.len(), "count for {content:?}");
            for (i, want) in expected.iter().enumerate() {
                assert_eq!(
                    &index.line_text(content, i),
                    want,
                    "line {i} of {content:?}"
                );
            }
        }
    }

    #[test]
    fn line_of_maps_offsets_to_lines() {
        let content = "ab\ncd\nef";
        let index = LineIndex::new(content);
        assert_eq!(index.line_of(0), 0);
        assert_eq!(index.line_of(1), 0);
        assert_eq!(index.line_of(3), 1);
        assert_eq!(index.line_of(6), 2);
    }

    #[test]
    fn group_spans_clips_multiline_match_onto_each_line() {
        let content = "start\nmiddle\nend\n";
        let index = LineIndex::new(content);
        // "start\nmiddle\nend" as one match spanning three lines.
        let hits = group_spans_by_line(content, &index, &[(0, 16)]);
        assert_eq!(
            hits,
            vec![
                LineHit {
                    idx: 0,
                    spans: vec![(0, 5)]
                },
                LineHit {
                    idx: 1,
                    spans: vec![(0, 6)]
                },
                LineHit {
                    idx: 2,
                    spans: vec![(0, 3)]
                },
            ]
        );
    }

    #[test]
    fn group_spans_merges_overlapping_ranges_on_one_line() {
        let content = "aaaa";
        let index = LineIndex::new(content);
        let hits = group_spans_by_line(content, &index, &[(0, 2), (1, 3)]);
        assert_eq!(
            hits,
            vec![LineHit {
                idx: 0,
                spans: vec![(0, 3)]
            }]
        );
    }

    #[test]
    fn group_spans_keeps_separate_ranges_apart() {
        let content = "foo bar foo";
        let index = LineIndex::new(content);
        let hits = group_spans_by_line(content, &index, &[(0, 3), (8, 11)]);
        assert_eq!(
            hits,
            vec![LineHit {
                idx: 0,
                spans: vec![(0, 3), (8, 11)]
            }]
        );
    }
}
