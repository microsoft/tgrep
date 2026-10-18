//! Shared matching logic used by both the server (serve.rs) and the local
//! search path (search.rs). Owns pattern compilation, line/offset mapping and
//! the match-and-context traversal so the two callers cannot drift apart —
//! they differ only in how they render the resulting events.

use std::borrow::Cow;
use std::collections::BTreeMap;

use anyhow::Result;
use regex::RegexBuilder;
use regex_syntax::ast::{self, Ast, ClassSetItem};

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
        self.starts()
            .get(idx)
            .copied()
            .unwrap_or(self.content.len())
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
pub fn group_spans_by_line(index: &LineIndex<'_>, spans: &[(usize, usize)]) -> Vec<LineHit> {
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
                    Some(last) if span.0 < last.1 => last.1 = last.1.max(span.1),
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

/// A match's decoded byte range and its expanded `--replace` text.
type Expansion = (usize, usize, String);

/// One match inside a [`ReplaceBlock`]: where it was in the source, and where
/// its replacement landed in the rewritten text.
struct Placed {
    /// Absolute decoded byte range of the match.
    src: (usize, usize),
    /// Byte range of the replacement, relative to the start of the block.
    dst: (usize, usize),
}

impl Placed {
    /// How many bytes the replacement added to, or took from, the text.
    fn delta(&self) -> isize {
        (self.dst.1 - self.dst.0) as isize - (self.src.1 - self.src.0) as isize
    }
}

/// A run of whole lines that `-U --replace` rewrites as a unit.
///
/// ripgrep's multiline printer receives the lines a match covers, extended by
/// any later match that starts on the line the run ends on or the line right
/// after it. It replaces every match in that text and then prints the result
/// line by line, so a replacement can add, remove or join lines, and the
/// printed line numbers simply count up from the first line of the run.
struct ReplaceBlock {
    /// 0-based first and last source lines.
    first: usize,
    last: usize,
    /// Decoded byte offset where the block starts.
    start: usize,
    /// The block's lines with every match replaced.
    text: String,
    reps: Vec<Placed>,
}

impl ReplaceBlock {
    /// Group matches into blocks and rewrite each one.
    fn build(index: &LineIndex<'_>, content: &str, reps: Vec<Expansion>) -> Vec<ReplaceBlock> {
        let mut groups: Vec<(usize, usize, Vec<Expansion>)> = Vec::new();
        for (s, e, text) in reps {
            let first = index.line_of(s);
            let last = if e > s { index.line_of(e - 1) } else { first };
            match groups.last_mut() {
                Some((_, end, members)) if first <= *end + 1 => {
                    *end = (*end).max(last);
                    members.push((s, e, text));
                }
                _ => groups.push((first, last, vec![(s, e, text)])),
            }
        }
        groups
            .into_iter()
            .map(|(first, last, members)| {
                let start = index.line_start(first);
                let end = index.line_start(last + 1);
                let mut text = String::new();
                let mut reps = Vec::with_capacity(members.len());
                let mut pos = start;
                for (s, e, rep) in members {
                    text.push_str(&content[pos..s]);
                    let at = text.len();
                    text.push_str(&rep);
                    reps.push(Placed {
                        src: (s, e),
                        dst: (at, text.len()),
                    });
                    pos = e;
                }
                text.push_str(&content[pos..end]);
                ReplaceBlock {
                    first,
                    last,
                    start,
                    text,
                    reps,
                }
            })
            .collect()
    }

    /// The rewritten lines as `(start, content_end, end)`, stepped the way
    /// ripgrep's printer does: each line keeps its terminator, a final
    /// unterminated line only counts if it is not empty, and so an empty
    /// rewrite has no lines at all.
    fn lines(&self) -> Vec<(usize, usize, usize)> {
        let bytes = self.text.as_bytes();
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < bytes.len() {
            let end = memchr::memchr(b'\n', &bytes[pos..]).map_or(bytes.len(), |i| pos + i + 1);
            let mut content_end = end;
            if content_end > pos && bytes[content_end - 1] == b'\n' {
                content_end -= 1;
            }
            if content_end > pos && bytes[content_end - 1] == b'\r' {
                content_end -= 1;
            }
            out.push((pos, content_end, end));
            pos = end;
        }
        out
    }
}

/// Maps positions in a [`ReplaceBlock`]'s rewritten text back to decoded
/// source offsets, for positions visited in increasing order.
///
/// Reported offsets have to go back through the decoding repairs, which only
/// know source offsets. A position inside a replacement maps to the start of
/// the match it replaced; anything else maps to the source byte it was copied
/// from.
struct SourceCursor<'b> {
    block: &'b ReplaceBlock,
    next: usize,
    shift: isize,
}

impl<'b> SourceCursor<'b> {
    fn new(block: &'b ReplaceBlock) -> Self {
        Self {
            block,
            next: 0,
            shift: 0,
        }
    }

    fn at(&mut self, pos: usize) -> usize {
        while let Some(rep) = self.block.reps.get(self.next) {
            if rep.dst.0 <= pos && pos < rep.dst.1 {
                return rep.src.0;
            }
            if rep.dst.1 > pos {
                break;
            }
            self.shift += rep.delta();
            self.next += 1;
        }
        (self.block.start + pos).saturating_add_signed(-self.shift)
    }
}

