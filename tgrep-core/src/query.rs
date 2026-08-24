/// Regex → trigram query decomposition.
///
/// Parses a regex pattern using `regex-syntax` and extracts literal segments
/// that can be converted to trigram lookups. Builds a QueryPlan tree of AND/OR
/// nodes that can be evaluated against the index.
use regex_syntax::hir::{Class, Hir, HirKind, Literal};

use crate::ondisk::PostingEntry;
use crate::trigram::{self, TrigramHash};

/// A single trigram query with optional next-byte constraint.
///
/// `expected_next` is the byte that follows this trigram in the parsed
/// literal, used for next_mask Bloom-filter checks. Computed at plan-build
/// time from the HIR-extracted literal so it is always correct — even for
/// regex patterns where the raw pattern string differs from the matched text.
#[derive(Debug, Clone)]
pub struct TrigramQuery {
    pub hash: TrigramHash,
    /// Expected next character for next_mask Bloom check.
    pub expected_next: Option<u8>,
}

/// A node in the query plan tree.
#[derive(Debug, Clone)]
pub enum QueryPlan {
    /// All trigrams must match (intersection of posting lists).
    And(Vec<TrigramQuery>),
    /// Any branch can match (union of results).
    Or(Vec<QueryPlan>),
    /// No trigrams could be extracted — must scan all files.
    MatchAll,
}

impl QueryPlan {
    pub fn is_match_all(&self) -> bool {
        matches!(self, QueryPlan::MatchAll)
    }
}

/// Parse a regex pattern and produce a query plan for trigram lookups.
pub fn build_query_plan(pattern: &str, case_insensitive: bool) -> Result<QueryPlan, String> {
    let hir = regex_syntax::parse(pattern).map_err(|e| format!("regex parse error: {e}"))?;
    let plan = decompose_hir(&hir, case_insensitive);
    Ok(simplify(plan))
}

/// Build a query plan for a literal (fixed-string) pattern.
pub fn build_literal_plan(literal: &str, case_insensitive: bool) -> QueryPlan {
    let text = if case_insensitive {
        literal.to_lowercase()
    } else {
        literal.to_string()
    };
    literals_to_query_plan(text.as_bytes())
}

/// Build one plan covering every pattern the user supplied.
///
/// A file matches if *any* pattern matches, so the plans are unioned. Building
/// from only the first pattern would narrow candidates using one pattern and
/// then search for all of them, silently hiding files that only the `-e`/`-f`
/// patterns match.
///
/// As with alternation, a single `MatchAll` branch absorbs the whole plan: an
/// unindexable pattern can match anywhere, so no candidate set is safe.
pub fn build_multi_pattern_plan(
    patterns: &[String],
    fixed_string: bool,
    case_insensitive: bool,
) -> Result<QueryPlan, String> {
    let mut plans = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let plan = if fixed_string {
            build_literal_plan(pattern, case_insensitive)
        } else {
            build_query_plan(pattern, case_insensitive)?
        };
        if plan.is_match_all() {
            return Ok(QueryPlan::MatchAll);
        }
        plans.push(plan);
    }

    Ok(match plans.len() {
        0 => QueryPlan::MatchAll,
        1 => plans.pop().unwrap(),
        _ => QueryPlan::Or(plans),
    })
}

/// Build a plan for PCRE-style patterns that `regex-syntax` cannot parse.
///
/// Each pattern is first relaxed (see [`relax_for_indexing`]); anything that
/// cannot be relaxed, or that still fails to parse afterwards, degrades to
/// `MatchAll`. Relaxation failure is never an error: it only costs the index
/// optimisation, and the caller still runs the real PCRE matcher over whatever
/// candidates come back.
pub fn build_relaxed_multi_pattern_plan(patterns: &[String], case_insensitive: bool) -> QueryPlan {
    let mut plans = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let plan = relax_for_indexing(pattern)
            .and_then(|relaxed| build_query_plan(&relaxed, case_insensitive).ok());
        match plan {
            Some(plan) if !plan.is_match_all() => plans.push(plan),
            _ => return QueryPlan::MatchAll,
        }
    }

    match plans.len() {
        0 => QueryPlan::MatchAll,
        1 => plans.pop().unwrap(),
        _ => QueryPlan::Or(plans),
    }
}

/// What kind of group a `(` opens.
enum GroupKind {
    /// `(?=`, `(?!`, `(?<=`, `(?<!` — a zero-width assertion.
    Lookaround,
    /// `(?>` — an atomic group.
    Atomic,
    /// `(?(...)...)` — a conditional, which has no relaxation.
    Unsupported,
    /// A capturing, named or plain non-capturing group.
    Plain,
}

