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
///
/// The table is built on first use rather than up front, because most searches
/// never need it. It costs one `usize` per line — 670 MB on a 13.4 GiB file
/// with 83.8M lines — plus two `memchr` passes over the whole buffer, and a
/// file with no match asks it nothing at all: the whole-buffer prescan finds no
/// match, so no offset is ever mapped to a line. Building it eagerly was 4.7 s
/// of the 30.7 s that file took to search, spent entirely on an answer nobody
/// wanted.
pub struct LineIndex<'a> {
    content: &'a str,
    starts: std::cell::OnceCell<Vec<usize>>,
}

impl<'a> LineIndex<'a> {
    pub fn new(content: &'a str) -> Self {
        Self {
            content,
            starts: std::cell::OnceCell::new(),
        }
    }

    fn starts(&self) -> &[usize] {
        self.starts.get_or_init(|| {
            let content = self.content;
            let mut starts = Vec::new();
            if !content.is_empty() {
                // Size the index up front. It holds one `usize` per line, so
                // growing it by doubling both overshoots — up to twice what is
                // needed — and copies the whole thing on the way there. On a
                // large file that transient is tens of megabytes, which is pure
                // waste when the exact count is one SIMD scan away.
                starts.reserve_exact(1 + memchr::memchr_iter(b'\n', content.as_bytes()).count());
                starts.push(0);
                for i in memchr::memchr_iter(b'\n', content.as_bytes()) {
                    starts.push(i + 1);
                }
                // A trailing newline opens a final empty line that `str::lines`
                // does not yield. Drop it so line counts agree with the rest of
                // the pipeline, which is still line-oriented.
                if starts.len() > 1 && starts[starts.len() - 1] == content.len() {
                    starts.pop();
                }
            }
            starts
        })
    }

    pub fn line_count(&self) -> usize {
        self.starts().len()
    }

    /// 0-based index of the line containing `offset`.
    pub fn line_of(&self, offset: usize) -> usize {
        let starts = self.starts();
        if starts.is_empty() {
            return 0;
        }
        match starts.binary_search(&offset) {
            Ok(i) => i,
            // `starts[0]` is always 0, so a miss can never land at index 0.
            Err(i) => i - 1,
        }
    }

    pub fn line_start(&self, idx: usize) -> usize {
        self.starts().get(idx).copied().unwrap_or(self.content.len())
    }

    /// End offset of line `idx`, excluding its `\n` or `\r\n` terminator.
    pub fn line_end(&self, idx: usize) -> usize {
        let start = self.line_start(idx);
        let mut end = self
            .starts()
            .get(idx + 1)
            .copied()
            .unwrap_or(self.content.len());
        let bytes = self.content.as_bytes();
        if end > start && bytes[end - 1] == b'\n' {
            end -= 1;
        }
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        end
    }

    /// Byte length of line `idx`'s terminator in the buffer: 2 for `\r\n`, 1
    /// for a bare `\n`, 0 for a final line that the file does not terminate.
    ///
    /// `-M/--max-columns` measures the line *including* its terminator, so this
    /// is needed to decide whether a line is over the limit.
    pub fn line_terminator_len(&self, idx: usize) -> usize {
        let end = self
            .starts()
            .get(idx + 1)
            .copied()
            .unwrap_or(self.content.len());
        end - self.line_end(idx)
    }