/// [`group_spans_by_line`] for `--json --replace`, which reports each match's
/// replacement beside it. Nothing is merged, so every piece keeps its own
/// replacement: the line a match starts on carries the expansion, and any line
/// it continues onto carries an empty one.
fn group_replacements_by_line(
    index: &LineIndex<'_>,
    reps: Vec<Expansion>,
) -> (Vec<LineHit>, Vec<Vec<String>>) {
    let mut hits: Vec<LineHit> = Vec::new();
    let mut texts: Vec<Vec<String>> = Vec::new();
    for (s, e, text) in reps {
        let first = index.line_of(s);
        // An empty match sits entirely on its start line.
        let last = if e > s { index.line_of(e - 1) } else { first };
        let mut text = Some(text);
        for li in first..=last.min(index.line_count().saturating_sub(1)) {
            let ls = index.line_start(li);
            let le = index.line_end(li);
            if hits.last().map(|h| h.idx) != Some(li) {
                hits.push(LineHit {
                    idx: li,
                    spans: Vec::new(),
                });
                texts.push(Vec::new());
            }
            if let (Some(hit), Some(line_texts)) = (hits.last_mut(), texts.last_mut()) {
                hit.spans.push((s.clamp(ls, le) - ls, e.clamp(ls, le) - ls));
                line_texts.push(text.take().unwrap_or_default());
            }
        }
    }
    (hits, texts)
}

/// The replacement of each of a line's `spans`, for `--json --replace`.
fn line_replacements(
    matcher: &SearchMatcher,
    line: &str,
    spans: &[(usize, usize)],
    replacement: &str,
) -> Result<Vec<String>> {
    let mut expansions = matcher.expansions(line, replacement)?.into_iter();
    Ok(spans
        .iter()
        .map(|&(s, e)| {
            expansions
                .by_ref()
                .find(|&(es, ee, _)| (es, ee) == (s, e))
                .map_or_else(String::new, |(_, _, text)| text)
        })
        .collect())
}

/// The compiled pattern. `regex` handles almost everything and is linear-time;
/// `fancy_regex` is the fallback for backreferences and lookaround.
pub enum SearchMatcher {
    Standard {
        re: regex::Regex,
        /// `--multiline-dotall`, which `re` was compiled with but does not
        /// expose, and which decides whether `.` can match a line terminator.
        dotall: bool,
        /// [`can_match_newline`](Self::can_match_newline), worked out the
        /// first time it is asked. It depends only on the pattern, so every
        /// later file, on any thread, reuses it instead of parsing again.
        newline: std::sync::OnceLock<bool>,
    },
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
    /// The linear-time matcher for `re`, which was compiled with `dotall`.
    pub fn standard(re: regex::Regex, dotall: bool) -> Self {
        Self::Standard {
            re,
            dotall,
            newline: std::sync::OnceLock::new(),
        }
    }