/// Rewrite a PCRE-style pattern into a more permissive one that `regex-syntax`
/// can parse, so `-P` queries can still narrow candidates through the index.
///
/// Returns `None` when the pattern uses a construct that cannot be widened
/// safely, in which case the caller must scan every file.
///
/// # Soundness
///
/// Every rewrite only ever *widens* the matched language, so
/// `L(pattern) ⊆ L(relaxed)`:
///
/// * deleting a zero-width lookaround removes a constraint on the match;
/// * turning `(?>…)` into `(?:…)` only restores backtracking paths.
///
/// That direction is the one the index needs. A trigram the relaxed pattern
/// requires is therefore required by the original too, so no file that really
/// matches can be filtered out. Narrowing would be a correctness bug — it could
/// drop a genuine hit — which is why anything ambiguous returns `None` instead
/// of being rewritten optimistically. Backreferences, conditionals, `\K` and
/// `\G` all bail out for that reason, as do possessive quantifiers: `}` is not
/// reliably distinguishable from a literal brace, and dropping the wrong `+`
/// would narrow an anchored pattern.
pub fn relax_for_indexing(pattern: &str) -> Option<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len());
    let mut i = 0;
    // Whether the previous token was an unescaped quantifier, which is what
    // makes a following `+` a possessive modifier rather than a repeat.
    let mut after_quantifier = false;

    while i < chars.len() {
        let c = chars[i];

        if c == '\\' {
            let next = *chars.get(i + 1)?;
            // `\1`-`\9` and `\k`/`\g` are backreferences, `\K` resets the match
            // start and `\G` anchors to the previous match. None has a
            // regex-syntax equivalent, and none can be widened.
            if next.is_ascii_digit() || matches!(next, 'k' | 'g' | 'K' | 'G') {
                return None;
            }
            out.push(c);
            out.push(next);
            i += 2;
            after_quantifier = false;
            continue;
        }

        match c {
            // A character class is copied through verbatim. Its extent has to be
            // found exactly, because `(?=`, `(?<!` and `(?>` are ordinary members
            // inside one - treating them as syntax would delete real class
            // members and *narrow* the pattern.
            '[' => {
                let end = skip_class(&chars, i)?;
                out.extend(&chars[i..end]);
                i = end;
                after_quantifier = false;
            }
            '(' => {
                match group_kind(&chars, i) {
                    GroupKind::Lookaround => i = skip_group(&chars, i)?,
                    GroupKind::Atomic => {
                        out.push_str("(?:");
                        i += 3;
                    }
                    GroupKind::Unsupported => return None,
                    GroupKind::Plain => {
                        out.push(c);
                        i += 1;
                    }
                }
                after_quantifier = false;
            }
            // Only a real `{n,m}` counts as a quantifier. A brace that is part of
            // `\p{L}` or a literal is not, and treating it as one would bail out
            // of perfectly relaxable patterns.
            '{' => match repetition_end(&chars, i) {
                Some(end) => {
                    out.extend(&chars[i..end]);
                    i = end;
                    after_quantifier = true;
                }
                None => {
                    out.push(c);
                    i += 1;
                    after_quantifier = false;
                }
            },
            // A possessive quantifier. Rewriting it means deciding which `+` is
            // the modifier, which is not always knowable; bail instead.
            '+' if after_quantifier => return None,
            _ => {
                out.push(c);
                i += 1;
                after_quantifier = matches!(c, '*' | '+' | '?');
            }
        }
    }

    Some(out)
}

