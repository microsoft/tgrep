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

/// Build a `Gitignore` matcher rooted at `root`, mirroring the same
/// gitignore semantics that `walker::walk_dir` / `walker::walk_file_metadata`
/// apply during iteration. Loads:
///   * `.git/info/exclude` (if present)
///   * every `.gitignore` file inside the tree
///   * the user's global gitignore (via `GitignoreBuilder`'s defaults)
///
/// Uses `WalkBuilder` to enumerate `.gitignore` files so we automatically
/// skip the `.git` dir and gitignored subtrees while collecting rules.
/// Returns `None` when no rules could be loaded.
pub fn build_matcher(root: &Path) -> Option<Gitignore> {
    use ignore::gitignore::GitignoreBuilder;

    let mut builder = GitignoreBuilder::new(root);

    let info_exclude = root.join(".git").join("info").join("exclude");
    if info_exclude.is_file() {
        let _ = builder.add(&info_exclude);
    }

    // Walk to find every `.gitignore` file. `.gitignore` itself starts
    // with `.` so we cannot use `hidden(true)` (it would filter the very
    // files we are looking for). Instead, walk with hidden=false but
    // filter out the `.git/` subtree explicitly so we don't recurse
    // into git's own metadata. Gitignore-based subtree skipping still
    // applies, so we don't descend into ignored dirs while collecting.
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walker.flatten() {
        if entry.file_name() == ".gitignore" && entry.path().is_file() {
            let _ = builder.add(entry.path());
        }
    }

    let gi = builder.build().ok()?;
    if gi.is_empty() { None } else { Some(gi) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn builds_matcher_from_root_gitignore() {
        let tmp = TempDir::new().unwrap();
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
}
