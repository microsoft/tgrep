/// Shared matching logic used by both the server (serve.rs) and the local
/// search path (search.rs). Extracts the core line-matching and context-window
/// algorithms so both callers avoid duplicating them.

/// Find indices of matching lines in `lines`, respecting `invert_match` and
/// `max_count`. Returns the indices into the `lines` slice.
pub fn find_match_indices(
    lines: &[&str],
    re: &regex::Regex,
    invert_match: bool,
    max_count: Option<usize>,
) -> Vec<usize> {
    let mut indices = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let is_match = re.is_match(line);
        let include = if invert_match { !is_match } else { is_match };
        if include {
            indices.push(i);
            if let Some(max) = max_count
                && indices.len() >= max
            {
                break;
            }
        }
    }
    indices
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

/// Extract match content — full line or only the matched portions.
pub fn match_content(line: &str, re: &regex::Regex, only_matching: bool) -> String {
    if only_matching {
        re.find_iter(line)
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        line.to_string()
    }
}
