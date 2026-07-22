//! Point-query gitignore matching for paths.
//!
//! `walk_dir` / `walk_file_metadata` in `walker.rs` get gitignore behavior
//! for free via `WalkBuilder::git_ignore(true)` — the rules are applied
//! inline as the walker descends. This module is for callers that *don't*
//! walk and instead need to ask "given an arbitrary path, would the
//! indexer have skipped it for gitignore reasons?".
//!
//! The canonical caller is the file watcher in `tgrep-cli`, which has to
//! answer that per `notify` event without re-walking.

use ignore::WalkBuilder;
use std::path::Path;

/// Re-export of `ignore::gitignore::Gitignore` so callers can hold a
/// matcher without taking a direct dependency on the `ignore` crate.
pub use ignore::gitignore::Gitignore;

/// Build a `Gitignore` matcher rooted at `root`, mirroring the ignore
/// semantics that `walker::walk_dir` / `walker::walk_file_metadata` apply
/// during iteration: every `.ignore` file in the tree always applies, and
/// `.gitignore` / `.git/info/exclude` apply only inside a git repository
/// (git-gating is handled by [`crate::walker::build_gitignore_matcher_from_files`]).
/// `.ignore` takes precedence over `.gitignore`. Returns `None` when no rules
/// could be loaded.
pub fn build_matcher(root: &Path) -> Option<Gitignore> {
    // Walk to find every `.gitignore` / `.ignore` file. We can't use
    // `hidden(true)` because those names start with `.` and would be
    // filtered. Instead, walk with hidden=false and use `filter_entry` to
    // skip all dot-prefixed *directories* (`.git`, `.tgrep`, `.vscode`, …) —
    // this avoids unnecessary I/O into hidden subtrees while still letting
    // dot-prefixed *files* through, since `filter_entry` only controls
    // directory descent for directories.
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with('.'))
            } else {
                true
            }
        })
        .build();

    let mut gitignore_paths = Vec::new();
    let mut ignore_paths = Vec::new();
    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        match path.file_name().and_then(|n| n.to_str()) {
            Some(".gitignore") => gitignore_paths.push(path.to_path_buf()),
            Some(".ignore") => ignore_paths.push(path.to_path_buf()),
            _ => {}
        }
    }

    // Delegate so git-gating and precedence live in one place.
    crate::walker::build_gitignore_matcher_from_files(root, &gitignore_paths, &ignore_paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn builds_matcher_from_root_gitignore() {
        let tmp = TempDir::new().unwrap();
        // `.gitignore` only applies inside a git repo.
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "*.log\ntarget/\n").unwrap();
        let gi = build_matcher(tmp.path()).expect("matcher should build");
        assert!(
            gi.matched_path_or_any_parents("build/output.log", false)
                .is_ignore()
        );
        assert!(
            gi.matched_path_or_any_parents("target/release/foo", false)
                .is_ignore()
        );
        assert!(
            !gi.matched_path_or_any_parents("src/main.rs", false)
                .is_ignore()
        );
    }

    #[test]
    fn returns_none_when_no_rules() {
        let tmp = TempDir::new().unwrap();
        assert!(build_matcher(tmp.path()).is_none());
    }

    #[test]
    fn builds_matcher_from_dot_ignore() {
        // `.ignore` applies with no `.git` present, unlike `.gitignore`.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".ignore"), "*.log\ntarget/\n").unwrap();
        let gi = build_matcher(tmp.path()).expect("matcher should build from .ignore");
        assert!(
            gi.matched_path_or_any_parents("build/output.log", false)
                .is_ignore()
        );
        assert!(
            gi.matched_path_or_any_parents("target/release/foo", false)
                .is_ignore()
        );
        assert!(
            !gi.matched_path_or_any_parents("src/main.rs", false)
                .is_ignore()
        );
    }

    #[test]
    fn gitignore_ignored_without_git_dir() {
        // No `.git`: `.gitignore` is git-gated and produces no rules, matching
        // what the indexing walk does.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "*.log\ntarget/\n").unwrap();
        assert!(build_matcher(tmp.path()).is_none());
    }

    #[test]
    fn dot_ignore_takes_precedence_over_gitignore() {
        // `.gitignore` excludes the tree; `.ignore` re-includes it via a
        // negation. Because `.ignore` is added last, its rule wins.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "logs/\n").unwrap();
        std::fs::write(tmp.path().join(".ignore"), "!logs/\n").unwrap();
        let gi = build_matcher(tmp.path()).expect("matcher should build");
        assert!(
            !gi.matched_path_or_any_parents("logs/today.txt", false)
                .is_ignore(),
            ".ignore negation should override the .gitignore rule"
        );
    }
}
