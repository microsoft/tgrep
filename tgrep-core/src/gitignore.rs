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

/// Thread count for the `.gitignore` enumeration walk.
///
/// Mirrors `walker::walker_thread_count`. The walk is I/O-bound rather than
/// CPU-bound, so the cap exists to avoid thrashing network filesystems, not
/// to match core count.
fn matcher_walk_thread_count() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get().min(12))
}

use ignore::Match;
/// Re-export of `ignore::gitignore::Gitignore` so callers can hold a
/// matcher without taking a direct dependency on the `ignore` crate.
pub use ignore::gitignore::Gitignore;

/// A `.gitignore` found below the repository root, kept anchored to its own
/// directory rather than folded into the root matcher.
struct NestedIgnore {
    /// Directory holding the `.gitignore`, relative to the repo root,
    /// `/`-separated with no trailing slash.
    dir: String,
    matcher: Gitignore,
}

pub struct IgnoreMatcher {
    /// Root-scoped rules: the root `.gitignore`, `.git/info/exclude`, and
    /// `p4ignore.ini`.
    local: Gitignore,
    /// Nested `.gitignore` files, deepest first.
    nested: Vec<NestedIgnore>,
    global: Gitignore,
}

impl IgnoreMatcher {
    pub fn new(local: Gitignore, global: Gitignore) -> Option<Self> {
        Self::with_nested(local, Vec::new(), global)
    }

    /// Build a matcher from root-scoped rules plus `.gitignore` files found
    /// in subdirectories, each paired with its directory relative to the repo
    /// root.
    ///
    /// Nested files **must** stay anchored to their own directory. Folding
    /// them into one root-scoped matcher silently changes what they mean: the
    /// Linux kernel has four `tools/testing/selftests/*/.gitignore` files
    /// containing a bare `*`, which is "ignore everything beside me" in place
    /// but becomes "ignore the entire repository" at the root. That made the
    /// file watcher treat `README` and `kernel/sched/core.c` as ignored and
    /// silently drop every event.
    pub fn with_nested(
        local: Gitignore,
        nested: Vec<(String, Gitignore)>,
        global: Gitignore,
    ) -> Option<Self> {
        let mut nested: Vec<NestedIgnore> = nested
            .into_iter()
            .filter(|(_, matcher)| !matcher.is_empty())
            .map(|(dir, matcher)| NestedIgnore {
                dir: dir.trim_matches('/').to_string(),
                matcher,
            })
            .collect();
        // Deepest first, so the innermost `.gitignore` decides — that is the
        // precedence git applies between levels.
        nested.sort_by(|a, b| {
            b.dir
                .matches('/')
                .count()
                .cmp(&a.dir.matches('/').count())
                .then_with(|| b.dir.len().cmp(&a.dir.len()))
        });

        (!local.is_empty() || !nested.is_empty() || !global.is_empty()).then_some(Self {
            local,
            nested,
            global,
        })
    }

    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let rel = path.to_string_lossy().replace('\\', "/");
        let rel = rel.trim_start_matches("./").trim_start_matches('/');