    pub fn line_text(&self, idx: usize) -> &'a str {
        &self.content[self.line_start(idx)..self.line_end(idx)]
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
    index: &LineIndex<'_>,
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
    index: &LineIndex<'_>,
    spans: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    spans
        .iter()
        .map(|&(s, e)| {
            let line_end = index.line_end(index.line_of(s));
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
    index: &LineIndex<'_>,
    spans: &[(usize, usize)],
) -> Vec<LineHit> {
    let mut by_line: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();

    for &(s, e) in spans {
        let first = index.line_of(s);
        // An empty match sits entirely on its start line.
        let last = if e > s { index.line_of(e - 1) } else { first };
        for li in first..=last.min(index.line_count().saturating_sub(1)) {
            let ls = index.line_start(li);
            let le = index.line_end(li);
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
    /// A backtracking match, optionally gated by a linear-time prefilter.
    ///
    /// `fancy_regex` already delegates to `regex` when a pattern needs nothing
    /// fancy, so this variant only really costs anything when a construct like
    /// lookaround forces the backtracking VM. That VM has no literal prefilter,
    /// so it tries to match at *every* byte offset - on a multi-gigabyte file
    /// that is the difference between seconds and never finishing.
    ///
    /// `prefilter` is the pattern relaxed into a form the linear-time engine can
    /// compile (see `tgrep_core::query::relax_for_indexing`). Relaxation only
    /// widens the language, so a haystack the prefilter rejects cannot possibly
    /// match the real pattern - which makes it sound to use for negative
    /// answers, and only for negative answers.
    Fancy {
        re: fancy_regex::Regex,
        prefilter: Option<regex::Regex>,
    },
}

impl SearchMatcher {
    pub fn is_standard(&self) -> bool {
        matches!(self, SearchMatcher::Standard(_))
    }

    /// Whether the prefilter has already ruled this haystack out.
    ///
    /// A `false` from the prefilter is conclusive; a `true` means nothing, and
    /// the real matcher still has to run.
    fn ruled_out(prefilter: &Option<regex::Regex>, hay: &str) -> bool {
        prefilter.as_ref().is_some_and(|re| !re.is_match(hay))
    }

    pub fn is_match(&self, hay: &str) -> Result<bool> {
        match self {
            SearchMatcher::Standard(re) => Ok(re.is_match(hay)),
            SearchMatcher::Fancy { re, prefilter } => {
                if Self::ruled_out(prefilter, hay) {
                    return Ok(false);
                }
                re.is_match(hay)
                    .map_err(|e| anyhow::anyhow!("regex match error: {e}"))
            }
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
            SearchMatcher::Fancy { re, prefilter } => {
                if Self::ruled_out(prefilter, hay) {
                    return Ok(spans);
                }
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
            SearchMatcher::Fancy { re, prefilter } => {
                if Self::ruled_out(prefilter, hay) {
                    return Ok(None);
                }
                re.find(hay)
                    .map(|m| m.map(|m| (m.start(), m.end())))
                    .map_err(|e| anyhow::anyhow!("regex match error: {e}"))
            }
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
            SearchMatcher::Fancy { re, prefilter } => {
                if Self::ruled_out(prefilter, hay) {
                    return Ok(out);
                }
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
    let wrap = |body: &str| {
        if flags.is_empty() {
            body.to_string()
        } else {
            format!("(?{flags}:{body})")
        }
    };
    let pattern = wrap(combined);
    let re = fancy_regex::Regex::new(&pattern).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(SearchMatcher::Fancy {
        re,
        prefilter: build_fancy_prefilter(combined, &wrap),
    })
}

/// Compile a linear-time approximation of `combined` to use as a prefilter.
///
/// Only worth building when relaxation actually removed something: if the
/// relaxed pattern is identical, `fancy_regex` is already delegating to `regex`
/// internally and a second engine would be pure overhead. Anything that fails to
/// relax or fails to compile simply yields no prefilter, which costs performance
/// and never correctness.
fn build_fancy_prefilter(combined: &str, wrap: &dyn Fn(&str) -> String) -> Option<regex::Regex> {
    let relaxed = tgrep_core::query::relax_for_indexing(combined)?;
    if relaxed == combined {
        return None;
    }
    regex::Regex::new(&wrap(&relaxed)).ok()
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
    index: LineIndex<'a>,
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
        Ok(Self { index, hits })
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
            let line = self.index.line_text(hit.idx);
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
                terminator_len: self.index.line_terminator_len(hit.idx),
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
                        content: self.index.line_text(li),
                        absolute_offset: self.index.line_start(li),
                        terminator_len: self.index.line_terminator_len(li),
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
                    content: self.index.line_text(li),
                    absolute_offset: self.index.line_start(li),
                    terminator_len: self.index.line_terminator_len(li),
                })?,
            }
        }
        Ok(())
    }
}

/// Whether `pattern` is free of anchors whose meaning depends on where the
/// haystack starts and ends.
///
/// Line-oriented matching runs the pattern against one line at a time, so `^`,
/// `$`, `\A`, `\z` and `\Z` all bind to the line. Running the identical regex
/// over the whole buffer instead would bind them to the buffer, which *narrows*
/// what matches - the one direction a prescan must never take. Without them the
/// two haystacks agree, because nothing else in the pattern can observe where
/// the haystack was cut.
///
/// Character classes are not tracked: a `^` or `$` inside one is a literal and
/// would be safe, but treating it as an anchor only costs the optimisation, not
/// correctness. Erring toward "not anchor-free" is always sound.
fn pattern_is_anchor_free(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                // `\A`, `\z` and `\Z` stay bound to the haystack ends even under
                // `(?m)`, so they can never be made line-relative.
                match bytes.get(i + 1) {
                    Some(b'A') | Some(b'z') | Some(b'Z') => return false,
                    Some(_) => i += 2,
                    None => return false,
                }
            }
            b'^' | b'$' => return false,
            _ => i += 1,
        }
    }
    true
}