    pub fn is_standard(&self) -> bool {
        matches!(self, Self::Standard { .. })
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
            Self::Standard { re, .. } => Ok(re.is_match(hay)),
            Self::Fancy { re, prefilter } => {
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
            Self::Standard { re, .. } => {
                for m in re.find_iter(hay) {
                    spans.push((m.start(), m.end()));
                }
            }
            Self::Fancy { re, prefilter } => {
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
            Self::Standard { re, .. } => Ok(re.find(hay).map(|m| (m.start(), m.end()))),
            Self::Fancy { re, prefilter } => {
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
    fn expansions(&self, hay: &str, replacement: &str) -> Result<Vec<Expansion>> {
        self.expansions_upto(hay, replacement, usize::MAX)
    }

    /// [`expansions`](Self::expansions) for only the first `limit` matches,
    /// which are the same matches [`find_spans`](Self::find_spans) reports
    /// first.
    fn expansions_upto(
        &self,
        hay: &str,
        replacement: &str,
        limit: usize,
    ) -> Result<Vec<Expansion>> {
        let mut out = Vec::new();
        match self {
            Self::Standard { re, .. } => {
                for caps in re.captures_iter(hay).take(limit) {
                    let m = caps.get(0).expect("group 0 always participates");
                    let mut dst = String::new();
                    caps.expand(replacement, &mut dst);
                    out.push((m.start(), m.end(), dst));
                }
            }
            Self::Fancy { re, prefilter } => {
                if Self::ruled_out(prefilter, hay) {
                    return Ok(out);
                }
                for caps in re.captures_iter(hay).take(limit) {
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

    /// Whether a match could contain a line terminator.
    ///
    /// ripgrep decides this statically from the pattern, not from what it
    /// actually matched, and only a pattern that can cross lines gets its `-U`
    /// results printed as blocks. Like ripgrep, line anchors count as able to
    /// match `\n`. The backtracking engine is never analysed, which ripgrep's
    /// PCRE2 matcher does not do either, so it always counts.
    pub fn can_match_newline(&self) -> bool {
        let Self::Standard {
            re,
            dotall,
            newline,
        } = self
        else {
            return true;
        };
        *newline.get_or_init(|| {
            regex_syntax::ParserBuilder::new()
                .nest_limit(250)
                .utf8(false)
                .multi_line(true)
                .dot_matches_new_line(*dotall)
                .build()
                .parse(re.as_str())
                .map_or(true, |hir| hir_can_match_newline(&hir))
        })
    }
}

/// Whether any match of `hir` can include `\n`, following the byte analysis
/// ripgrep runs to choose between line and multiline printing.
fn hir_can_match_newline(hir: &regex_syntax::hir::Hir) -> bool {
    use regex_syntax::hir::{Class, HirKind, Look};
    match hir.kind() {
        HirKind::Empty => false,
        HirKind::Literal(lit) => lit.0.contains(&b'\n'),
        HirKind::Class(Class::Unicode(cls)) => cls
            .ranges()
            .iter()
            .any(|r| r.start() <= '\n' && '\n' <= r.end()),
        HirKind::Class(Class::Bytes(cls)) => cls
            .ranges()
            .iter()
            .any(|r| r.start() <= b'\n' && b'\n' <= r.end()),
        HirKind::Look(look) => matches!(
            look,
            Look::Start | Look::End | Look::StartLF | Look::EndLF | Look::StartCRLF | Look::EndCRLF
        ),
        HirKind::Repetition(rep) => hir_can_match_newline(&rep.sub),
        HirKind::Capture(cap) => hir_can_match_newline(&cap.sub),
        HirKind::Concat(subs) | HirKind::Alternation(subs) => {
            subs.iter().any(hir_can_match_newline)
        }
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
    if cfg.engine == RegexEngine::Pcre2 {
        return build_fancy(patterns, cfg).map_err(|e| {
            anyhow::anyhow!("regex error: PCRE-style engine rejected the pattern: {e}")
        });
    }

    let combined = combine_patterns(patterns, cfg, false);
    match build_regex(&combined, cfg) {
        Ok(re) => Ok(SearchMatcher::standard(re, cfg.dotall)),
        // Exceeding `--regex-size-limit`/`--dfa-size-limit` is a resource error,
        // not an unsupported construct. Retrying on the backtracking engine
        // would silently ignore the limit the user asked for.
        Err(e @ regex::Error::CompiledTooBig(_)) => Err(anyhow::anyhow!("regex error: {e}")),
        Err(regex_err) if !cfg.fixed_string && cfg.engine == RegexEngine::Auto => {
            build_fancy(patterns, cfg).map_err(|fancy_err| {
                anyhow::anyhow!("regex error: {regex_err}; PCRE-style fallback failed: {fancy_err}")
            })
        }
        Err(regex_err) => Err(anyhow::anyhow!("regex error: {regex_err}")),
    }
}

fn build_fancy(patterns: &[String], cfg: &MatcherConfig) -> Result<SearchMatcher> {
    let combined = combine_patterns(patterns, cfg, true);
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
    let pattern = wrap(&combined);
    let re = fancy_regex::Regex::new(&pattern).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(SearchMatcher::Fancy {
        re,
        prefilter: build_fancy_prefilter(&combined, &wrap),
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

/// An empty character-class intersection matches nothing in either engine.
pub const NEVER_MATCH_PATTERN: &str = "[a&&b]";

fn combine_patterns(patterns: &[String], cfg: &MatcherConfig, fancy: bool) -> String {
    if patterns.is_empty() {
        return NEVER_MATCH_PATTERN.to_string();
    }
    let wrap = |p: &String| {
        let p = if cfg.fixed_string {
            regex::escape(p)
        } else {
            p.clone()
        };
        if cfg.line_regexp {
            format!(r"^(?:{p})$")
        } else if cfg.word_boundary {
            if fancy {
                // fancy-regex does not support half-boundary syntax.
                let word = if cfg.no_unicode {
                    "[a-zA-Z0-9_]"
                } else {
                    r"\w"
                };
                format!(r"(?<!{word})(?:{p})(?!{word})")
            } else {
                format!(r"\b{{start-half}}(?:{p})\b{{end-half}}")
            }
        } else if patterns.len() > 1 {
            // Bare inline flags must not reach a different pattern's branch.
            format!("(?:{p})")
        } else {
            p
        }
    };

    if patterns.len() == 1 {
        wrap(&patterns[0])
    } else {
        let parts: Vec<String> = patterns.iter().map(wrap).collect();
        format!("(?:{})", parts.join("|"))
    }
}

/// Smart case considers literals, not uppercase letters in regex syntax.
pub fn smart_case_insensitive(patterns: &[String], fixed_string: bool) -> Result<bool> {
    let mut case = SmartCase::default();
    for pattern in patterns {
        if fixed_string {
            for c in pattern.chars() {
                case.literal(c);
            }
        } else if case.add_ast(pattern).is_err() {
            // Lookaround and backreferences need the same fallback as matching.
            case.add_fancy(pattern)
                .map_err(|error| anyhow::anyhow!("regex error: {error}"))?;
        }
    }
    Ok(case.has_literal && !case.has_uppercase)
}

#[derive(Default)]
struct SmartCase {
    has_literal: bool,
    has_uppercase: bool,
}

impl SmartCase {
    fn literal(&mut self, c: char) {
        self.has_literal = true;
        self.has_uppercase |= c.is_uppercase();
    }

    fn add_ast(&mut self, pattern: &str) -> Result<()> {
        let ast = ast::parse::ParserBuilder::new()
            .nest_limit(250)
            .build()
            .parse(pattern)?;
        ast::visit(&ast, self)?;
        Ok(())
    }

    fn add_fancy(&mut self, pattern: &str) -> Result<()> {
        use fancy_regex::Expr;

        let tree = Expr::parse_tree(pattern)?;
        let mut pending = vec![&tree.expr];
        while let Some(expr) = pending.pop() {
            match expr {
                Expr::Literal { val, .. } => {
                    for c in val.chars() {
                        self.literal(c);
                    }
                }
                Expr::Concat(children) | Expr::Alt(children) => pending.extend(children),
                Expr::Group(child)
                | Expr::LookAround(child, _)
                | Expr::AtomicGroup(child)
                | Expr::Repeat { child, .. } => pending.push(child),
                Expr::Delegate { inner, .. } => self.add_ast(inner)?,
                Expr::Conditional {
                    condition,
                    true_branch,
                    false_branch,
                } => pending.extend([
                    condition.as_ref(),
                    true_branch.as_ref(),
                    false_branch.as_ref(),
                ]),
                Expr::Empty
                | Expr::Any { .. }
                | Expr::Assertion(_)
                | Expr::Backref(_)
                | Expr::KeepOut
                | Expr::ContinueFromPreviousMatchEnd
                | Expr::BackrefExistsCondition(_) => {}
            }
        }
        Ok(())
    }
}

impl ast::Visitor for &mut SmartCase {
    type Output = ();
    type Err = std::convert::Infallible;

    fn finish(self) -> std::result::Result<(), Self::Err> {
        Ok(())
    }

    fn visit_pre(&mut self, ast: &Ast) -> std::result::Result<(), Self::Err> {
        if let Ast::Literal(literal) = ast {
            self.literal(literal.c);
        }
        Ok(())
    }

    fn visit_class_set_item_pre(
        &mut self,
        item: &ClassSetItem,
    ) -> std::result::Result<(), Self::Err> {
        match item {
            ClassSetItem::Literal(literal) => self.literal(literal.c),
            ClassSetItem::Range(range) => {
                self.literal(range.start.c);
                self.literal(range.end.c);
            }
            _ => {}
        }
        Ok(())
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
    /// Render `-U --replace` the way ripgrep's multiline printer does: rewrite
    /// every match inside the block of lines it covers, then print the result
    /// line by line. Only text output prints those blocks; JSON keeps its
    /// per-line events and counts or file lists never render lines.
    pub replace_blocks: bool,
    /// `--json --replace`: keep the source line and spans, and report each
    /// match's expanded replacement alongside them instead of rewriting the
    /// line.
    pub report_replacements: bool,
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
        /// 1-based first and last source lines of the block a `-U --replace`
        /// row was rewritten from. The rewritten text can have more or fewer
        /// lines than the source, so context gaps are measured from these
        /// rather than from `line_number`.
        block: Option<(usize, usize)>,
        /// The block matched, but its replacement left no line to print. The
        /// row still opens the file's heading and advances the context
        /// position, as ripgrep does, and keeps the match visible to callers
        /// that only learn about matches from rows.
        hidden: bool,
        /// `--json --replace`: the expanded replacement of each entry in
        /// `spans`, which then still describe the source text.
        replacements: Option<Vec<String>>,
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
    match_totals: (u64, u64),
    /// `-U --replace` blocks, which replace `hits` when rendering. Empty when
    /// matches print line by line.
    blocks: Vec<ReplaceBlock>,
    /// `--json --replace`: the replacement of every span in `hits`, parallel
    /// to it. Empty otherwise.
    replacements: Vec<Vec<String>>,
}

impl<'a> FileMatches<'a> {
    pub fn find(content: &'a str, matcher: &SearchMatcher, opts: &MatchOptions) -> Result<Self> {
        let index = LineIndex::new(content);
        let mut blocks = Vec::new();
        let mut replacements = Vec::new();
        let (hits, match_totals) = if opts.max_count == Some(0) {
            (Vec::new(), (0, 0))
        } else if opts.multiline && !opts.invert_match {
            // Count original matches after the search limit, but before
            // splitting, merging or clipping their spans for presentation.
            let mut spans = matcher.find_spans(content)?;
            // EOF after a terminator is not another physical line. Keeping its
            // empty match would duplicate the last line's end-of-line match.
            if (content.is_empty() || content.ends_with('\n'))
                && spans.last() == Some(&(content.len(), content.len()))
            {
                spans.pop();
            }
            let spans = limit_to_line_blocks(&index, &spans, opts.max_count);
            let hits = group_spans_by_line(&index, &spans);
            let totals = (spans.len() as u64, hits.len() as u64);
            // `captures_iter` finds the same matches as `find_spans`, and
            // `spans` only ever drops a suffix of them, so the first
            // `spans.len()` expansions line up with `spans` one for one.
            let hits = match opts.replace.as_deref() {
                Some(rep) if opts.report_replacements => {
                    let reps = matcher.expansions_upto(content, rep, spans.len())?;
                    let (hits, texts) = group_replacements_by_line(&index, reps);
                    replacements = texts;
                    hits
                }
                Some(rep)
                    if opts.replace_blocks && !spans.is_empty() && matcher.can_match_newline() =>
                {
                    let reps = matcher.expansions_upto(content, rep, spans.len())?;
                    blocks = ReplaceBlock::build(&index, content, reps);
                    hits
                }
                _ if opts.vimgrep => {
                    group_spans_by_line(&index, &clip_spans_to_start_line(&index, &spans))
                }
                _ => hits,
            };
            (hits, totals)
        } else {
            let hits = collect_hits(content, &index, matcher, opts)?;
            let totals = (
                hits.iter().map(|h| h.spans.len() as u64).sum(),
                hits.len() as u64,
            );
            if let Some(rep) = opts.replace.as_deref().filter(|_| opts.report_replacements) {
                replacements = hits
                    .iter()
                    .map(|hit| {
                        line_replacements(matcher, index.line_text(hit.idx), &hit.spans, rep)
                    })
                    .collect::<Result<_>>()?;
            }
            (hits, totals)
        };
        Ok(Self {
            index,
            hits,
            match_totals,
            blocks,
            replacements,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// Original regex matches and selected physical lines, before rendering.
    /// Inverted lines contribute no regex matches. Requires `all_spans` for
    /// complete match counts in line-oriented mode.
    pub fn match_totals(&self) -> (u64, u64) {
        self.match_totals
    }

    /// Number of matching lines, which is what `-c/--count` reports.
    pub fn matched_lines(&self) -> usize {
        self.hits.len()
    }

    /// Total number of individual matches, which is what `--count-matches`
    /// reports. Inverted matches have no spans but still count as one.
    pub fn match_count(&self) -> usize {
        if self.match_totals.0 == 0 {
            self.hits.len()
        } else {
            self.match_totals.0 as usize
        }
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
        if !self.blocks.is_empty() {
            return self.for_each_block(opts, &mut on_emit);
        }

        let emit_hit = |hit: &LineHit,
                        on_emit: &mut dyn FnMut(Emit<'a>) -> std::result::Result<(), E>|
         -> std::result::Result<(), E> {
            let line = self.index.line_text(hit.idx);
            let offset = self.index.line_start(hit.idx);

            // `--json --replace` describes the source line and reports what
            // each match would be replaced with, rather than rewriting it.
            let replacements = self
                .hits
                .binary_search_by_key(&hit.idx, |h| h.idx)
                .ok()
                .and_then(|i| self.replacements.get(i));
            if let Some(texts) = replacements.filter(|_| !hit.spans.is_empty()) {
                return on_emit(Emit::Match {
                    line_number: hit.idx + 1,
                    content: Cow::Borrowed(line),
                    columns: hit.spans.iter().map(|&(s, _)| s + 1).collect(),
                    spans: hit.spans.clone(),
                    absolute_offset: offset,
                    line_offset: offset,
                    column_shifts: Vec::new(),
                    offset_shift: 0,
                    terminator_len: self.index.line_terminator_len(hit.idx),
                    block: None,
                    hidden: false,
                    replacements: Some(texts.clone()),
                });
            }

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
                            block: None,
                            hidden: false,
                            replacements: None,
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
                        block: None,
                        hidden: false,
                        replacements: None,
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
                block: None,
                hidden: false,
                replacements: None,
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

    fn context_line(&self, li: usize) -> Emit<'a> {
        Emit::Context {
            line_number: li + 1,
            content: self.index.line_text(li),
            absolute_offset: self.index.line_start(li),
            terminator_len: self.index.line_terminator_len(li),
        }
    }

    /// [`for_each`](Self::for_each) for `-U --replace` blocks. Context is
    /// measured from the source lines each block covers, however many lines
    /// its rewrite prints.
    fn for_each_block<E>(
        &self,
        opts: &MatchOptions,
        on_emit: &mut dyn FnMut(Emit<'a>) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        if opts.passthru {
            let mut li = 0;
            for block in &self.blocks {
                while li < block.first {
                    on_emit(self.context_line(li))?;
                    li += 1;
                }
                emit_block(block, opts, on_emit)?;
                li = block.last + 1;
            }
            while li < self.index.line_count() {
                on_emit(self.context_line(li))?;
                li += 1;
            }
            return Ok(());
        }

        if opts.before_context == 0 && opts.after_context == 0 {
            for block in &self.blocks {
                emit_block(block, opts, on_emit)?;
            }
            return Ok(());
        }

        let block_lines: Vec<usize> = self.blocks.iter().flat_map(|b| b.first..=b.last).collect();
        let printed = expand_context_window(
            &block_lines,
            self.index.line_count(),
            opts.before_context,
            opts.after_context,
        );
        let mut blocks = self.blocks.iter().peekable();
        let mut inside_until = None;
        for &li in &printed {
            if inside_until.is_some_and(|last| li <= last) {
                continue;
            }
            if let Some(block) = blocks.next_if(|b| b.first == li) {
                emit_block(block, opts, on_emit)?;
                inside_until = Some(block.last);
            } else {
                on_emit(self.context_line(li))?;
            }
        }
        Ok(())
    }
}

/// The rows ripgrep prints for one `-U --replace` block.
///
/// Plain output prints every rewritten line, each with the column of the
/// block's first replacement. `-o` prints the part of each replacement that
/// falls on each line, and `--vimgrep` the first line each replacement touches.
/// Positions come out in the rewritten text, as ripgrep reports them, by way of
/// a source position plus a shift, which lets the caller still map the source
/// position back through any decoding repairs.
fn emit_block<'a, E>(
    block: &ReplaceBlock,
    opts: &MatchOptions,
    on_emit: &mut dyn FnMut(Emit<'a>) -> std::result::Result<(), E>,
) -> std::result::Result<(), E> {
    let lines = block.lines();
    let range = Some((block.first + 1, block.last + 1));
    // The part of a replacement inside `ls..te`, if any.
    let piece = |rep: &Placed, ls: usize, te: usize| {
        let (a, b) = (rep.dst.0.max(ls), rep.dst.1.min(te));
        (a < b).then_some((a, b))
    };
    let mut rows = 0usize;

    if opts.only_matching {
        let mut shift = 0isize;
        let shifts: Vec<isize> = block
            .reps
            .iter()
            .map(|rep| {
                let before = shift;
                shift += rep.delta();
                before
            })
            .collect();
        for (k, &(ls, te, _)) in lines.iter().enumerate() {
            for (rep, &shift) in block.reps.iter().zip(&shifts) {
                let Some((a, b)) = piece(rep, ls, te) else {
                    continue;
                };
                on_emit(Emit::Match {
                    line_number: block.first + 1 + k,
                    content: Cow::Owned(block.text[a..b].to_owned()),
                    columns: vec![rep.src.0 - block.start + 1],
                    spans: vec![(0, b - a)],
                    absolute_offset: rep.src.0,
                    line_offset: block.start,
                    column_shifts: vec![shift],
                    offset_shift: shift,
                    terminator_len: 0,
                    block: range,
                    hidden: false,
                    replacements: None,
                })?;
                rows += 1;
            }
        }
    } else if opts.vimgrep {
        let mut cursor = SourceCursor::new(block);
        let mut k = 0;
        for rep in &block.reps {
            while lines.get(k).is_some_and(|&(_, _, le)| le <= rep.dst.0) {
                k += 1;
            }
            let Some(&(ls, te, le)) = lines.get(k) else {
                break;
            };
            // An empty replacement right at the start of a line is on no line.
            if ls >= rep.dst.1 {
                continue;
            }
            let source_line = cursor.at(ls);
            let column = rep.src.0.saturating_sub(source_line);
            on_emit(Emit::Match {
                line_number: block.first + 1 + k,
                content: Cow::Owned(block.text[ls..te].to_owned()),
                columns: vec![column + 1],
                spans: piece(rep, ls, te)
                    .map(|(a, b)| (a - ls, b - ls))
                    .into_iter()
                    .collect(),
                absolute_offset: source_line,
                line_offset: source_line,
                column_shifts: vec![(rep.dst.0 - ls) as isize - column as isize],
                offset_shift: (block.start + ls) as isize - source_line as isize,
                terminator_len: le - te,
                block: range,
                hidden: false,
                replacements: None,
            })?;
            rows += 1;
        }
    } else {
        let mut cursor = SourceCursor::new(block);
        let column = block
            .reps
            .first()
            .map_or(1, |rep| rep.src.0 - block.start + 1);
        for (k, &(ls, te, le)) in lines.iter().enumerate() {
            let source_line = cursor.at(ls);
            on_emit(Emit::Match {
                line_number: block.first + 1 + k,
                content: Cow::Owned(block.text[ls..te].to_owned()),
                columns: vec![column],
                spans: block
                    .reps
                    .iter()
                    .filter_map(|rep| piece(rep, ls, te))
                    .map(|(a, b)| (a - ls, b - ls))
                    .collect(),
                absolute_offset: source_line,
                line_offset: block.start,
                column_shifts: Vec::new(),
                offset_shift: (block.start + ls) as isize - source_line as isize,
                terminator_len: le - te,
                block: range,
                hidden: false,
                replacements: None,
            })?;
            rows += 1;
        }
    }

    if rows == 0 {
        on_emit(Emit::Match {
            line_number: block.first + 1,
            content: Cow::Borrowed(""),
            columns: Vec::new(),
            spans: Vec::new(),
            absolute_offset: block.start,
            line_offset: block.start,
            column_shifts: Vec::new(),
            offset_shift: 0,
            terminator_len: 0,
            block: range,
            hidden: true,
            replacements: None,
        })?;
    }
    Ok(())
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
/// cross `\n` (via `\s`, a negated class, or `-s`) can span several.
///
/// A match covers the lines from its start offset through its end offset. A
/// zero-length match, like those of `\b` and `\<`, covers only the line it sits
/// on. A match ending just past a `\n` also covers the next line: `find_iter`
/// never yields an empty match where the previous match ended, so it may have
/// hidden one at that line's start (`\<` in `\s|\<`). Under those rules every
/// line that really matches is covered, and the per-line matcher still has the
/// final say, so extra candidates cost time and never accuracy.
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
    let SearchMatcher::Standard { re, .. } = matcher else {
        return None;
    };
    if !pattern_is_anchor_free(re.as_str()) {
        return None;
    }
    // A pattern that matches the empty string matches at every offset, so the
    // prescan would yield every line and the real loop is cheaper. Patterns
    // like `\b` still match empty at some offsets; the spans below allow that.
    if re.is_match("") {
        return None;
    }

    // Each match starts at or after the previous one's end, so lines arrive in
    // non-decreasing order and dropping repeats leaves them strictly ascending.
    let mut last: Option<usize> = None;
    Some(
        re.find_iter(content)
            .flat_map(move |m| index.line_of(m.start())..=index.line_of(m.end()))
            .filter(move |&li| {
                let fresh = last != Some(li);
                if fresh {
                    last = Some(li);
                }
                fresh
            }),
    )
}

/// Find selected lines and their spans in line-oriented or inverted mode.
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

    #[test]
    fn smart_case_distinguishes_literals_from_regex_syntax() {
        for (pattern, insensitive) in [
            ("abc", true),
            ("ABC", false),
            (r"a\Sc", true),
            (r"a\Wc", true),
            (r"a\Dc", true),
            (r"a\pLc", true),
            (r"\w+", false),
            (r"\p{Lu}", false),
            ("", false),
            ("^$", false),
            ("[[:upper:]]", false),
            ("[a-z]", true),
            ("[a-zA]", false),
            ("[A-Z]", false),
            (r"\x41", false),
            (r"\x61", true),
            ("\u{e9}", true),
            ("\u{c9}", false),
            (r"\u{00C9}", false),
            ("(?P<UPPER>abc)", true),
            ("(?U)abc", true),
            ("(?x) abc # UPPER", true),
            ("abc(?=d)", true),
            ("abc(?=D)", false),
            ("(?<=A)abc", false),
            ("(?>abc)", true),
            ("(?=a)[a-z]+", true),
            ("(?=a)[A-Z]+", false),
            (r"(abc)\1", true),
            (r"(ABC)\1", false),
        ] {
            assert_eq!(
                smart_case_insensitive(&[pattern.to_string()], false).unwrap(),
                insensitive,
                "{pattern:?}"
            );
        }
        assert!(smart_case_insensitive(&["abc".into(), r"\S".into()], false).unwrap());
        assert!(!smart_case_insensitive(&["abc".into(), "XYZ".into()], false).unwrap());
        assert!(!smart_case_insensitive(&[r"a\Sc".into()], true).unwrap());
        assert!(smart_case_insensitive(&[r"a\sc".into()], true).unwrap());
        assert!(smart_case_insensitive(&["abc".into(), "[".into()], false).is_err());
    }

    #[test]
    fn an_empty_pattern_set_never_matches_in_either_engine() {
        for engine in [RegexEngine::Auto, RegexEngine::Default, RegexEngine::Pcre2] {
            for fixed_string in [false, true] {
                let matcher = build_search_matcher(
                    &[],
                    &MatcherConfig {
                        engine,
                        fixed_string,
                        ..Default::default()
                    },
                )
                .unwrap();
                for content in ["", "a", "b", "anything", NEVER_MATCH_PATTERN] {
                    assert!(
                        !matcher.is_match(content).unwrap(),
                        "{engine:?}: {content:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn automatic_fancy_fallback_preserves_half_word_boundaries() {
        let matcher = build_search_matcher(
            &[r"a-(?= x)".into()],
            &MatcherConfig {
                word_boundary: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(matcher.find_spans("a- x").unwrap(), vec![(0, 2)]);
        assert!(!matcher.is_match("xa- x").unwrap());
    }

    #[test]
    fn stats_keep_original_multiline_matches_and_physical_lines() {
        let matcher = SearchMatcher::standard(regex::Regex::new(r"hello\nworld").unwrap(), false);
        for vimgrep in [false, true] {
            let found = FileMatches::find(
                "hello\nworld\n",
                &matcher,
                &MatchOptions {
                    multiline: true,
                    vimgrep,
                    all_spans: true,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(found.match_totals(), (1, 2));
            assert_eq!(found.match_count(), 1);
            assert_eq!(found.matched_lines(), if vimgrep { 1 } else { 2 });
        }
    }

    #[test]
    fn multiline_empty_matches_do_not_create_a_phantom_trailing_line() {
        let matcher = SearchMatcher::standard(regex::Regex::new("").unwrap(), false);
        for (content, matches, lines) in [("", 0, 0), ("\n", 1, 1), ("aa\n", 3, 1), ("aa", 3, 1)] {
            for max_count in [None, Some(1)] {
                let found = FileMatches::find(
                    content,
                    &matcher,
                    &MatchOptions {
                        multiline: true,
                        max_count,
                        ..Default::default()
                    },
                )
                .unwrap();
                assert_eq!(found.match_totals(), (matches, lines), "{content:?}");
                assert_eq!(found.match_count() as u64, matches, "{content:?}");
                assert_eq!(found.matched_lines() as u64, lines, "{content:?}");
            }
        }
    }

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
            r"\b",
            r"\<",
            r"\>",
            r"\b{start}",
            r"\b{end}",
            r"\s|\<",
            r"\W|\<",
            r"\b|zzz",
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
            "x\nbbb",
            "!!\nbbb\n",
            " aaa\nbbb\n",
        ];

        for pat in patterns {
            let re = regex::Regex::new(pat).expect("test pattern compiles");
            let matcher = SearchMatcher::standard(re, false);
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
        let matcher = SearchMatcher::standard(regex::Regex::new("hit").unwrap(), false);
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
        let matcher = SearchMatcher::standard(regex::Regex::new("x*").unwrap(), false);
        let opts = MatchOptions::default();
        assert!(candidate_lines(hay, &index, &matcher, &opts).is_none());
    }

    #[test]
    fn prescan_proposes_every_line_a_match_spans() {
        // `\s+` matches across the newline, so one match covers both lines and
        // both have to be offered.
        let hay = "a \n b\nzzz\n";
        let index = LineIndex::new(hay);
        let matcher = SearchMatcher::standard(regex::Regex::new(r"\s+").unwrap(), false);
        let opts = MatchOptions::default();
        let proposed: Vec<usize> = candidate_lines(hay, &index, &matcher, &opts)
            .expect("anchor-free and non-empty")
            .collect();
        assert!(proposed.contains(&0));
        assert!(proposed.contains(&1));
    }

    #[test]
    fn prescan_covers_zero_length_matches() {
        // `\b` and `\<` match empty at a word's edge but never match `""` on
        // their own, so they take the prescan. An empty match at offset 0 has
        // no byte before its end, and one at a later line's start has that byte
        // on the previous line. `find_iter` also skips the empty `\<` match
        // right after the `\n` that `\s` matched in `\s|\<`, so only where that
        // `\n` match ends puts line 1 forward.
        let opts = MatchOptions::default();
        for (pattern, hay, want) in [
            (r"\b", "aaa\nbbb\n", &[0, 1][..]),
            (r"\<", "!!\nbbb\n", &[1][..]),
            (r"\s|\<", "x\nbbb", &[0, 1][..]),
        ] {
            let index = LineIndex::new(hay);
            let matcher = SearchMatcher::standard(regex::Regex::new(pattern).unwrap(), false);
            assert!(
                candidate_lines(hay, &index, &matcher, &opts).is_some(),
                "{pattern:?} should take the prescan"
            );
            let hits = collect_hits(hay, &index, &matcher, &opts).unwrap();
            let got: Vec<usize> = hits.iter().map(|h| h.idx).collect();
            assert_eq!(got, want, "{pattern:?} on {hay:?}");
        }
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
            // A dropped lookaround must not fuse its neighbours into a repeat.
            r"Nee{(?=2)2}",
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
            "Nee{2}",
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
                assert_eq!(&index.line_text(i), want, "line {i} of {content:?}");
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

    #[test]
    fn group_spans_keeps_touching_ranges_apart() {
        let index = LineIndex::new("aa\n");
        let hits = group_spans_by_line(&index, &[(1, 2), (0, 1)]);
        assert_eq!(
            hits,
            vec![LineHit {
                idx: 0,
                spans: vec![(0, 1), (1, 2)]
            }]
        );
    }

    /// The rows a `-U` search emits, in the shape of `rg -n --column`: a match
    /// is `line:column:text`, a context line `line-text`, and a block whose
    /// rewrite printed nothing `line:hidden`.
    fn multiline_rows(
        content: &str,
        pattern: &str,
        replace: &str,
        tweak: impl FnOnce(&mut MatchOptions),
    ) -> Vec<String> {
        let matcher = build_search_matcher(
            &[pattern.to_string()],
            &MatcherConfig {
                multiline: true,
                ..Default::default()
            },
        )
        .unwrap();
        let mut opts = MatchOptions {
            multiline: true,
            replace: Some(replace.to_string()),
            replace_blocks: true,
            all_spans: true,
            ..Default::default()
        };
        tweak(&mut opts);
        let found = FileMatches::find(content, &matcher, &opts).unwrap();
        let mut rows = Vec::new();
        found
            .for_each(&opts, &matcher, |emit| -> Result<()> {
                rows.push(match emit {
                    Emit::Match {
                        hidden: true,
                        line_number,
                        ..
                    } => format!("{line_number}:hidden"),
                    Emit::Match {
                        line_number,
                        content,
                        columns,
                        column_shifts,
                        ..
                    } => {
                        let column =
                            columns[0] as isize + column_shifts.first().copied().unwrap_or(0);
                        format!("{line_number}:{column}:{content}")
                    }
                    Emit::Context {
                        line_number,
                        content,
                        ..
                    } => format!("{line_number}-{content}"),
                });
                Ok(())
            })
            .unwrap();
        rows
    }

    #[test]
    fn newline_capability_follows_the_pattern_not_the_match() {
        for (pattern, dotall, expected) in [
            (r"hello\nworld", false, true),
            (r"l\s?", false, true),
            (r"[^a]", false, true),
            ("$", false, true),
            ("^foo", false, true),
            (r"(foo)+\n?", false, true),
            ("l.", false, false),
            ("l.", true, true),
            (r"\bfoo\b", false, false),
            ("foo|bar", false, false),
        ] {
            let matcher = build_search_matcher(
                &[pattern.to_string()],
                &MatcherConfig {
                    multiline: true,
                    dotall,
                    ..Default::default()
                },
            )
            .unwrap();
            // Worked out once, on first use, from the `dotall` the pattern was
            // compiled with.
            let SearchMatcher::Standard { newline, .. } = &matcher else {
                panic!("{pattern:?} should use the linear-time engine");
            };
            assert_eq!(newline.get(), None, "{pattern:?} dotall={dotall}");
            for _ in 0..2 {
                assert_eq!(
                    matcher.can_match_newline(),
                    expected,
                    "{pattern:?} dotall={dotall}"
                );
            }
            assert_eq!(
                newline.get(),
                Some(&expected),
                "{pattern:?} dotall={dotall}"
            );
        }
        // Lookaround is never analysed, so it always counts.
        let fancy = build_search_matcher(
            &["l(?=o)".to_string()],
            &MatcherConfig {
                multiline: true,
                engine: RegexEngine::Pcre2,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(fancy.can_match_newline());
    }

    #[test]
    fn multiline_replace_rewrites_whole_blocks() {
        // Expected rows are ripgrep 15's.
        let two = "hello\nworld\nhello\nworld\n";
        assert_eq!(
            multiline_rows("hello\nworld\n", r"hello\nworld", "X", |_| {}),
            ["1:1:X"]
        );
        // Matches on adjacent lines share one block.
        assert_eq!(
            multiline_rows(two, r"hello\nworld", "X", |_| {}),
            ["1:1:X", "2:1:X"]
        );
        assert_eq!(
            multiline_rows(two, r"hello\nworld", "X", |o| o.max_count = Some(1)),
            ["1:1:X"]
        );
        assert_eq!(
            multiline_rows("hello\nworld\n", r"(hello)\n(world)", "${2}/${1}", |o| {
                o.only_matching = true
            }),
            ["1:1:world/hello"]
        );
        let mid = "hello world\nfoo hello\nworld bar\n";
        assert_eq!(
            multiline_rows(mid, r"(o\nw)", "<$1>", |_| {}),
            ["2:9:foo hell<o", "3:9:w>orld bar"]
        );
        assert_eq!(
            multiline_rows(mid, r"(o\nw)", "<$1>", |o| o.only_matching = true),
            ["2:9:<o", "3:9:w>"]
        );
        assert_eq!(
            multiline_rows(mid, r"(o\nw)", "<$1>", |o| o.vimgrep = true),
            ["2:9:foo hell<o"]
        );
        // A pattern that cannot match a line terminator keeps line-by-line
        // rows, each with its own column.
        assert_eq!(
            multiline_rows(mid, "l.", "X", |_| {}),
            ["1:3:heXo worX", "2:7:foo heXo", "3:4:worX bar"]
        );
    }

    #[test]
    fn multiline_replace_reports_positions_in_the_rewritten_text() {
        // Expected rows are ripgrep 15's.
        let content = "hello\nworld\n";
        assert_eq!(
            multiline_rows(content, r"l\s?", "A\nB", |_| {}),
            ["1:3:heA", "2:3:BA", "3:3:Bo", "4:3:worA", "5:3:Bd"]
        );
        assert_eq!(
            multiline_rows(content, r"l\s?", "A\nB", |o| o.only_matching = true),
            ["1:3:A", "2:3:B", "2:6:A", "3:6:B", "4:14:A", "5:14:B"]
        );
        assert_eq!(
            multiline_rows(content, r"l\s?", "A\nB", |o| o.vimgrep = true),
            ["1:3:heA", "2:2:BA", "4:4:worA"]
        );
    }

    #[test]
    fn multiline_replace_measures_context_from_source_blocks() {
        // Expected rows are ripgrep 15's, which prints a `--` between the
        // second and third rows of the last case.
        let content = "x\nhello\nworld\ny\nz\nhello\nworld\n";
        let rows = ["1-x", "2:1:X", "4-y", "5-z", "6:1:X"];
        let around = |o: &mut MatchOptions| {
            o.before_context = 1;
            o.after_context = 1;
        };
        assert_eq!(multiline_rows(content, r"hello\nworld", "X", around), rows);
        assert_eq!(
            multiline_rows(content, r"hello\nworld", "X", |o| o.passthru = true),
            rows
        );
        // A block whose rewrite is empty still occupies its source lines.
        assert_eq!(
            multiline_rows(content, r"hello\nworld\n", "", |o| o.after_context = 1),
            ["2:hidden", "4-y", "6:hidden"]
        );
    }

    #[test]
    fn json_replace_reports_source_text_beside_each_replacement() {
        let matcher = SearchMatcher::standard(regex::Regex::new("(l+)|(o)").unwrap(), false);
        for multiline in [false, true] {
            let opts = MatchOptions {
                multiline,
                replace: Some("<$1>".into()),
                report_replacements: true,
                all_spans: true,
                ..Default::default()
            };
            let found = FileMatches::find("hello\nfoo\n", &matcher, &opts).unwrap();
            let mut rows = Vec::new();
            found
                .for_each(&opts, &matcher, |emit| -> Result<()> {
                    let Emit::Match {
                        content,
                        spans,
                        replacements,
                        ..
                    } = emit
                    else {
                        panic!("no context requested");
                    };
                    rows.push((content.into_owned(), spans, replacements));
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                rows,
                [
                    (
                        "hello".to_string(),
                        vec![(2, 4), (4, 5)],
                        Some(vec!["<ll>".to_string(), "<>".to_string()])
                    ),
                    (
                        "foo".to_string(),
                        vec![(1, 2), (2, 3)],
                        Some(vec!["<>".to_string(), "<>".to_string()])
                    ),
                ],
                "multiline={multiline}"
            );
        }
    }
}
