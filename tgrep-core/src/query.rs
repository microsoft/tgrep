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
            // segments in `mutex.*mutex_lock`), clear expected_next to
            // avoid false negatives — we can't reliably filter on the
            // next byte if the trigram appears in multiple contexts.
            queries.dedup_by(|b, a| {
                if a.hash == b.hash {
                    if a.expected_next != b.expected_next {
                        a.expected_next = None;
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
            let mut result = Vec::new();
            for sub in plans {
                let mut sub_result = execute_plan(sub, lookup);
                result.append(&mut sub_result);
            }
            result.sort_unstable();
            result.dedup();
            result
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

            // Diagnostic: collect sizes before sort for zero-candidate analysis
            let list_sizes: Vec<(u32, usize)> =
                lists.iter().map(|(q, l)| (q.hash, l.len())).collect();

            lists.sort_by_key(|(_, l)| l.len());

            // Start with smallest posting list
            let (first_query, first_list) = lists.remove(0);
            let mut candidates: Vec<(u32, u8, u8)> = first_list
                .into_iter()
                .map(|e| (e.file_id, e.loc_mask, e.next_mask))
                .collect();
            candidates.sort_by_key(|&(fid, _, _)| fid);
            candidates.dedup_by_key(|e| e.0);

            // Apply next_mask check for the first trigram
            if let Some(next_byte) = first_query.expected_next {
                let bit = trigram::bloom_hash(next_byte);
                candidates.retain(|&(_, _, nm)| nm & bit != 0);
            }

            // Intersect with remaining posting lists
            for (query, mut list) in lists {
                list.sort_by_key(|e| e.file_id);
                list.dedup_by_key(|e| e.file_id);

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

            // Only log when a posting list is unexpectedly empty (trigram in
            // lookup but 0 postings — indicates index corruption).
            if candidates.is_empty() && queries.len() >= 3 {
                let any_empty = list_sizes.iter().any(|(_, sz)| *sz == 0);
                if any_empty {
                    let min_size = list_sizes.iter().map(|(_, sz)| sz).min().copied().unwrap_or(0);
                    let max_size = list_sizes.iter().map(|(_, sz)| sz).max().copied().unwrap_or(0);
                    eprintln!(
                        "[trace] AND plan: 0 candidates from {} trigrams (EMPTY posting list detected). \
                         min_list={min_size} max_list={max_size}",
                        queries.len(),
                    );
                }
            }

            candidates.into_iter().map(|(fid, _, _)| fid).collect()
        }
        QueryPlan::Or(plans) => {
            let mut result = Vec::new();
            for sub in plans {
                let mut sub_result = execute_plan_with_masks(sub, lookup);
                result.append(&mut sub_result);
            }
            result.sort_unstable();
            result.dedup();
            result
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
