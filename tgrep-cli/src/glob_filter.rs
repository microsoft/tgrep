/// Ripgrep-style glob overrides, shared by indexed filtering and full walks.
use std::path::Path;

use anyhow::Result;
use tgrep_core::walker::{Override, OverrideBuilder};

pub struct GlobFilter {
    overrides: Override,
    globs: Vec<(String, bool)>,
}

impl Default for GlobFilter {
    fn default() -> Self {
        Self {
            overrides: Override::empty(),
            globs: Vec::new(),
        }
    }
}

impl GlobFilter {
    /// Compile `-g/--glob` and `--iglob` patterns into a reusable filter.
    ///
    /// Patterns prefixed with `!` become exclusions; others become inclusions.
    /// `case_insensitive` applies to `globs` only — `iglobs` are always
    /// case-insensitive. Returns an error if any pattern fails to compile.
    pub fn new(globs: &[String], iglobs: &[String], case_insensitive: bool) -> Result<Self> {
        let mut filter = Self {
            globs: globs
                .iter()
                .map(|glob| (glob.replace('\\', "/"), case_insensitive))
                .chain(iglobs.iter().map(|glob| (glob.replace('\\', "/"), true)))
                .collect(),
            ..Default::default()
        };
        filter.overrides = filter.walk_overrides(Path::new(""))?;
        Ok(filter)
    }

    pub fn walk_overrides(&self, root: &Path) -> Result<Override> {
        let mut builder = OverrideBuilder::new(root);
        for (glob, case_insensitive) in &self.globs {
            builder.case_insensitive(*case_insensitive)?.add(glob)?;
        }
        Ok(builder.build()?)
    }

    /// Positive overrides may reinclude ignored files absent from the index.
    pub fn has_includes(&self) -> bool {
        self.overrides.num_whitelists() > 0
    }

    /// Returns true if the glob list is empty (no filtering needed).
    pub fn is_empty(&self) -> bool {
        self.overrides.is_empty()
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
        // A directory exclusion prunes its whole subtree in a filesystem walk.
        for (end, _) in path.match_indices('/') {
            if self.overrides.matched(&path[..end], true).is_ignore() {
                return false;
            }
        }
        !self.overrides.matched(path, false).is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_passes_all() {
        let f = GlobFilter::new(&[], &[], false).unwrap();
        assert!(f.matches("anything"));
        assert!(f.is_empty());
    }

    #[test]
    fn include_patterns() {
        let f = GlobFilter::new(&["**/*.cs".to_string()], &[], false).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(f.matches("bar.cs"));
        assert!(!f.matches("src/foo/bar.rs"));
    }

    #[test]
    fn exclude_patterns() {
        let f = GlobFilter::new(&["!.git".to_string()], &[], false).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(!f.matches(".git"));
        assert!(!f.matches("foo/.git"));
    }

    #[test]
    fn include_and_exclude() {
        let f = GlobFilter::new(
            &["**/*.cs".to_string(), "!**/test/**".to_string()],
            &[],
            false,
        )
        .unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(!f.matches("src/test/bar.cs"));
        assert!(!f.matches("src/foo/bar.rs"));
    }

    #[test]
    fn backslash_normalization() {
        let f = GlobFilter::new(&[r"**\*.cs".to_string()], &[], false).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
    }

    #[test]
    fn globs_are_case_sensitive_by_default() {
        // ripgrep matches `-g` patterns case-sensitively unless asked otherwise.
        let f = GlobFilter::new(&["**/*.CS".to_string()], &[], false).unwrap();
        assert!(!f.matches("src/foo/bar.cs"));
        assert!(f.matches("src/foo/BAR.CS"));
    }

    #[test]
    fn glob_case_insensitive_flag_applies_to_globs() {
        let f = GlobFilter::new(&["**/*.CS".to_string()], &[], true).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(f.matches("src/foo/BAR.CS"));
    }

    #[test]
    fn iglob_patterns_are_always_case_insensitive() {
        let f = GlobFilter::new(&[], &["**/*.CS".to_string()], false).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(f.matches("src/foo/BAR.CS"));
    }

    #[test]
    fn iglob_negation_excludes_case_insensitively() {
        let f = GlobFilter::new(&[], &["!**/*.CS".to_string()], false).unwrap();
        assert!(!f.matches("src/foo/bar.cs"));
        assert!(f.matches("src/foo/bar.rs"));
    }

    #[test]
    fn special_characters_are_literal() {
        // Characters like `+` and `(` should be treated as literal glob chars
        let f = GlobFilter::new(&["**/(test)+.cs".to_string()], &[], false).unwrap();
        assert!(f.matches("src/(test)+.cs"));
        assert!(!f.matches("src/testtest.cs"));
    }

    #[test]
    fn glob_character_class() {
        let f = GlobFilter::new(&["**/*.[ch]".to_string()], &[], false).unwrap();
        assert!(f.matches("src/main.c"));
        assert!(f.matches("src/main.h"));
        assert!(!f.matches("src/main.rs"));
    }

    #[test]
    fn question_mark_wildcard() {
        let f = GlobFilter::new(&["**/*.?s".to_string()], &[], false).unwrap();
        assert!(f.matches("src/foo.cs"));
        assert!(f.matches("src/foo.rs"));
        assert!(f.matches("src/foo.ts"));
        assert!(!f.matches("src/foo.css"));
    }

    #[test]
    fn directory_prefix_pattern() {
        let f = GlobFilter::new(&["src/**".to_string()], &[], false).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(!f.matches("lib/foo/bar.cs"));
    }

    #[test]
    fn bare_extension_matches_at_any_depth() {
        // "*.cs" without path separator should match at any depth (like ripgrep --glob)
        let f = GlobFilter::new(&["*.cs".to_string()], &[], false).unwrap();
        assert!(f.matches("bar.cs"));
        assert!(f.matches("src/foo/bar.cs"));
        assert!(!f.matches("src/foo/bar.rs"));
    }
}
