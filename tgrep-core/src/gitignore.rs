//! Point-query source-control ignore matching for paths.
//!
//! `walk_dir` / `walk_file_metadata` in `walker.rs` get ignore behavior
//! inline as the walker descends. This module is for callers that *don't*
//! walk and instead need to ask "given an arbitrary path, would the
//! indexer have skipped it for ignore-file reasons?".
//!
//! The canonical caller is the file watcher in `tgrep-cli`, which has to
//! answer that per `notify` event without re-walking.

use ignore::WalkBuilder;
use std::path::Path;

pub const P4IGNORE_FILENAME: &str = "p4ignore.ini";

use ignore::Match;
/// Re-export of `ignore::gitignore::Gitignore` so callers can hold a
/// matcher without taking a direct dependency on the `ignore` crate.
pub use ignore::gitignore::Gitignore;

pub struct IgnoreMatcher {
    local: Gitignore,
    global: Gitignore,
}

impl IgnoreMatcher {
    pub fn new(local: Gitignore, global: Gitignore) -> Option<Self> {
        (!local.is_empty() || !global.is_empty()).then_some(Self { local, global })
    }

    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        match self.local.matched_path_or_any_parents(path, is_dir) {
            Match::Ignore(_) => true,
            Match::Whitelist(_) => false,
            Match::None => self
                .global
                .matched_path_or_any_parents(path, is_dir)
                .is_ignore(),
        }
    }
}

pub fn build_global_matcher(root: &Path) -> Gitignore {
    let builder = ignore::gitignore::GitignoreBuilder::new(root);
    builder.build_global().0
}

fn p4ignore_lines(root: &Path) -> Option<(std::path::PathBuf, Vec<String>)> {
    let path = root.join(P4IGNORE_FILENAME);
    let contents = std::fs::read_to_string(&path).ok()?;
    let lines = contents
        .lines()
        .map(|line| line.replace('\\', "/"))
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .collect();
    Some((path, lines))
}

/// Add root-level `p4ignore.ini` rules to a gitignore-compatible builder.
///
/// Perforce ignore files commonly use Windows path separators. The `ignore`
/// crate expects `/` in patterns on every platform, so normalize separators
/// before adding each rule. Comments, globs, and `!` negations then retain
/// their usual ignore-file semantics.
pub fn add_p4ignore_rules(builder: &mut ignore::gitignore::GitignoreBuilder, root: &Path) -> bool {
    let Some((path, lines)) = p4ignore_lines(root) else {
        return false;
    };

    let mut added = false;
    for line in lines {
        if builder.add_line(Some(path.clone()), &line).is_ok() {
            added = true;
        }
    }
    added
}

pub struct P4IgnoreMatcher {
    matcher: Gitignore,
    reinclude_prefixes: Vec<String>,
}

impl P4IgnoreMatcher {
    pub fn is_ignored(&self, relative: &Path, is_dir: bool) -> bool {
        if is_dir {
            let directory = relative.to_string_lossy().replace('\\', "/");
            let directory = directory.trim_matches('/');
            if self.reinclude_prefixes.iter().any(|prefix| {
                prefix == directory
                    || prefix
                        .strip_prefix(directory)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            }) {
                return false;
            }
        }
        self.matcher
            .matched_path_or_any_parents(relative, is_dir)
            .is_ignore()
    }
}

/// Build a matcher containing only root-level `p4ignore.ini` rules.
pub fn build_p4ignore_matcher(root: &Path) -> Option<P4IgnoreMatcher> {
    use ignore::gitignore::GitignoreBuilder;

    let (_, lines) = p4ignore_lines(root)?;
    let mut builder = GitignoreBuilder::new(root);
    if !add_p4ignore_rules(&mut builder, root) {
        return None;
    }
    let matcher = builder.build().ok()?;
    let reinclude_prefixes = lines
        .iter()
        .filter_map(|line| line.strip_prefix('!'))
        .map(|pattern| pattern.trim_start_matches('/'))
        .filter_map(|pattern| {
            let literal_end = pattern.find(['*', '?', '[']).unwrap_or(pattern.len());
            let literal = pattern[..literal_end].trim_end_matches('/');
            (!literal.is_empty()).then(|| literal.to_string())
        })
        .collect();

    (!matcher.is_empty()).then_some(P4IgnoreMatcher {
        matcher,
        reinclude_prefixes,
    })
}

