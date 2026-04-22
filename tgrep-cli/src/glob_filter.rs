/// Compiled glob filter backed by the `globset` crate (same engine as ripgrep).
///
/// Glob patterns are compiled once at construction time and reused for all
/// subsequent matches. Supports include/exclude semantics:
/// - Patterns prefixed with `!` act as exclusions
/// - Other patterns act as inclusions
/// - If only exclusions exist, paths pass unless they match an exclusion
/// - If inclusions exist, a path must match at least one inclusion AND
///   must not match any exclusion
use anyhow::Result;
use globset::{GlobBuilder, GlobMatcher};

pub struct GlobFilter {
    includes: Vec<GlobMatcher>,
    excludes: Vec<GlobMatcher>,
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
        if self.is_empty() {
            return true;
        }
        // Normalize backslashes only when needed (index paths use forward slashes)
        let normalized;
        let path = if path.contains('\\') {
            normalized = path.replace('\\', "/");
            &*normalized
        } else {
            path
        };
        for m in &self.excludes {
            if m.is_match(path) {
                return false;
            }
        }
        if self.includes.is_empty() {
            return true;
        }
        self.includes.iter().any(|m| m.is_match(path))
    }
}

/// Compile a single glob pattern into a `GlobMatcher`.
fn compile_glob(pattern: &str) -> Result<GlobMatcher> {
    // Normalize backslashes so Windows-style globs work uniformly
    let pattern = pattern.replace('\\', "/");
    // Patterns without a path separator should match at any depth,
    // e.g. "*.rs" behaves like "**/*.rs" (consistent with ripgrep --glob)
    let pattern = if !pattern.contains('/') {
        format!("**/{pattern}")
    } else {
        pattern
    };
    let glob = GlobBuilder::new(&pattern)
        .case_insensitive(true)
        .literal_separator(true)
        .build()?;
    Ok(glob.compile_matcher())
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
        assert!(f.matches("bar.cs"));
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
    fn special_characters_are_literal() {
        // Characters like `+` and `(` should be treated as literal glob chars
        let f = GlobFilter::new(&["**/(test)+.cs".to_string()]).unwrap();
        assert!(f.matches("src/(test)+.cs"));
        assert!(!f.matches("src/testtest.cs"));
    }

    #[test]
    fn glob_character_class() {
        let f = GlobFilter::new(&["**/*.[ch]".to_string()]).unwrap();
        assert!(f.matches("src/main.c"));
        assert!(f.matches("src/main.h"));
        assert!(!f.matches("src/main.rs"));
    }

    #[test]
    fn question_mark_wildcard() {
        let f = GlobFilter::new(&["**/*.?s".to_string()]).unwrap();
        assert!(f.matches("src/foo.cs"));
        assert!(f.matches("src/foo.rs"));
        assert!(f.matches("src/foo.ts"));
        assert!(!f.matches("src/foo.css"));
    }

    #[test]
    fn directory_prefix_pattern() {
        let f = GlobFilter::new(&["src/**".to_string()]).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(!f.matches("lib/foo/bar.cs"));
    }

    #[test]
    fn bare_extension_matches_at_any_depth() {
        // "*.cs" without path separator should match at any depth (like ripgrep --glob)
        let f = GlobFilter::new(&["*.cs".to_string()]).unwrap();
        assert!(f.matches("bar.cs"));
        assert!(f.matches("src/foo/bar.cs"));
        assert!(!f.matches("src/foo/bar.rs"));
    }
}
