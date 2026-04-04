/// Regex → trigram query decomposition.
///
/// Parses a regex pattern using `regex-syntax` and extracts literal segments
/// that can be converted to trigram lookups. Builds a QueryPlan tree of AND/OR
/// nodes that can be evaluated against the index.
use regex_syntax::hir::{Class, Hir, HirKind, Literal};

use crate::trigram::{self, TrigramHash};

/// A node in the query plan tree.
#[derive(Debug, Clone)]
pub enum QueryPlan {
    /// All trigrams must match (intersection of posting lists).
    And(Vec<TrigramHash>),
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
    let trigrams = trigram::extract_from_literal(&text);
    if trigrams.is_empty() {
        QueryPlan::MatchAll
    } else {
        QueryPlan::And(trigrams)
    }
}

fn decompose_hir(hir: &Hir, case_insensitive: bool) -> QueryPlan {
    match hir.kind() {
        HirKind::Literal(Literal(bytes)) => {
            let text = if case_insensitive {
                String::from_utf8_lossy(bytes).to_lowercase()
            } else {
                String::from_utf8_lossy(bytes).into_owned()
            };
            let trigrams = trigram::extract_from_literal(&text);
            if trigrams.is_empty() {
                QueryPlan::MatchAll
            } else {
                QueryPlan::And(trigrams)
            }
        }
        HirKind::Concat(subs) => {
            // Collect all literals from concat children into a single string,
            // then extract trigrams. Non-literal children break the chain.
            let mut all_trigrams = Vec::new();
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
                        all_trigrams.extend(trigram::extract_from_literal(&text));
                        current_literal.clear();
                    }
                    // Recurse into the non-literal child
                    let sub_plan = decompose_hir(sub, case_insensitive);
                    if let QueryPlan::And(tris) = sub_plan {
                        all_trigrams.extend(tris);
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
                all_trigrams.extend(trigram::extract_from_literal(&text));
            }

            if all_trigrams.is_empty() {
                QueryPlan::MatchAll
            } else {
                QueryPlan::And(all_trigrams)
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
        QueryPlan::And(mut tris) => {
            tris.sort_unstable();
            tris.dedup();
            if tris.is_empty() {
                QueryPlan::MatchAll
            } else {
                QueryPlan::And(tris)
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
        QueryPlan::And(trigrams) => {
            if trigrams.is_empty() {
                return Vec::new();
            }
            // Start with the smallest posting list for efficiency
            let mut lists: Vec<Vec<u32>> = trigrams.iter().map(|&t| lookup(t)).collect();
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
            QueryPlan::And(tris) => {
                assert!(!tris.is_empty(), "should have trigrams");
                // Verify these are lowercase trigrams
                let expected = trigram::extract_from_literal("class alertschema");
                for tri in &expected {
                    assert!(tris.contains(tri), "missing trigram {tri:#010x}");
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
            QueryPlan::And(tris) => {
                assert!(!tris.is_empty(), "should have trigrams");
                let expected = trigram::extract_from_literal("class alertschema");
                for tri in &expected {
                    assert!(tris.contains(tri), "missing trigram {tri:#010x}");
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
}
