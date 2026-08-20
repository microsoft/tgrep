//! Shared matching logic used by both the server (serve.rs) and the local
//! search path (search.rs). Owns pattern compilation, line/offset mapping and
//! the match-and-context traversal so the two callers cannot drift apart —
//! they differ only in how they render the resulting events.

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
    pub fn find_spans(&self, hay: &str, limit: Option<usize>) -> Result<Vec<(usize, usize)>> {
        let mut spans: Vec<(usize, usize)> = Vec::new();
        match self {
            SearchMatcher::Standard(re) => {
                for m in re.find_iter(hay) {
                    spans.push((m.start(), m.end()));
                    if limit.is_some_and(|l| spans.len() >= l) {
                        break;
                    }
                }
            }
            SearchMatcher::Fancy(re) => {
                for m in re.find_iter(hay) {
                    let m = m.map_err(|e| anyhow::anyhow!("regex match error: {e}"))?;
                    spans.push((m.start(), m.end()));
                    if limit.is_some_and(|l| spans.len() >= l) {
                        break;
                    }
                }
            }
        }
        Ok(spans)
    }
}

/// Compile `patterns` into a single matcher.
///
/// `multiline` lets a match cross line boundaries; `dotall` is kept separate
/// so `-U` alone does not silently turn `.` into a line-crossing wildcard,
/// matching ripgrep's split between `--multiline` and `--multiline-dotall`.
pub fn build_search_matcher(
    patterns: &[String],
    case_insensitive: bool,
    fixed_string: bool,
    word_boundary: bool,
    multiline: bool,
    dotall: bool,
) -> Result<SearchMatcher> {
    let combined = combine_patterns(patterns, fixed_string, word_boundary);

    match build_regex(&combined, case_insensitive, multiline, dotall) {
        Ok(re) => Ok(SearchMatcher::Standard(re)),
        Err(regex_err) if !fixed_string => {
            let mut flags = String::new();
            if case_insensitive {
                flags.push('i');
            }
            if multiline {
                flags.push('m');
            }
            if dotall {
                flags.push('s');
            }
            let fancy_pattern = if flags.is_empty() {
                combined
            } else {
                format!("(?{flags}:{combined})")
            };
            fancy_regex::Regex::new(&fancy_pattern)
                .map(SearchMatcher::Fancy)
                .map_err(|fancy_err| {
                    anyhow::anyhow!(
                        "regex error: {regex_err}; PCRE-style fallback failed: {fancy_err}"
                    )
                })
        }
        Err(regex_err) => Err(regex_err),
    }
}

fn combine_patterns(patterns: &[String], fixed_string: bool, word_boundary: bool) -> String {
    let wrap = |p: &String| {
        let mut p = if fixed_string {
            regex::escape(p)
        } else {
            p.clone()
        };
        if word_boundary {
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
    case_insensitive: bool,
    multiline: bool,
    dotall: bool,
) -> Result<regex::Regex> {
    RegexBuilder::new(combined)
        .case_insensitive(case_insensitive)
        .multi_line(multiline)
        .dot_matches_new_line(dotall)
        .size_limit(100 * (1 << 20))
        .dfa_size_limit(1000 * (1 << 20))
        .nest_limit(250)
        .build()
        .map_err(|e| anyhow::anyhow!("regex error: {e}"))
}

/// The subset of search options that affects which lines match and which are
/// emitted. Shared so the server and the local search path cannot drift.
#[derive(Clone, Copy, Default)]
pub struct MatchOptions {
    pub invert_match: bool,
    pub multiline: bool,
    pub only_matching: bool,
    pub before_context: usize,
    pub after_context: usize,
    pub max_count: Option<usize>,
}

/// One unit of output produced by searching a single file.
pub enum Emit<'a> {
    Match {
        line_number: usize,
        content: &'a str,
        column: Option<usize>,
        spans: Vec<(usize, usize)>,
        absolute_offset: usize,
    },
    Context {
        line_number: usize,
        content: &'a str,
        absolute_offset: usize,
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

    /// Walk the output in line order, expanding context and fanning
    /// `--only-matching` out to one event per match.
    pub fn for_each<E>(
        &self,
        opts: &MatchOptions,
        mut on_emit: impl FnMut(Emit<'a>) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        let emit_hit = |hit: &LineHit,
                        on_emit: &mut dyn FnMut(Emit<'a>) -> std::result::Result<(), E>|
         -> std::result::Result<(), E> {
            let line = self.index.line_text(self.content, hit.idx);
            let offset = self.index.line_start(hit.idx);

            if opts.only_matching && !hit.spans.is_empty() {
                for &(s, e) in &hit.spans {
                    let text = &line[s..e];
                    on_emit(Emit::Match {
                        line_number: hit.idx + 1,
                        content: text,
                        column: Some(s + 1),
                        spans: vec![(0, text.len())],
                        absolute_offset: offset + s,
                    })?;
                }
                return Ok(());
            }

            on_emit(Emit::Match {
                line_number: hit.idx + 1,
                content: line,
                column: hit.spans.first().map(|&(s, _)| s + 1),
                spans: hit.spans.clone(),
                absolute_offset: offset,
            })
        };

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
            }
        }
        return Ok(out);
    }

    if opts.multiline {
        // Match against the whole buffer so a single match can cross lines.
        //
        // `--max-count` limits *matches*, not lines, so it is applied to the
        // spans before they are exploded into lines. Truncating the grouped
        // lines instead would print a partial match — output that doesn't
        // actually match the pattern, with submatch spans clipped to match.
        let spans = matcher.find_spans(content, max)?;
        return Ok(group_spans_by_line(content, index, &spans));
    }

    // Line-oriented mode: match each line separately so `^` and `$` anchor
    // per line, as ripgrep does.
    let mut out = Vec::new();
    for i in 0..index.line_count() {
        let spans = matcher.find_spans(index.line_text(content, i), None)?;
        if !spans.is_empty() {
            out.push(LineHit { idx: i, spans });
            if max.is_some_and(|m| out.len() >= m) {
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