/// Return the index just past the `]` that closes the class at `chars[open]`,
/// or `None` if it is unterminated.
///
/// This has to agree with the engines it feeds, both of which have the same two
/// rules: a `]` in the *first* position (after an optional `^`) is a literal
/// member rather than the terminator, and `[` nests, so `[[:digit:]]` closes on
/// the second `]`, not the first.
///
/// Erring long is safe - the extra text is copied verbatim, and any lookaround
/// swallowed with it survives into a pattern `regex-syntax` rejects, which
/// degrades to a full scan. Erring *short* is the dangerous direction, because
/// the rest of the class body would then be reinterpreted as regex syntax.
fn skip_class(chars: &[char], open: usize) -> Option<usize> {
    debug_assert_eq!(chars.get(open), Some(&'['));
    let mut i = open + 1;
    if chars.get(i) == Some(&'^') {
        i += 1;
    }
    if chars.get(i) == Some(&']') {
        i += 1;
    }

    let mut depth = 1usize;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Return the index just past the `}` of a `{n}`, `{n,}` or `{n,m}` repetition
/// starting at `chars[open]`, or `None` if this brace is not one.
fn repetition_end(chars: &[char], open: usize) -> Option<usize> {
    debug_assert_eq!(chars.get(open), Some(&'{'));
    let mut i = open + 1;
    let digits = |i: &mut usize| {
        let start = *i;
        while chars.get(*i).is_some_and(char::is_ascii_digit) {
            *i += 1;
        }
        *i > start
    };

    if !digits(&mut i) {
        return None;
    }
    if chars.get(i) == Some(&',') {
        i += 1;
        digits(&mut i);
    }
    (chars.get(i) == Some(&'}')).then_some(i + 1)
}

/// Classify the group opening at `chars[open]`, which must be `(`.
fn group_kind(chars: &[char], open: usize) -> GroupKind {
    if chars.get(open + 1) != Some(&'?') {
        return GroupKind::Plain;
    }
    match chars.get(open + 2) {
        Some('=') | Some('!') => GroupKind::Lookaround,
        // `(?<=` and `(?<!` are lookbehind; `(?<name>` is just a named group.
        Some('<') => match chars.get(open + 3) {
            Some('=') | Some('!') => GroupKind::Lookaround,
            _ => GroupKind::Plain,
        },
        Some('>') => GroupKind::Atomic,
        // `(?(1)yes|no)` — a conditional.
        Some('(') => GroupKind::Unsupported,
        // `(?P=name)` is a backreference; `(?P<name>` is a named group.
        Some('P') if chars.get(open + 3) == Some(&'=') => GroupKind::Unsupported,
        // `(?#…)` is a comment, whose body is arbitrary text rather than syntax.
        Some('#') => GroupKind::Unsupported,
        // `(?x)` switches on verbose mode, which changes how whitespace and `#`
        // are lexed for the rest of the pattern. This scanner does not model
        // that, so a `(?=`-looking sequence inside an x-mode comment would be
        // read as a real lookaround. Bail rather than reason about it.
        _ if enables_extended_mode(chars, open + 2) => GroupKind::Unsupported,
        _ => GroupKind::Plain,
    }
}

/// Whether the `(?…` at `start` is an inline flag group that turns on `x`.
///
/// Only a group made up entirely of flag letters counts, terminated by `)` or
/// `:`; anything else is some other construct and is left alone.
fn enables_extended_mode(chars: &[char], start: usize) -> bool {
    let mut i = start;
    let mut negated = false;
    let mut extended = false;

    while let Some(&c) = chars.get(i) {
        match c {
            '-' => negated = true,
            'x' if !negated => extended = true,
            'i' | 'm' | 's' | 'u' | 'U' | 'R' | 'x' => {}
            ':' | ')' => return extended,
            _ => return false,
        }
        i += 1;
    }
    false
}

/// Return the index just past the `)` that closes the group at `chars[open]`,
/// or `None` if the pattern is unbalanced.
fn skip_group(chars: &[char], open: usize) -> Option<usize> {
    debug_assert_eq!(chars.get(open), Some(&'('));
    // Starts at the opening paren, so `depth` is 1 before any `)` is seen and
    // the decrement below cannot underflow.
    let mut depth = 0usize;
    let mut i = open;

    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            // A class body can contain unescaped `(` and `)`, so it has to be
            // skipped as a unit or the depth count goes wrong.
            '[' => i = skip_class(chars, i)?,
            '(' => {
                depth += 1;
                i += 1;
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Convert a byte sequence into a QueryPlan of AND'd trigram queries.
/// Each `TrigramQuery` carries the expected next byte (if any) so that
/// next_mask Bloom-filter checks use the correct literal byte — not the
/// raw regex pattern string.
fn literals_to_query_plan(bytes: &[u8]) -> QueryPlan {
    if bytes.len() < 3 {
        return QueryPlan::MatchAll;
    }
    let queries: Vec<TrigramQuery> = (0..bytes.len() - 2)
        .map(|i| {
            let hash = trigram::hash(bytes[i], bytes[i + 1], bytes[i + 2]);
            let expected_next = if i + 3 < bytes.len() {
                Some(bytes[i + 3])
            } else {
                None
            };
            TrigramQuery {
                hash,
                expected_next,
            }
        })
        .collect();
    QueryPlan::And(queries)
}

fn decompose_hir(hir: &Hir, case_insensitive: bool) -> QueryPlan {
    match hir.kind() {
        HirKind::Literal(Literal(bytes)) => {
            let text = if case_insensitive {
                String::from_utf8_lossy(bytes).to_lowercase()
            } else {
                String::from_utf8_lossy(bytes).into_owned()
            };
            literals_to_query_plan(text.as_bytes())
        }
        HirKind::Concat(subs) => {
            // Collect all literals from concat children into a single string,
            // then extract trigrams. Non-literal children break the chain.
            let mut all_queries = Vec::new();
            let mut current_literal = String::new();

            for sub in subs {
                if let HirKind::Literal(Literal(bytes)) = sub.kind() {
                    let s = String::from_utf8_lossy(bytes);
                    current_literal.push_str(&s);
                } else {
                    // Flush the current literal run
                    if !current_literal.is_empty() {
                        let text = if case_insensitive {
                            current_literal.to_lowercase()
                        } else {
                            current_literal.clone()
                        };
                        if let QueryPlan::And(queries) = literals_to_query_plan(text.as_bytes()) {
                            all_queries.extend(queries);
                        }
                        current_literal.clear();
                    }
                    // Recurse into the non-literal child
                    let sub_plan = decompose_hir(sub, case_insensitive);
                    if let QueryPlan::And(queries) = sub_plan {
                        all_queries.extend(queries);
                    }
                    // MatchAll or Or children don't contribute AND trigrams
                }
            }

            // Flush remaining literal
            if !current_literal.is_empty() {
                let text = if case_insensitive {
                    current_literal.to_lowercase()
                } else {
                    current_literal
                };
                if let QueryPlan::And(queries) = literals_to_query_plan(text.as_bytes()) {
                    all_queries.extend(queries);
                }
            }

            if all_queries.is_empty() {
                QueryPlan::MatchAll
            } else {
                QueryPlan::And(all_queries)
            }
        }
        HirKind::Alternation(alts) => {
            let plans: Vec<QueryPlan> = alts
                .iter()
                .map(|a| decompose_hir(a, case_insensitive))
                .collect();
            // If any branch is MatchAll, the whole alternation is MatchAll
            if plans.iter().any(|p| p.is_match_all()) {
                QueryPlan::MatchAll
            } else {
                QueryPlan::Or(plans)
            }
        }
        HirKind::Repetition(rep) => {
            if rep.min >= 1 {
                decompose_hir(&rep.sub, case_insensitive)
            } else {
                // min=0 means the pattern is optional → can match anything
                QueryPlan::MatchAll
            }
        }
        HirKind::Capture(cap) => decompose_hir(&cap.sub, case_insensitive),
        HirKind::Class(Class::Unicode(_)) | HirKind::Class(Class::Bytes(_)) => QueryPlan::MatchAll,
        HirKind::Look(_) | HirKind::Empty => QueryPlan::MatchAll,
    }
}

/// Simplify the query plan (dedup trigrams, flatten nested structures).
fn simplify(plan: QueryPlan) -> QueryPlan {
    match plan {
        QueryPlan::And(mut queries) => {
            queries.sort_by_key(|q| q.hash);
            // Dedup by trigram hash. When the same trigram appears with
            // different expected_next values (e.g. from separate literal
            // segments in `mutex.*mutex_lock`), clear expected_next on the
            // retained query to avoid false negatives — we can't reliably
            // filter on the next byte if the trigram appears in multiple
            // contexts.
            queries.dedup_by(|retained, duplicate| {
                if retained.hash == duplicate.hash {
                    if retained.expected_next != duplicate.expected_next {
                        retained.expected_next = None;
                    }
                    true
                } else {
                    false
                }
            });
            if queries.is_empty() {
                QueryPlan::MatchAll
            } else {
                QueryPlan::And(queries)
            }
        }
        QueryPlan::Or(plans) => {
            let simplified: Vec<QueryPlan> = plans.into_iter().map(simplify).collect();
            if simplified.len() == 1 {
                simplified.into_iter().next().unwrap()
            } else {
                QueryPlan::Or(simplified)
            }
        }
        other => other,
    }
}

/// Execute a query plan against an index, returning candidate file IDs.
pub fn execute_plan<F>(plan: &QueryPlan, lookup: &F) -> Vec<u32>
where
    F: Fn(TrigramHash) -> Vec<u32>,
{
    match plan {
        QueryPlan::And(queries) => {
            if queries.is_empty() {
                return Vec::new();
            }
            let mut lists: Vec<Vec<u32>> = queries.iter().map(|q| lookup(q.hash)).collect();
            lists.sort_by_key(|l| l.len());

            let mut result: Vec<u32> = lists.remove(0);
            result.sort_unstable();
            result.dedup();

            for mut list in lists {
                list.sort_unstable();
                list.dedup();
                result = intersect_sorted(&result, &list);
                if result.is_empty() {
                    break;
                }
            }
            result
        }
        QueryPlan::Or(plans) => {
            let lists = plans.iter().map(|sub| execute_plan(sub, lookup)).collect();
            union_many_sorted(lists)
        }
        QueryPlan::MatchAll => Vec::new(), // caller must handle: scan all files
    }
}

/// Execute a query plan with mask-aware filtering.
///
/// Uses next_mask Bloom-filter checks to reduce false-positive candidates.
/// The `expected_next` byte is embedded in each `TrigramQuery` at plan-build
/// time from the HIR-parsed literal, so no raw pattern string is needed.
pub fn execute_plan_with_masks<F>(plan: &QueryPlan, lookup: &F) -> Vec<u32>
where
    F: Fn(TrigramHash) -> Vec<PostingEntry>,
{
    match plan {
        QueryPlan::And(queries) => {
            if queries.is_empty() {
                return Vec::new();
            }

            // Fetch full posting entries (with masks) for each trigram
            let mut lists: Vec<(&TrigramQuery, Vec<PostingEntry>)> =
                queries.iter().map(|q| (q, lookup(q.hash))).collect();

            lists.sort_by_key(|(_, l)| l.len());

            // Start with smallest posting list
            let (first_query, mut first_list) = lists.remove(0);
            sort_dedup_postings_by_file_id(&mut first_list);
            let mut candidates: Vec<(u32, u8, u8)> = first_list
                .into_iter()
                .map(|e| (e.file_id, e.loc_mask, e.next_mask))
                .collect();

            // Apply next_mask check for the first trigram
            if let Some(next_byte) = first_query.expected_next {
                let bit = trigram::bloom_hash(next_byte);
                candidates.retain(|&(_, _, nm)| nm & bit != 0);
            }

            // Intersect with remaining posting lists
            for (query, mut list) in lists {
                sort_dedup_postings_by_file_id(&mut list);

                // Intersect by file_id, using next_mask to filter
                let mut new_candidates = Vec::new();
                let (mut i, mut j) = (0, 0);
                while i < candidates.len() && j < list.len() {
                    let (fid_a, _, _) = candidates[i];
                    let fid_b = list[j].file_id;
                    match fid_a.cmp(&fid_b) {
                        std::cmp::Ordering::Equal => {
                            let nm = list[j].next_mask;
                            // Apply next_mask check using the literal-derived expected_next
                            let pass = match query.expected_next {
                                Some(nb) => nm & trigram::bloom_hash(nb) != 0,
                                None => true,
                            };
                            if pass {
                                new_candidates.push((fid_a, list[j].loc_mask, nm));
                            }
                            i += 1;
                            j += 1;
                        }
                        std::cmp::Ordering::Less => i += 1,
                        std::cmp::Ordering::Greater => j += 1,
                    }
                }
                candidates = new_candidates;
                if candidates.is_empty() {
                    break;
                }
            }

            candidates.into_iter().map(|(fid, _, _)| fid).collect()
        }
        QueryPlan::Or(plans) => {
            let lists = plans
                .iter()
                .map(|sub| execute_plan_with_masks(sub, lookup))
                .collect();
            union_many_sorted(lists)
        }
        QueryPlan::MatchAll => Vec::new(),
    }
}

fn intersect_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    result
}

fn union_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => {
                result.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                result.push(b[j]);
                j += 1;
            }
        }
    }
    result.extend_from_slice(&a[i..]);
    result.extend_from_slice(&b[j..]);
    result
}

