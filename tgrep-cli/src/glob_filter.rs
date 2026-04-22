/// Compiled glob filter with pre-compiled regex patterns.
///
/// Glob patterns are compiled to regex once at construction time and reused
/// for all subsequent matches. Supports include/exclude semantics:
/// - Patterns prefixed with `!` act as exclusions
/// - Other patterns act as inclusions
/// - If only exclusions exist, paths pass unless they match an exclusion
/// - If inclusions exist, a path must match at least one inclusion AND
///   must not match any exclusion
use anyhow::Result;
use regex::Regex;

pub struct GlobFilter {
    includes: Vec<Regex>,
    excludes: Vec<Regex>,
}

impl GlobFilter {
    /// Compile a list of glob patterns into a reusable filter.
    /// Patterns prefixed with `!` become exclusions; others become inclusions.
    /// Returns an error if any pattern fails to compile.
    pub fn new(globs: &[String]) -> Result<Self> {
        let mut includes = Vec::new();
        let mut excludes = Vec::new();
        for g in globs {
            if let Some(neg) = g.strip_prefix('!') {
                excludes.push(compile_glob(neg)?);
            } else {
                includes.push(compile_glob(g)?);
            }
        }
        Ok(GlobFilter { includes, excludes })
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
///
/// Escapes all regex metacharacters first, then expands glob tokens (`**`, `*`)
/// into their regex equivalents so that only glob semantics are exposed.
fn compile_glob(pattern: &str) -> Result<Regex> {
    // Normalize backslashes so Windows-style globs match forward-slash index paths
    let pattern = pattern.replace('\\', "/");

    // Preserve glob tokens by replacing them with placeholders before escaping
    let pattern = pattern.replace("**/", "\x01");
    let pattern = pattern.replace("**", "\x02");
    let pattern = pattern.replace('*', "\x03");

    // Escape all regex metacharacters in the literal parts
    let pattern = regex::escape(&pattern);

    // Restore glob tokens as regex equivalents
    let pattern = pattern.replace('\x01', "(.*/)?");
    let pattern = pattern.replace('\x02', ".*");
    let pattern = pattern.replace('\x03', "[^/]*");

    // Anchor with (^|/) so bare patterns like ".git" match at any path level
    Regex::new(&format!("(?i)(^|/){pattern}$"))
        .map_err(|e| anyhow::anyhow!("invalid glob pattern: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_passes_all() {
        let f = GlobFilter::new(&[]).unwrap();
        assert!(f.matches("anything"));
        assert!(f.is_empty());
    }

    #[test]
    fn include_patterns() {
        let f = GlobFilter::new(&["**/*.cs".to_string()]).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(!f.matches("src/foo/bar.rs"));
    }

    #[test]
    fn exclude_patterns() {
        let f = GlobFilter::new(&["!.git".to_string()]).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(!f.matches(".git"));
        assert!(!f.matches("foo/.git"));
    }

    #[test]
    fn include_and_exclude() {
        let f = GlobFilter::new(&["**/*.cs".to_string(), "!**/test/**".to_string()]).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(!f.matches("src/test/bar.cs"));
        assert!(!f.matches("src/foo/bar.rs"));
    }

    #[test]
    fn backslash_normalization() {
        let f = GlobFilter::new(&[r"**\*.cs".to_string()]).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
    }

    #[test]
    fn case_insensitive() {
        let f = GlobFilter::new(&["**/*.CS".to_string()]).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(f.matches("src/foo/BAR.CS"));
    }

    #[test]
    fn regex_metacharacters_are_literal() {
        // Parentheses, brackets, plus, etc. should be treated as literal chars
        let f = GlobFilter::new(&["**/(test)+[0].cs".to_string()]).unwrap();
        assert!(f.matches("src/(test)+[0].cs"));
        assert!(!f.matches("src/testtest0.cs"));
    }
}