        for entry in &self.nested {
            // Only consult a nested file for paths beneath its directory, and
            // match against the path *relative to that directory*, which is
            // the scope its patterns were written for.
            let Some(under) = strip_dir_prefix(rel, &entry.dir) else {
                continue;
            };
            match entry
                .matcher
                .matched_path_or_any_parents(Path::new(under), is_dir)
            {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }

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

/// Return `rel` relative to `dir`, or `None` when `rel` is not beneath it.
/// An empty `dir` means the repository root, which matches everything.
fn strip_dir_prefix<'a>(rel: &'a str, dir: &str) -> Option<&'a str> {
    if dir.is_empty() {
        return Some(rel);
    }
    let rest = rel.strip_prefix(dir)?;
    rest.strip_prefix('/')
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

/// Build an [`IgnoreMatcher`] from already-discovered `.gitignore` paths.
///
/// `gitignore_files` must be **absolute** paths under `root` — that is what
/// `walker::walk_dir` yields. Each nested file is anchored by stripping `root`
/// from its parent directory, so repo-relative paths would leave every nested
/// rule out of the matcher.
///
/// Root-level rules (`.git/info/exclude`, `p4ignore.ini`, and a `.gitignore`
/// directly in `root`) share the root matcher. Every deeper `.gitignore` gets
/// its own matcher anchored at its directory, because its patterns are written
/// relative to that directory — see [`IgnoreMatcher::with_nested`].
pub fn matcher_from_gitignore_paths(
    root: &Path,
    gitignore_files: &[std::path::PathBuf],
) -> Option<IgnoreMatcher> {
    use ignore::gitignore::GitignoreBuilder;

    let mut root_builder = GitignoreBuilder::new(root);
    let info_exclude = root.join(".git").join("info").join("exclude");
    if info_exclude.is_file() {
        let _ = root_builder.add(&info_exclude);
    }
    add_p4ignore_rules(&mut root_builder, root);

    let mut nested: Vec<(String, Gitignore)> = Vec::new();
    for path in gitignore_files {
        if !path.is_file() {
            continue;
        }
        let dir = path.parent().unwrap_or(root);
        // A path we can't place relative to the root has unknown scope, and
        // guessing is what broke this before. Skipping it only under-ignores,
        // which is the safe direction, but it is still silent — so make it
        // loud in debug builds rather than quietly dropping nested rules.
        let Ok(rel_dir) = dir.strip_prefix(root) else {
            debug_assert!(
                false,
                "gitignore path {} is not under root {}; \
                 pass absolute paths from the walk, not repo-relative ones",
                path.display(),
                root.display()
            );
            continue;
        };
        let rel_dir = rel_dir.to_string_lossy().replace('\\', "/");
        let rel_dir = rel_dir.trim_matches('/');

        if rel_dir.is_empty() {
            let _ = root_builder.add(path);
            continue;
        }

        let mut builder = GitignoreBuilder::new(dir);
        if builder.add(path).is_some() {
            continue;
        }
        if let Ok(matcher) = builder.build() {
            nested.push((rel_dir.to_string(), matcher));
        }
    }

    let local = root_builder.build().ok()?;
    IgnoreMatcher::with_nested(local, nested, build_global_matcher(root))
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
///
/// The enumeration walk is *parallel*, matching `walker.rs`. It used to be
/// single-threaded, which made this a latency trap on large or
/// network-backed trees: the walk is almost pure I/O wait, so one thread
/// serializes every directory read. On a 289k-file repo on a network drive
/// it took 205 s, against 1.6 s for the parallel stale-check walk over the
/// same tree immediately afterwards. Callers that already have the
/// `.gitignore` paths from their own walk should still prefer
/// `matcher_from_gitignore_paths` and skip this entirely.
pub fn build_matcher(root: &Path) -> Option<IgnoreMatcher> {
    let p4ignore = build_p4ignore_matcher(root).map(std::sync::Arc::new);
    let match_root = root.to_path_buf();

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
        .threads(matcher_walk_thread_count())
        .build_parallel();

    let gitignore_files = std::sync::Mutex::new(Vec::<std::path::PathBuf>::new());
    walker.run(|| {
        let found = &gitignore_files;
        Box::new(move |entry| {
            if let Ok(entry) = entry
                && entry.file_name() == ".gitignore"
                && entry.path().is_file()
            {
                // Ignore poisoning: a panic in another worker must not
                // cascade into every remaining thread.
                found
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(entry.path().to_path_buf());
            }
            ignore::WalkState::Continue
        })
    });

    // Sorting is not needed for correctness: `IgnoreMatcher::with_nested`
    // re-sorts deepest-first, which is what actually establishes git's
    // between-level precedence, and its remaining ties are same-depth
    // same-length directories that anchor to disjoint subtrees and so can
    // never both match one path. But that re-sort is *stable*, so leaving
    // the input in parallel-completion order would let the built matcher
    // vary run to run. Sorting here keeps builds reproducible, for a few
    // hundred paths' worth of cost.
    let mut gitignore_files = gitignore_files
        .into_inner()
        .unwrap_or_else(|e| e.into_inner());
    gitignore_files.sort();

    matcher_from_gitignore_paths(root, &gitignore_files)
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

    /// Regression test for the bug that silently disabled the serve file
    /// watcher on the Linux kernel tree.
    ///
    /// Four `tools/testing/selftests/*/.gitignore` files there contain a bare
    /// `*`, and `arch/riscv/kernel/vdso_cfi/.gitignore` contains `*.c`. When
    /// every `.gitignore` was folded into one root-anchored matcher, those
    /// patterns applied tree-wide, so `README` and `kernel/sched/core.c` both
    /// reported as ignored and the watcher dropped every event.
    #[test]
    fn nested_gitignore_rules_stay_scoped_to_their_directory() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let selftest = root.join("tools/testing/selftests/kvm");
        std::fs::create_dir_all(&selftest).unwrap();
        std::fs::write(selftest.join(".gitignore"), "*\n").unwrap();

        let vdso = root.join("arch/riscv/kernel/vdso_cfi");
        std::fs::create_dir_all(&vdso).unwrap();
        std::fs::write(vdso.join(".gitignore"), "*.c\n").unwrap();

        std::fs::create_dir_all(root.join("kernel/sched")).unwrap();
        std::fs::write(root.join(".gitignore"), "*.o\n").unwrap();
        std::fs::write(root.join("README"), "hello").unwrap();
        std::fs::write(root.join("kernel/sched/core.c"), "int main;").unwrap();
        std::fs::write(vdso.join("vgetrandom.c"), "int x;").unwrap();
        std::fs::write(selftest.join("kvm_util.c"), "int y;").unwrap();

        let matcher = build_matcher(root).expect("matcher should build");

        // Ordinary source outside those directories stays visible.
        assert!(!matcher.is_ignored(Path::new("README"), false));
        assert!(!matcher.is_ignored(Path::new("kernel/sched/core.c"), false));
        assert!(!matcher.is_ignored(Path::new("arch/riscv/kernel/vdso_cfi/vdso.lds"), false));

        // The nested rules still apply where they were written.
        assert!(matcher.is_ignored(Path::new("arch/riscv/kernel/vdso_cfi/vgetrandom.c"), false));
        assert!(matcher.is_ignored(Path::new("tools/testing/selftests/kvm/kvm_util.c"), false));

        // Root rules keep applying tree-wide.
        assert!(matcher.is_ignored(Path::new("kernel/sched/core.o"), false));
    }

    /// The contract is "absolute paths from the walk". Passing repo-relative
    /// ones would silently drop every nested rule, so the guard must fire.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "not under root")]
    fn relative_gitignore_paths_trip_the_debug_guard() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".gitignore"), "*.log\n").unwrap();

        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = std::panic::catch_unwind(|| {
            matcher_from_gitignore_paths(
                Path::new("."),
                &[std::path::PathBuf::from("sub/.gitignore")],
            )
        });
        std::env::set_current_dir(cwd).unwrap();

        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn deeper_gitignore_negation_overrides_a_shallower_ignore() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let inner = root.join("build/keep");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(root.join("build/.gitignore"), "*.log\n").unwrap();
        std::fs::write(inner.join(".gitignore"), "!important.log\n").unwrap();
        std::fs::write(root.join("build/noisy.log"), "x").unwrap();
        std::fs::write(inner.join("important.log"), "x").unwrap();

        let matcher = build_matcher(root).expect("matcher should build");
        assert!(matcher.is_ignored(Path::new("build/noisy.log"), false));
        assert!(!matcher.is_ignored(Path::new("build/keep/important.log"), false));
    }
}