fn union_many_sorted(mut lists: Vec<Vec<u32>>) -> Vec<u32> {
    lists.retain(|list| !list.is_empty());
    let mut iter = lists.into_iter();
    let Some(mut result) = iter.next() else {
        return Vec::new();
    };

    while let Some(list) = iter.next() {
        let previous_len = result.len();
        let list_len = list.len();
        result = union_sorted(&result, &list);
        let added = result.len() - previous_len;
        if added > list_len / 2 {
            let mut remaining = Vec::with_capacity(iter.size_hint().0 + 1);
            remaining.push(result);
            remaining.extend(iter);
            return union_many_sorted_balanced(remaining);
        }
    }
    result
}

fn union_many_sorted_balanced(mut lists: Vec<Vec<u32>>) -> Vec<u32> {
    while lists.len() > 1 {
        let mut next = Vec::with_capacity(lists.len().div_ceil(2));
        let mut iter = lists.into_iter();
        while let Some(left) = iter.next() {
            if let Some(right) = iter.next() {
                next.push(union_sorted(&left, &right));
            } else {
                next.push(left);
            }
        }
        lists = next;
    }
    lists.pop().unwrap_or_default()
}

fn sort_dedup_postings_by_file_id(entries: &mut Vec<PostingEntry>) {
    if entries
        .windows(2)
        .all(|pair| pair[0].file_id < pair[1].file_id)
    {
        return;
    }
    entries.sort_by_key(|e| e.file_id);
    entries.dedup_by_key(|e| e.file_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Relaxing PCRE patterns for indexing
    //
    // `-P` patterns used to bypass the index wholesale, because `regex-syntax`
    // rejects lookaround and the plan builder is written against its HIR. That
    // turned a pattern with a perfectly good mandatory literal into a full-corpus
    // scan. Relaxation recovers the literal; these tests pin the property that
    // makes it safe, namely that the rewrite only ever widens the language.
    // -----------------------------------------------------------------------

    /// Every trigram the relaxed pattern demands must also be demanded by the
    /// original, so no genuinely matching file can be filtered out.
    fn assert_relaxes_to(pattern: &str, expected: &str) {
        assert_eq!(
            relax_for_indexing(pattern).as_deref(),
            Some(expected),
            "relaxing {pattern:?}"
        );
    }

    #[test]
    fn negative_lookbehind_is_dropped_but_its_literal_survives() {
        // The Substrate benchmark's `pcre2-lookaround` query. `ExchangePrincipal`
        // is mandatory for any match, so the plan must be able to see it.
        assert_relaxes_to("(?<!//)ExchangePrincipal", "ExchangePrincipal");

        let plan = build_query_plan("ExchangePrincipal", false).unwrap();
        assert!(
            !plan.is_match_all(),
            "the relaxed pattern yields a real trigram plan, not a full scan"
        );
    }

    #[test]
    fn every_lookaround_flavour_is_dropped() {
        assert_relaxes_to("(?=foo)bar", "bar");
        assert_relaxes_to("(?!foo)bar", "bar");
        assert_relaxes_to("(?<=foo)bar", "bar");
        assert_relaxes_to("(?<!foo)bar", "bar");
    }

    #[test]
    fn nested_groups_inside_a_lookaround_are_skipped_wholesale() {
        assert_relaxes_to("(?=a(b(c))d)needle", "needle");
        // A `)` inside a character class must not be mistaken for the closer.
        assert_relaxes_to("(?![)])needle", "needle");
        // Nor must an escaped one.
        assert_relaxes_to(r"(?=a\))needle", "needle");
        // A class whose first member is a literal `]` still has to be skipped as
        // a unit, or the `)` after it ends the group early.
        assert_relaxes_to("(?=[])])needle", "needle");
    }

    // A `]` in first position is a class member, and `[` nests. Getting either
    // wrong ends the class early, after which its remaining body is read as
    // top-level syntax - and `(?=`, `(?<!` and `(?>` inside a class are ordinary
    // members that would then be deleted. That NARROWS the pattern, which is the
    // one direction relaxation must never take.
    #[test]
    fn a_leading_bracket_is_a_class_member_not_a_terminator() {
        assert_relaxes_to("[]a]n", "[]a]n");
        assert_relaxes_to("[^]a]n", "[^]a]n");
        // The regression: these must survive untouched.
        assert_relaxes_to("[](?=a)]n", "[](?=a)]n");
        assert_relaxes_to("[^](?<!a)]n", "[^](?<!a)]n");
        assert_relaxes_to("[](?>a)]n", "[](?>a)]n");
    }

    #[test]
    fn character_classes_nest() {
        assert_relaxes_to("[[:digit:]a]n", "[[:digit:]a]n");
        // The regression: the inner `]` closed the class, exposing the lookahead.
        assert_relaxes_to("[[:digit:](?=a)]n", "[[:digit:](?=a)]n");
        assert_relaxes_to("[a[b]]n", "[a[b]]n");
        // An unterminated class must bail rather than truncate.
        assert_eq!(relax_for_indexing("[[:digit:]"), None);
    }

    #[test]
    fn a_brace_is_only_a_quantifier_when_it_really_is_one() {
        // `\p{L}+` is not a possessive quantifier, and bailing on it would
        // forfeit the optimisation for a common construct.
        assert_relaxes_to(r"\p{L}+x", r"\p{L}+x");
        assert_relaxes_to(r"\p{Greek}+(?=x)", r"\p{Greek}+");
        // A literal brace is not one either.
        assert_relaxes_to("a{x}+b", "a{x}+b");
        // But a real repetition still guards against the possessive form.
        assert_eq!(relax_for_indexing("a{2,3}+b"), None);
        assert_eq!(relax_for_indexing("a{2}+b"), None);
        assert_eq!(relax_for_indexing("a{2,}+b"), None);
        assert_relaxes_to("a{2,3}b", "a{2,3}b");
    }

    #[test]
    fn named_groups_are_not_mistaken_for_lookbehind() {
        assert_relaxes_to("(?<name>needle)", "(?<name>needle)");
        assert_relaxes_to("(?P<name>needle)", "(?P<name>needle)");
    }

    #[test]
    fn atomic_groups_become_ordinary_ones() {
        // `(?>x)` matches a subset of `(?:x)`, so widening is safe.
        assert_relaxes_to("(?>needle)s", "(?:needle)s");
    }

    #[test]
    fn a_plain_pattern_is_returned_unchanged() {
        for pattern in ["needle", r"foo\d+bar", "[a-z](x|y)z", r"a\(b"] {
            assert_relaxes_to(pattern, pattern);
        }
    }

    #[test]
    fn verbose_mode_and_comments_bail_out() {
        // In `x` mode whitespace is insignificant and `#` starts a comment, so a
        // `(?=`-looking sequence inside a comment is not a lookaround at all.
        // The scanner does not model that lexing, so these must not be relaxed.
        assert_eq!(relax_for_indexing("(?x)needle (?=x)"), None);
        assert_eq!(relax_for_indexing("(?x:needle)"), None);
        assert_eq!(relax_for_indexing("(?ix)needle"), None);
        assert_eq!(relax_for_indexing("(?xi:needle)"), None);
        // A comment group's body is arbitrary text, not syntax.
        assert_eq!(relax_for_indexing("(?#a comment)needle"), None);
        // Turning `x` back off still means the pattern used it.
        assert_eq!(relax_for_indexing("(?x-i)needle"), None);

        // Flag groups that do not enable `x` are still relaxable, and `x` after
        // a `-` is being switched off, not on.
        assert_relaxes_to("(?i)needle(?=s)", "(?i)needle");
        assert_relaxes_to("(?i:needle)(?=s)", "(?i:needle)");
        assert_relaxes_to("(?-x)needle(?=s)", "(?-x)needle");
        // `(?<name>` must not be mistaken for a flag group.
        assert_relaxes_to("(?<name>needle)(?=s)", "(?<name>needle)");
    }

    #[test]
    fn constructs_that_cannot_be_widened_bail_out() {
        // Backreferences: no regex-syntax equivalent, and no safe superset that
        // keeps the trigrams meaningful.
        assert_eq!(relax_for_indexing(r"(a)\1"), None);
        assert_eq!(relax_for_indexing(r"(?<n>a)\k<n>"), None);
        assert_eq!(relax_for_indexing("(?P<n>a)(?P=n)"), None);
        // `\K` resets the reported match start; `\G` anchors to the last match.
        assert_eq!(relax_for_indexing(r"foo\Kbar"), None);
        assert_eq!(relax_for_indexing(r"\Gfoo"), None);
        // Conditionals.
        assert_eq!(relax_for_indexing("(a)(?(1)b|c)"), None);
        // Unbalanced input must not panic or silently truncate.
        assert_eq!(relax_for_indexing("(?=abc"), None);
        assert_eq!(relax_for_indexing("[abc"), None);
        assert_eq!(relax_for_indexing(r"abc\"), None);
    }

    #[test]
    fn possessive_quantifiers_bail_rather_than_guess() {
        // Dropping the modifier off `x}+` would turn one-or-more `}` into
        // exactly one, which narrows an anchored pattern. `}` is not reliably
        // distinguishable from a literal brace, so every case bails.
        assert_eq!(relax_for_indexing("a*+b"), None);
        assert_eq!(relax_for_indexing("a++b"), None);
        assert_eq!(relax_for_indexing("a?+b"), None);
        assert_eq!(relax_for_indexing("a{2,3}+b"), None);
        // An escaped `+` after a quantifier is a literal, not a modifier.
        assert_relaxes_to(r"a*\+b", r"a*\+b");
        // And a `+` after an *escaped* quantifier character is an ordinary
        // repeat, so it must not be mistaken for a modifier either.
        assert_relaxes_to(r"a\*+b", r"a\*+b");
    }

    #[test]
    fn relaxed_multi_pattern_plan_degrades_instead_of_erroring() {
        // One unrelaxable pattern poisons the union, exactly as `MatchAll` does
        // for alternation — but it is a plan, never an error.
        let plan = build_relaxed_multi_pattern_plan(
            &["(?<!//)ExchangePrincipal".to_string(), r"(a)\1".to_string()],
            false,
        );
        assert!(plan.is_match_all());

        let plan = build_relaxed_multi_pattern_plan(
            &[
                "(?<!//)ExchangePrincipal".to_string(),
                "(?=x)Serialization".to_string(),
            ],
            false,
        );
        assert!(
            !plan.is_match_all(),
            "two relaxable patterns union into a usable plan"
        );
    }

    #[test]
    fn a_lookaround_only_pattern_falls_back_to_a_full_scan() {
        // Relaxing leaves `^`, which constrains nothing the index can use. The
        // result must be `MatchAll`, not an empty (and therefore empty-result)
        // plan.
        let plan = build_relaxed_multi_pattern_plan(&["^(?=.*foo)".to_string()], false);
        assert!(plan.is_match_all());
    }

    #[test]
    fn relaxation_never_narrows_the_language() {
        // The soundness property, checked concretely: anything the original
        // matches, the relaxed pattern matches too.
        let cases: [(&str, &[&str]); 4] = [
            ("(?<!//)ExchangePrincipal", &["x ExchangePrincipal"]),
            ("(?=.*foo)bar", &["foo bar", "bar foo"]),
            ("(?>ab)c", &["abc"]),
            ("a(?!b)c", &["ac"]),
        ];
        for (pattern, haystacks) in cases {
            let relaxed = relax_for_indexing(pattern).expect("relaxable");
            let re = regex::Regex::new(&relaxed).expect("relaxed pattern parses");
            for haystack in haystacks {
                assert!(
                    re.is_match(haystack),
                    "{relaxed:?} (from {pattern:?}) must still match {haystack:?}"
                );
            }
        }
    }

    #[test]
    fn test_literal_plan() {
        let plan = build_query_plan("hello", false).unwrap();
        match plan {
            QueryPlan::And(tris) => {
                assert_eq!(tris.len(), 3); // "hel", "ell", "llo"
            }
            _ => panic!("expected And plan for literal"),
        }
    }

    #[test]
    fn test_alternation_plan() {
        let plan = build_query_plan("foo|bar", false).unwrap();
        match plan {
            QueryPlan::Or(branches) => {
                assert_eq!(branches.len(), 2);
            }
            _ => panic!("expected Or plan for alternation"),
        }
    }

    #[test]
    fn test_short_pattern() {
        let plan = build_query_plan("ab", false).unwrap();
        assert!(plan.is_match_all());
    }

    #[test]
    fn test_wildcard_is_match_all() {
        let plan = build_query_plan(".*", false).unwrap();
        assert!(plan.is_match_all());
    }

    #[test]
    fn test_intersect_sorted() {
        assert_eq!(intersect_sorted(&[1, 3, 5, 7], &[2, 3, 5, 8]), vec![3, 5]);
        assert_eq!(intersect_sorted(&[1, 2, 3], &[4, 5, 6]), Vec::<u32>::new());
    }

    #[test]
    fn test_union_sorted() {
        assert_eq!(
            union_sorted(&[1, 3, 5, 7], &[2, 3, 5, 8]),
            vec![1, 2, 3, 5, 7, 8]
        );
        assert_eq!(union_sorted(&[], &[4, 5, 6]), vec![4, 5, 6]);
        assert_eq!(union_sorted(&[1, 2, 3], &[]), vec![1, 2, 3]);
    }

    #[test]
    fn test_union_many_sorted_handles_disjoint_lists_without_repeated_linear_growth() {
        let lists = vec![vec![1, 3], vec![2, 4], vec![10, 12], vec![11, 13]];
        assert_eq!(union_many_sorted(lists), vec![1, 2, 3, 4, 10, 11, 12, 13]);
    }

    #[test]
    fn test_or_execution_returns_sorted_unique_candidates() {
        let plan = QueryPlan::Or(vec![
            QueryPlan::And(vec![TrigramQuery {
                hash: 1,
                expected_next: None,
            }]),
            QueryPlan::And(vec![TrigramQuery {
                hash: 2,
                expected_next: None,
            }]),
        ]);
        let candidates = execute_plan(&plan, &|tri| match tri {
            1 => vec![3, 1, 3],
            2 => vec![2, 3, 4],
            _ => Vec::new(),
        });
        assert_eq!(candidates, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_sort_dedup_postings_keeps_sorted_unique_list_unchanged() {
        let mut entries = vec![
            PostingEntry {
                file_id: 1,
                loc_mask: 1,
                next_mask: 1,
            },
            PostingEntry {
                file_id: 2,
                loc_mask: 2,
                next_mask: 2,
            },
        ];
        sort_dedup_postings_by_file_id(&mut entries);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].loc_mask, 1);
        assert_eq!(entries[1].loc_mask, 2);
    }

    #[test]
    fn test_sort_dedup_postings_sorts_and_dedups_unsorted_list() {
        let mut entries = vec![
            PostingEntry {
                file_id: 2,
                loc_mask: 2,
                next_mask: 2,
            },
            PostingEntry {
                file_id: 1,
                loc_mask: 1,
                next_mask: 1,
            },
            PostingEntry {
                file_id: 2,
                loc_mask: 3,
                next_mask: 3,
            },
        ];
        sort_dedup_postings_by_file_id(&mut entries);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].file_id, 1);
        assert_eq!(entries[1].file_id, 2);
    }

    #[test]
    fn test_case_insensitive_literal_plan() {
        // "class AlertSchema" with case-insensitive should produce trigrams
        // from "class alertschema"
        let plan = build_literal_plan("class AlertSchema", true);
        match &plan {
            QueryPlan::And(queries) => {
                assert!(!queries.is_empty(), "should have trigrams");
                // Verify these are lowercase trigrams
                let expected = trigram::extract_from_literal("class alertschema");
                let hashes: Vec<TrigramHash> = queries.iter().map(|q| q.hash).collect();
                for tri in &expected {
                    assert!(hashes.contains(tri), "missing trigram {tri:#010x}");
                }
            }
            _ => panic!("expected And plan"),
        }
    }

    #[test]
    fn test_case_insensitive_regex_plan() {
        // Same test but via regex parser path
        let plan = build_query_plan("class AlertSchema", true).unwrap();
        match &plan {
            QueryPlan::And(queries) => {
                assert!(!queries.is_empty(), "should have trigrams");
                let expected = trigram::extract_from_literal("class alertschema");
                let hashes: Vec<TrigramHash> = queries.iter().map(|q| q.hash).collect();
                for tri in &expected {
                    assert!(hashes.contains(tri), "missing trigram {tri:#010x}");
                }
            }
            _ => panic!("expected And plan, got {plan:?}"),
        }
    }

    #[test]
    fn test_case_insensitive_end_to_end() {
        // Simulate: index file with "class AlertSchema", query case-insensitively
        let content = b"internal class AlertSchema : AlertBaseSchema";

        // Extract trigrams the way the builder does (original + lowercase)
        let mut file_tris = trigram::extract(content);
        let lower = content.to_ascii_lowercase();
        file_tris.extend(trigram::extract(&lower));

        // Build inverted index for file_id=0
        let mut inverted = std::collections::HashMap::<u32, Vec<u32>>::new();
        for &tri in &file_tris {
            inverted.entry(tri).or_default().push(0);
        }

        // Query with case-insensitive plan
        let plan = build_query_plan("class AlertSchema", true).unwrap();
        let candidates = execute_plan(&plan, &|tri| {
            inverted.get(&tri).cloned().unwrap_or_default()
        });

        assert!(
            candidates.contains(&0),
            "case-insensitive search should find the file"
        );
    }

    #[test]
    fn test_mask_filtering_finds_match() {
        // File contains "mutex_lock" — should be found with mask filtering
        let content = b"calling mutex_lock here";
        let tri_masks = trigram::extract_with_masks(content);
        let lower = content.to_ascii_lowercase();
        let lower_tri_masks = trigram::extract_with_masks(&lower);

        // Build inverted index with masks for file_id=0
        let mut inverted = std::collections::HashMap::<u32, Vec<PostingEntry>>::new();
        let mut per_tri = std::collections::HashMap::<u32, trigram::TrigramMasks>::new();
        for &(tri, m) in tri_masks.iter().chain(lower_tri_masks.iter()) {
            let entry = per_tri.entry(tri).or_default();
            entry.loc_mask |= m.loc_mask;
            entry.next_mask |= m.next_mask;
        }
        for (tri, m) in per_tri {
            inverted.entry(tri).or_default().push(PostingEntry {
                file_id: 0,
                loc_mask: m.loc_mask,
                next_mask: m.next_mask,
            });
        }

        let plan = build_literal_plan("mutex_lock", false);
        let candidates = execute_plan_with_masks(&plan, &|tri| {
            inverted.get(&tri).cloned().unwrap_or_default()
        });

        assert!(
            candidates.contains(&0),
            "mask filtering should find the file containing 'mutex_lock'"
        );
    }

    #[test]
    fn test_mask_filtering_rejects_false_positive() {
        // File contains "mutex" and "clock" but NOT "mutex_clock" or anything
        // that has the trigrams adjacent. The next_mask should filter it out.
        let content = b"use mutex; use clock;";
        let tri_masks = trigram::extract_with_masks(content);

        let mut inverted = std::collections::HashMap::<u32, Vec<PostingEntry>>::new();
        let mut per_tri = std::collections::HashMap::<u32, trigram::TrigramMasks>::new();
        for &(tri, m) in &tri_masks {
            let entry = per_tri.entry(tri).or_default();
            entry.loc_mask |= m.loc_mask;
            entry.next_mask |= m.next_mask;
        }
        for (tri, m) in per_tri {
            inverted.entry(tri).or_default().push(PostingEntry {
                file_id: 0,
                loc_mask: m.loc_mask,
                next_mask: m.next_mask,
            });
        }

        // Search for "mutex_lock" — file doesn't contain this, but has some
        // overlapping trigrams. The mask filtering should reduce or eliminate
        // this as a candidate.
        let plan = build_literal_plan("mutex_lock", false);
        let candidates = execute_plan_with_masks(&plan, &|tri| {
            inverted.get(&tri).cloned().unwrap_or_default()
        });

        // The file should NOT be a candidate because it doesn't contain all
        // required trigrams (e.g., "x_l", "_lo", "loc" are missing entirely)
        assert!(
            candidates.is_empty(),
            "mask filtering should reject file not containing 'mutex_lock' trigrams"
        );
    }

    #[test]
    fn multi_pattern_plan_unions_every_pattern() {
        let patterns = vec!["alphaword".to_string(), "betaword".to_string()];
        match build_multi_pattern_plan(&patterns, false, false).unwrap() {
            QueryPlan::Or(branches) => assert_eq!(branches.len(), 2),
            other => panic!("expected an OR over both patterns, got {other:?}"),
        }
    }

    #[test]
    fn multi_pattern_plan_collapses_to_single_branch() {
        let patterns = vec!["alphaword".to_string()];
        match build_multi_pattern_plan(&patterns, false, false).unwrap() {
            QueryPlan::And(_) => {}
            other => panic!("a lone pattern should not be wrapped in OR, got {other:?}"),
        }
    }

    #[test]
    fn multi_pattern_plan_absorbs_unindexable_patterns() {
        // `.` matches anywhere, so no candidate set is safe: the union must
        // degrade to a full scan rather than silently dropping that branch.
        let patterns = vec!["alphaword".to_string(), ".".to_string()];
        assert!(
            build_multi_pattern_plan(&patterns, false, false)
                .unwrap()
                .is_match_all()
        );
    }

    #[test]
    fn multi_pattern_plan_supports_fixed_strings() {
        let patterns = vec!["a.b.c(d)".to_string(), "x[y]z".to_string()];
        // These are invalid/odd regexes but valid literals; -F must not error.
        match build_multi_pattern_plan(&patterns, true, false).unwrap() {
            QueryPlan::Or(branches) => assert_eq!(branches.len(), 2),
            other => panic!("expected an OR, got {other:?}"),
        }
    }
}