/// Build a `Gitignore` matcher rooted at `root`, mirroring the same
/// ignore semantics that `walker::walk_dir` / `walker::walk_file_metadata`
/// apply during iteration. Loads:
///   * `.git/info/exclude` (if present)
///   * every `.gitignore` file inside the tree
///   * the user's global gitignore (via `GitignoreBuilder`'s defaults)
///   * root-level `p4ignore.ini`
///
/// Uses `WalkBuilder` to enumerate `.gitignore` files so we automatically
/// skip the `.git` dir and gitignored subtrees while collecting rules.
/// Returns `None` when no rules could be loaded.
pub fn build_matcher(root: &Path) -> Option<IgnoreMatcher> {
    use ignore::gitignore::GitignoreBuilder;

    let mut builder = GitignoreBuilder::new(root);
    let p4ignore = build_p4ignore_matcher(root).map(std::sync::Arc::new);
    let match_root = root.to_path_buf();

    let info_exclude = root.join(".git").join("info").join("exclude");
    if info_exclude.is_file() {
        let _ = builder.add(&info_exclude);
    }
    add_p4ignore_rules(&mut builder, root);

    // Walk to find every `.gitignore` file. We can't use `hidden(true)`
    // because `.gitignore` itself starts with `.` and would be filtered.
    // Instead, walk with hidden=false and use `filter_entry` to skip
    // all dot-prefixed *directories* (`.git`, `.tgrep`, `.vscode`, …) —
    // this avoids unnecessary I/O into hidden subtrees while still
    // letting dot-prefixed *files* like `.gitignore` through, since
    // `filter_entry` only controls directory descent for directories.
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(move |entry| {
            // Allow files (we only care about .gitignore among them).
            // For directories, skip any that start with '.'.
            if entry.file_type().is_some_and(|ft| ft.is_dir())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with('.'))
            {
                return false;
            }
            if entry.file_name() == ".gitignore" {
                return true;
            }

            let Some(matcher) = &p4ignore else {
                return true;
            };
            let Ok(relative) = entry.path().strip_prefix(&match_root) else {
                return true;
            };
            !matcher.is_ignored(
                relative,
                entry.file_type().is_some_and(|kind| kind.is_dir()),
            )
        })
        .build();
    for entry in walker.flatten() {
        if entry.file_name() == ".gitignore" && entry.path().is_file() {
            let _ = builder.add(entry.path());
        }
    }

    let local = builder.build().ok()?;
    IgnoreMatcher::new(local, build_global_matcher(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn builds_matcher_from_root_gitignore() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "*.log\ntarget/\n").unwrap();
        std::fs::write(tmp.path().join(P4IGNORE_FILENAME), ".gitignore\n").unwrap();
        let gi = build_matcher(tmp.path()).expect("matcher should build");
        assert!(gi.is_ignored(Path::new("build/output.log"), false));
        assert!(gi.is_ignored(Path::new("target/release/foo"), false));
        assert!(!gi.is_ignored(Path::new("src/main.rs"), false));
    }

    #[test]
    fn returns_none_when_no_rules() {
        let tmp = TempDir::new().unwrap();
        assert!(build_matcher(tmp.path()).is_none());
    }

    #[test]
    fn local_whitelist_overrides_global_ignore() {
        use ignore::gitignore::GitignoreBuilder;

        let tmp = TempDir::new().unwrap();
        let mut local = GitignoreBuilder::new(tmp.path());
        local.add_line(None, "!keep.log").unwrap();
        let mut global = GitignoreBuilder::new(tmp.path());
        global.add_line(None, "*.log").unwrap();
        let matcher = IgnoreMatcher::new(local.build().unwrap(), global.build().unwrap()).unwrap();

        assert!(!matcher.is_ignored(Path::new("keep.log"), false));
        assert!(matcher.is_ignored(Path::new("drop.log"), false));
    }

    #[test]
    fn builds_matcher_from_windows_style_p4ignore() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(P4IGNORE_FILENAME),
            "# generated files\nDerivedDataCache\nBuild\\XboxOne\nbin\\*.xml\n*.pdb\n",
        )
        .unwrap();

        let matcher = build_p4ignore_matcher(tmp.path()).expect("matcher should build");
        assert!(matcher.is_ignored(Path::new("Game/DerivedDataCache/cache.dat"), false));
        assert!(matcher.is_ignored(Path::new("Build/XboxOne/game.exe"), false));
        assert!(matcher.is_ignored(Path::new("bin/generated.xml"), false));
        assert!(matcher.is_ignored(Path::new("symbols/game.pdb"), false));
        assert!(!matcher.is_ignored(Path::new("src/main.cpp"), false));
    }

    #[test]
    fn p4ignore_reinclude_keeps_parent_directory_walkable() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(P4IGNORE_FILENAME),
            "Build\\XboxOne\n!Build\\XboxOne\\*.template\n",
        )
        .unwrap();

        let matcher = build_p4ignore_matcher(tmp.path()).expect("matcher should build");
        assert!(!matcher.is_ignored(Path::new("Build"), true));
        assert!(!matcher.is_ignored(Path::new("Build/XboxOne"), true));
        assert!(matcher.is_ignored(Path::new("Build/XboxOne/game.exe"), false));
        assert!(!matcher.is_ignored(Path::new("Build/XboxOne/game.template"), false));
    }
}