/// The lines that could possibly contain a match, or `None` for "check them
/// all".
///
/// The line-oriented loop asks the regex engine once per line. On a
/// multi-gigabyte file that is billions of calls, and the engine's SIMD
/// prefilter never gets a run long enough to pay for itself - which is most of
/// why a whole-file scan costs several times what ripgrep charges. Scanning the
/// buffer once and verifying only the lines a match touches collapses that to a
/// single pass.
///
/// Soundness rests on two things. The pattern must be anchor-free, so that a
/// line and the buffer are interchangeable haystacks. And every line a returned
/// match *spans* is a candidate, not just the line it starts on: `find_iter`
/// yields leftmost non-overlapping matches, so a match beginning on an earlier
/// line can be the one that covers this line's match, and a pattern that can
/// cross `\n` (via `\s`, a negated class, or `-s`) can span several. Under those
/// rules every line that really matches is covered, and the per-line matcher
/// still has the final say, so extra candidates cost time and never accuracy.
fn candidate_lines<'a>(
    content: &'a str,
    index: &'a LineIndex<'a>,
    matcher: &'a SearchMatcher,
    opts: &MatchOptions,
) -> Option<impl Iterator<Item = usize> + 'a> {
    // `--stop-on-nonmatch` has to see the first line that *fails*, which a
    // candidate list by construction skips over.
    if opts.stop_on_nonmatch {
        return None;
    }
    let SearchMatcher::Standard(re) = matcher else {
        return None;
    };
    if !pattern_is_anchor_free(re.as_str()) {
        return None;
    }
    // A pattern that matches the empty string matches at every offset, so the
    // prescan would yield every line and the real loop is cheaper.
    if re.is_match("") {
        return None;
    }

    let mut last: Option<usize> = None;
    Some(
        re.find_iter(content)
            .flat_map(move |m| {
                let first = index.line_of(m.start());
                // Matches are non-empty here, so `end - 1` is inside the match.
                let last_line = index.line_of(m.end() - 1);
                first..=last_line
            })
            .filter(move |&li| {
                let fresh = last != Some(li);
                if fresh {
                    last = Some(li);
                }
                fresh
            }),
    )
}

