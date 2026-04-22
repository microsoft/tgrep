/// Compiled glob filter with pre-compiled regex patterns.
///
/// Glob patterns are compiled to regex once at construction time and reused
/// for all subsequent matches. Supports include/exclude semantics:
/// - Patterns prefixed with `!` act as exclusions
/// - Other patterns act as inclusions
/// - If only exclusions exist, paths pass unless they match an exclusion
/// - If inclusions exist, a path must match at least one inclusion AND
///   must not match any exclusion
use regex::Regex;

pub struct GlobFilter {
    includes: Vec<Regex>,
    excludes: Vec<Regex>,
}

impl GlobFilter {
    /// Compile a list of glob patterns into a reusable filter.
    /// Patterns prefixed with `!` become exclusions; others become inclusions.
    pub fn new(globs: &[String]) -> Self {
        let mut includes = Vec::new();
        let mut excludes = Vec::new();
        for g in globs {
            if let Some(neg) = g.strip_prefix('!') {
                if let Some(re) = compile_glob(neg) {
                    excludes.push(re);
                }
            } else if let Some(re) = compile_glob(g) {
                includes.push(re);
            }
        }
        GlobFilter { includes, excludes }
    }

    /// Returns true if the glob list is empty (no filtering needed).
    pub fn is_empty(&self) -> bool {
        self.includes.is_empty() && self.excludes.is_empty()
    }

    /// Check if a path passes this glob filter.
    pub fn matches(&self, path: &str) -> bool {
        for re in &self.excludes {
            if re.is_match(path) {
                return false;
            }
        }
        if self.includes.is_empty() {
            return true;
        }
        self.includes.iter().any(|re| re.is_match(path))
    }
}

/// Compile a single glob pattern to a regex.
fn compile_glob(pattern: &str) -> Option<Regex> {
    // Normalize backslashes so Windows-style globs match forward-slash index paths
    let pattern = pattern.replace('\\', "/");
    let pattern = pattern.replace('.', r"\.");
    // Use placeholders to avoid double-replacement of '*' inside '**' expansions
    let pattern = pattern.replace("**/", "\x01/");
    let pattern = pattern.replace("**", "\x02");
    let pattern = pattern.replace('*', "[^/]*");
    let pattern = pattern.replace("\x01/", "(.*/)?");
    let pattern = pattern.replace('\x02', ".*");
    // Anchor with (^|/) so bare patterns like ".git" match at any path level
    Regex::new(&format!("(?i)(^|/){pattern}$")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_passes_all() {
        let f = GlobFilter::new(&[]);
        assert!(f.matches("anything"));
        assert!(f.is_empty());
    }

    #[test]
    fn include_patterns() {
        let f = GlobFilter::new(&["**/*.cs".to_string()]);
        assert!(f.matches("src/foo/bar.cs"));
        assert!(!f.matches("src/foo/bar.rs"));
    }

    #[test]
    fn exclude_patterns() {
        let f = GlobFilter::new(&["!.git".to_string()]);
        assert!(f.matches("src/foo/bar.cs"));
        assert!(!f.matches(".git"));
        assert!(!f.matches("foo/.git"));
    }

    #[test]
    fn include_and_exclude() {
        let f = GlobFilter::new(&["**/*.cs".to_string(), "!**/test/**".to_string()]);
        assert!(f.matches("src/foo/bar.cs"));
        assert!(!f.matches("src/test/bar.cs"));
        assert!(!f.matches("src/foo/bar.rs"));
    }

    #[test]
    fn backslash_normalization() {
        let f = GlobFilter::new(&[r"**\*.cs".to_string()]);
        assert!(f.matches("src/foo/bar.cs"));
    }

    #[test]
    fn case_insensitive() {
        let f = GlobFilter::new(&["**/*.CS".to_string()]);
        assert!(f.matches("src/foo/bar.cs"));
        assert!(f.matches("src/foo/BAR.CS"));
    }
}