/// Find every matching line together with the match ranges inside it.
fn collect_hits(
    content: &str,
    index: &LineIndex<'_>,
    matcher: &SearchMatcher,
    opts: &MatchOptions,
) -> Result<Vec<LineHit>> {
    let max = opts.max_count;

    if opts.invert_match {
        let mut out = Vec::new();
        for i in 0..index.line_count() {
            if !matcher.is_match(index.line_text(i))? {
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
                index,
                &clip_spans_to_start_line(index, &spans),
            ));
        }
        return Ok(group_spans_by_line(index, &spans));
    }

    // Line-oriented mode: match each line separately so `^` and `$` anchor
    // per line, as ripgrep does.
    let mut out = Vec::new();
    let visit = |i: usize, out: &mut Vec<LineHit>| -> Result<bool> {
        let spans = if opts.all_spans {
            matcher.find_spans(index.line_text(i))?
        } else {
            matcher
                .find_first_span(index.line_text(i))?
                .into_iter()
                .collect()
        };
        if !spans.is_empty() {
            out.push(LineHit { idx: i, spans });
            if max.is_some_and(|m| out.len() >= m) {
                return Ok(false);
            }
        } else if opts.stop_on_nonmatch && !out.is_empty() {
            return Ok(false);
        }
        Ok(true)
    };

    // The prescan is lazy, so `--max-count` still stops early instead of
    // scanning the rest of the file for candidates it will never look at.
    if let Some(candidates) = candidate_lines(content, index, matcher, opts) {
        for i in candidates {
            if !visit(i, &mut out)? {
                break;
            }
        }
        return Ok(out);
    }

    for i in 0..index.line_count() {
        if !visit(i, &mut out)? {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // The whole-buffer line prescan
    //
    // Asking the regex engine once per line is billions of calls on a large
    // file. Scanning the buffer once and verifying only the lines a match
    // touches is equivalent *only* when the pattern cannot tell where the
    // haystack was cut, so these pin down both the gate and the equivalence.
    // -----------------------------------------------------------------------

    #[test]
    fn anchors_disable_the_prescan() {
        assert!(pattern_is_anchor_free("needle"));
        assert!(pattern_is_anchor_free(r"\bneedle\b"));
        assert!(pattern_is_anchor_free(r"a\\b"));
        assert!(pattern_is_anchor_free(r"\^literal"));
        assert!(pattern_is_anchor_free(r"\$literal"));

        assert!(!pattern_is_anchor_free("^needle"));
        assert!(!pattern_is_anchor_free("needle$"));
        assert!(!pattern_is_anchor_free(r"\Aneedle"));
        assert!(!pattern_is_anchor_free(r"needle\z"));
        assert!(!pattern_is_anchor_free(r"needle\Z"));
        // A class member is a literal, but rejecting it only loses speed.
        assert!(!pattern_is_anchor_free("[$]"));
        // A trailing backslash is malformed; refuse rather than index past it.
        assert!(!pattern_is_anchor_free("needle\\"));
    }

    // The property that makes the prescan safe: for every pattern it accepts,
    // the lines it proposes are a superset of the lines that actually match.
    // Brute-forced against the per-line loop the prescan replaces.
    #[test]
    fn prescan_never_drops_a_matching_line() {
        let patterns = [
            "a",
            "ab",
            "a.c",
            "a+b",
            "[abc]+",
            "a|bc",
            r"\bword\b",
            r"\s+x",
            "a\nb",
            r"[^q]z",
            "(?i)ABC",
            r"\d+",
            "xyz",
        ];
        let haystacks = [
            "",
            "a",
            "abc\n",
            "abc\ndef\n",
            "a\nb\nc\n",
            "aaa\nbbb\nccc",
            "word here\nno match\nanother word\n",
            "x\r\ny\r\n",
            " x\n  x\nq\n",
            "az\nqz\nbz\n",
            "ABC\nabc\nAbC\n",
            "12\n34a\n\n56\n",
            "no matches at all\nnothing here\n",
            "a\n\n\na\n",
        ];

        for pat in patterns {
            let re = regex::Regex::new(pat).expect("test pattern compiles");
            let matcher = SearchMatcher::Standard(re);
            for hay in haystacks {
                let index = LineIndex::new(hay);
                let opts = MatchOptions::default();

                // Lines that genuinely match, computed the slow, obvious way.
                let mut expected = Vec::new();
                for i in 0..index.line_count() {
                    if matcher
                        .is_match(index.line_text(i))
                        .expect("standard matcher cannot error")
                    {
                        expected.push(i);
                    }
                }

                // Declining is always allowed; it just means the full loop.
                if let Some(iter) = candidate_lines(hay, &index, &matcher, &opts) {
                    let proposed: Vec<usize> = iter.collect();
                    // Ascending and deduplicated, so `--max-count` can stop
                    // early and still return the first N matching lines.
                    assert!(
                        proposed.windows(2).all(|w| w[0] < w[1]),
                        "pattern {pat:?} on {hay:?}: candidates {proposed:?} not ascending"
                    );
                    for want in &expected {
                        assert!(
                            proposed.contains(want),
                            "pattern {pat:?} on {hay:?}: line {want} matches but was not proposed \
                             (candidates {proposed:?})"
                        );
                    }
                }

                // Whichever path it took, the answer must be the same.
                let hits = collect_hits(hay, &index, &matcher, &opts)
                    .expect("standard matcher cannot error");
                let got: Vec<usize> = hits.iter().map(|h| h.idx).collect();
                assert_eq!(got, expected, "pattern {pat:?} on {hay:?}");
            }
        }
    }

    #[test]
    fn prescan_respects_max_count_and_stop_on_nonmatch() {
        let hay = "hit\nhit\nmiss\nhit\n";
        let matcher = SearchMatcher::Standard(regex::Regex::new("hit").unwrap());
        let index = LineIndex::new(hay);

        let opts = MatchOptions {
            max_count: Some(2),
            ..MatchOptions::default()
        };
        let hits = collect_hits(hay, &index, &matcher, &opts).unwrap();
        assert_eq!(hits.iter().map(|h| h.idx).collect::<Vec<_>>(), vec![0, 1]);

        // `--stop-on-nonmatch` must see line 2 fail, which a candidate list
        // skips, so the prescan has to decline outright.
        let opts = MatchOptions {
            stop_on_nonmatch: true,
            ..MatchOptions::default()
        };
        assert!(candidate_lines(hay, &index, &matcher, &opts).is_none());
        let hits = collect_hits(hay, &index, &matcher, &opts).unwrap();
        assert_eq!(hits.iter().map(|h| h.idx).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn prescan_declines_when_every_line_matches_anyway() {
        // An empty-matching pattern matches at every offset, so a prescan would
        // propose every line at the cost of an extra pass.
        let hay = "a\nb\n";
        let index = LineIndex::new(hay);
        let matcher = SearchMatcher::Standard(regex::Regex::new("x*").unwrap());
        let opts = MatchOptions::default();
        assert!(candidate_lines(hay, &index, &matcher, &opts).is_none());
    }

    #[test]
    fn prescan_proposes_every_line_a_match_spans() {
        // `\s+` matches across the newline, so one match covers both lines and
        // both have to be offered.
        let hay = "a \n b\nzzz\n";
        let index = LineIndex::new(hay);
        let matcher = SearchMatcher::Standard(regex::Regex::new(r"\s+").unwrap());
        let opts = MatchOptions::default();
        let proposed: Vec<usize> = candidate_lines(hay, &index, &matcher, &opts)
            .expect("anchor-free and non-empty")
            .collect();
        assert!(proposed.contains(&0));
        assert!(proposed.contains(&1));
    }

    //
    // `fancy_regex`'s VM has no literal prefilter, so a pattern like
    // `(?<!//)Needle` is tried at every byte offset. Gating it on a relaxed,
    // linear-time approximation makes the common "this haystack has nothing"
    // case cheap. The prefilter is only ever allowed to answer *no*, so these
    // tests care about one thing above all: the answers must not change.
    // -----------------------------------------------------------------------

    fn pcre(pattern: &str) -> SearchMatcher {
        build_search_matcher(
            &[pattern.to_string()],
            &MatcherConfig {
                engine: RegexEngine::Pcre2,
                ..Default::default()
            },
        )
        .expect("pattern compiles")
    }

    #[test]
    fn a_lookaround_pattern_gets_a_prefilter() {
        match pcre(r"(?<!//)Needle") {
            SearchMatcher::Fancy { prefilter, .. } => {
                let re = prefilter.expect("lookaround is relaxable, so it is worth prefiltering");
                assert!(re.is_match("x Needle"), "the prefilter admits real matches");
                assert!(
                    re.is_match("//Needle"),
                    "and admits near-misses too - it only rules things out"
                );
                assert!(!re.is_match("nothing here"));
            }
            _ => panic!("expected a fancy matcher"),
        }
    }

    #[test]
    fn a_pattern_with_nothing_to_relax_gets_no_prefilter() {
        // `fancy_regex` already delegates these to `regex`; a second engine
        // would be pure overhead.
        for pattern in ["Needle", r"Needle\d+"] {
            match pcre(pattern) {
                SearchMatcher::Fancy { prefilter, .. } => {
                    assert!(prefilter.is_none(), "{pattern} needs no prefilter")
                }
                _ => panic!("expected a fancy matcher"),
            }
        }
    }

    #[test]
    fn an_unrelaxable_pattern_gets_no_prefilter() {
        match pcre(r"(Needle)\1") {
            SearchMatcher::Fancy { prefilter, .. } => assert!(prefilter.is_none()),
            _ => panic!("expected a fancy matcher"),
        }
    }

    #[test]
    fn the_prefilter_does_not_change_any_answer() {
        // Each case pairs a haystack with whether the real pattern matches it.
        // A prefilter that ever turned a `true` into a `false` would be silently
        // losing results, which is the failure mode worth guarding.
        let cases: [(&str, &[(&str, bool)]); 5] = [
            (
                r"(?<!//)Needle",
                &[
                    ("let x = Needle;", true),
                    ("//Needle", false),
                    ("nothing at all", false),
                    ("//Needle and Needle", true),
                ],
            ),
            (
                r"Needle(?=::)",
                &[("Needle::new", true), ("Needle.new", false)],
            ),
            (
                r"Needle(?!::)",
                &[("Needle::new", false), ("Needle.new", true)],
            ),
            (
                r"(?>Nee)dle",
                &[("Needle", true), ("Needle", true), ("nope", false)],
            ),
            (
                r"(?=.*Haystack)Needle",
                &[
                    ("Needle in a Haystack", true),
                    ("Needle alone", false),
                    ("Haystack alone", false),
                ],
            ),
        ];

        for (pattern, haystacks) in cases {
            let gated = pcre(pattern);
            // The same pattern with the prefilter removed, as the control.
            let ungated = match pcre(pattern) {
                SearchMatcher::Fancy { re, .. } => SearchMatcher::Fancy {
                    re,
                    prefilter: None,
                },
                other => other,
            };
            for (hay, expected) in haystacks {
                assert_eq!(
                    gated.is_match(hay).unwrap(),
                    *expected,
                    "{pattern:?} against {hay:?}"
                );
                assert_eq!(
                    gated.is_match(hay).unwrap(),
                    ungated.is_match(hay).unwrap(),
                    "prefilter changed is_match for {pattern:?} against {hay:?}"
                );
                assert_eq!(
                    gated.find_spans(hay).unwrap(),
                    ungated.find_spans(hay).unwrap(),
                    "prefilter changed find_spans for {pattern:?} against {hay:?}"
                );
                assert_eq!(
                    gated.find_first_span(hay).unwrap(),
                    ungated.find_first_span(hay).unwrap(),
                    "prefilter changed find_first_span for {pattern:?} against {hay:?}"
                );
            }
        }
    }

    /// The subset property, checked by brute force over a broad pattern corpus.
    ///
    /// This is the assumption the whole optimisation rests on, in both places it
    /// is used - candidate planning and the prefilter - so it is worth checking
    /// against the real backtracking engine rather than by inspection. For every
    /// pattern and every haystack: if `fancy_regex` matches, the relaxed pattern
    /// compiled with `regex` must match too. The converse is allowed and
    /// expected; that is what makes it a *relaxation*.
    #[test]
    fn relaxation_is_a_superset_of_every_pattern_it_accepts() {
        let patterns = [
            r"(?<!//)Needle",
            r"(?<=//)Needle",
            r"Needle(?=::)",
            r"Needle(?!::)",
            r"(?=.*Hay)Needle",
            r"(?!.*Hay)Needle",
            r"^(?=.*a)(?=.*b).*$",
            r"(?>Nee)dle",
            r"a(?>b|bc)d",
            r"(?:Nee(?=d))+dle",
            r"\bNeedle\b(?!s)",
            r"(?i)(?<!x)needle",
            r"[Nn](?<!x)eedle",
            r"(?<!\w)Needle",
            r"Needle(?=\s*[;,])",
            r"(?<name>Nee)(?=dle)dle",
            r"(a(?=b)|c)d",
            r"^(?!#)\s*Needle",
            r"Needle(?![)])",
            r"(?=[(])\(Needle",
            // Character-class forms: `(?=` and friends are ordinary members here.
            r"[](?=a)]n",
            r"[^](?<!a)]n",
            r"[[:digit:](?=a)]n",
            r"[]a]n",
            r"[[:alpha:]]eedle(?!s)",
            r"\p{L}+(?=dle)",
        ];
        let haystacks = [
            "",
            "Needle",
            "//Needle",
            " Needle ",
            "Needle::new()",
            "Needle.new()",
            "Needle in a Hay stack",
            "ab",
            "ba",
            "abd",
            "abcd",
            "cd",
            "needle",
            "xneedle",
            "Needles",
            "Needle;",
            "Needle , x",
            "# Needle",
            "  Needle",
            "Needle)",
            "(Needle",
            "NeeNeedle",
            "no match at all",
            "]Needle[",
            "]n",
            "an",
            "(n",
            "?n",
            "=n",
            "3n",
            "Needle(?=a)",
            "\u{3b1}\u{3b2}dle",
        ];

        for pattern in patterns {
            let fancy = fancy_regex::Regex::new(pattern).expect("pattern compiles as PCRE");
            let Some(relaxed_src) = tgrep_core::query::relax_for_indexing(pattern) else {
                continue; // bailing out is always safe
            };
            let Ok(relaxed) = regex::Regex::new(&relaxed_src) else {
                continue; // failing to compile degrades to a full scan
            };
            for hay in haystacks {
                if fancy.is_match(hay).unwrap() {
                    assert!(
                        relaxed.is_match(hay),
                        "relaxation NARROWED {pattern:?} to {relaxed_src:?}: it drops {hay:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_prefilter_inherits_the_case_insensitive_flag() {
        // A prefilter compiled without `-i` would reject `NEEDLE` outright and
        // hide a match the real pattern would have found.
        let matcher = build_search_matcher(
            &[r"(?<!//)Needle".to_string()],
            &MatcherConfig {
                engine: RegexEngine::Pcre2,
                case_insensitive: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matcher.is_match("let x = NEEDLE;").unwrap());
        assert!(!matcher.is_match("//NEEDLE").unwrap());
    }

    #[test]
    fn the_prefilter_survives_replacement() {
        let matcher = pcre(r"(?<!//)Needle");
        let (out, _) = matcher.replace_all("a Needle here", "Pin").unwrap();
        assert_eq!(out, "a Pin here");
        let (out, spans) = matcher.replace_all("no match here", "Pin").unwrap();
        assert_eq!(out, "no match here");
        assert!(spans.is_empty());
    }

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
                    &index.line_text(i),
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
        let hits = group_spans_by_line(&index, &[(0, 16)]);
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
        let hits = group_spans_by_line(&index, &[(0, 2), (1, 3)]);
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
        let hits = group_spans_by_line(&index, &[(0, 3), (8, 11)]);
        assert_eq!(
            hits,
            vec![LineHit {
                idx: 0,
                spans: vec![(0, 3), (8, 11)]
            }]
        );
    }
}
