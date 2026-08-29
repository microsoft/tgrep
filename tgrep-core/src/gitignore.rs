//! Point-query source-control ignore matching for paths.
//!
//! `walk_dir` / `walk_file_metadata` in `walker.rs` get ignore behavior
//! inline as the walker descends. This module is for callers that *don't*
//! walk and instead need to ask "given an arbitrary path, would the
//! indexer have skipped it for ignore-file reasons?".
//!
//! The canonical caller is the file watcher in `tgrep-cli`, which has to
//! answer that per `notify` event without re-walking.

use crate::walker::walker_thread_count;
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

pub const P4IGNORE_FILENAME: &str = "p4ignore.ini";

/// Git's per-directory ignore file.
pub const GITIGNORE_FILENAME: &str = ".gitignore";

/// The VCS-agnostic ignore file honored by ripgrep and friends. Unlike
/// `.gitignore` it applies with or without a git repository.
pub const DOT_IGNORE_FILENAME: &str = ".ignore";

/// Return the ignore files sitting directly inside `dir`, as
/// `(.gitignore, .ignore)`.
///
/// Walks must call this on each directory they descend into rather than
/// collecting the ignore files the walk *yields*. An ignore file matched by its
/// own rules — the common `build/.gitignore` holding `*` — is filtered out of
/// the walk's own output, so collecting yielded entries silently drops that
/// rule. The indexer would still skip the subtree (the walker reads the file
/// even though it won't emit it), but the matcher built from those paths would
/// not know about it, and the watcher would then index a subtree the indexer
/// deliberately excluded.
///
/// Probing directories is also *complete*: a `.gitignore` only matters when the
/// walk descends into its directory, and every such directory is yielded. It
/// avoids duplicates for the same reason — one probe per directory.
pub fn ignore_files_in(dir: &Path) -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
    let gitignore = dir.join(GITIGNORE_FILENAME);
    let dot_ignore = dir.join(DOT_IGNORE_FILENAME);
    (
        gitignore.is_file().then_some(gitignore),
        dot_ignore.is_file().then_some(dot_ignore),
    )
}

use ignore::Match;
/// Re-export of `ignore::gitignore::Gitignore` so callers can hold a
/// matcher without taking a direct dependency on the `ignore` crate.
pub use ignore::gitignore::Gitignore;

/// Relative precedence of ignore sources that live in the *same* directory.
/// `.ignore` outranks `.gitignore`, matching the `ignore` crate's ordering in
/// the indexing walk.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum IgnoreKind {
    /// `.ignore` — consulted first, so it wins over a same-directory
    /// `.gitignore`.
    DotIgnore,
    /// `.gitignore`.
    GitIgnore,
}

/// An ignore file found below the repository root, kept anchored to its own
/// directory rather than folded into the root matcher.
struct NestedIgnore {
    /// Directory holding the ignore file, relative to the repo root,
    /// `/`-separated with no trailing slash.
    dir: String,
    kind: IgnoreKind,
    matcher: Gitignore,
}

/// An ignore file above the served root. `prefix` is the served root relative
/// to the directory that owns the matcher, so a root-relative event path can be
/// queried with the same anchoring the filesystem walk used.
struct AncestorIgnore {
    prefix: String,
    kind: IgnoreKind,
    matcher: Gitignore,
}

pub struct IgnoreMatcher {
    /// Root-scoped custom rules such as `p4ignore.ini`.
    local: Gitignore,
    /// `p4ignore.ini` is a separate walker filter: its ignores are additive,
    /// while its whitelists do not override the standard ignore sources.
    local_is_filter: bool,
    /// Ignore files inside (or at) the served root, deepest first.
    nested: Vec<NestedIgnore>,
    /// Parent `.ignore` / `.gitignore` files, closest directory first.
    ancestors: Vec<AncestorIgnore>,
    /// Repository-local `.git/info/exclude`, below global rules in precedence.
    repo_exclude: Option<(String, Gitignore)>,
    global: Gitignore,
    /// The directory the relative paths handed to [`Self::is_ignored`] are
    /// relative to, needed to rebuild the absolute path
    /// [`CaseInsensitiveIgnore`] matches against.
    root: std::path::PathBuf,
    /// Git's `core.ignorecase` narrowing, when the repository asks for it.
    ///
    /// The indexing walk applies this as a `filter_entry` alongside the
    /// case-sensitive rules, so a matcher without it answers a different
    /// question than the walk did. That gap is not academic: on a Windows
    /// enlistment it left the watcher subscribing to, and indexing, a 13.4 GiB
    /// build artifact the walk had already excluded — and every event under it
    /// re-added a file the next stale check then evicted.
    ignorecase: Option<CaseInsensitiveIgnore>,
}

impl IgnoreMatcher {
    pub fn new(local: Gitignore, global: Gitignore) -> Option<Self> {
        Self::with_nested(local, Vec::new(), global)
    }

    /// Build a matcher from root-scoped rules plus ignore files found in
    /// subdirectories, each paired with its directory relative to the repo
    /// root and the kind of ignore file it came from.
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
        nested: Vec<(String, IgnoreKind, Gitignore)>,
        global: Gitignore,
    ) -> Option<Self> {
        Self::with_all_sources(local, false, nested, Vec::new(), None, global, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn with_all_sources(
        local: Gitignore,
        local_is_filter: bool,
        nested: Vec<(String, IgnoreKind, Gitignore)>,
        ancestors: Vec<(String, IgnoreKind, Gitignore)>,
        repo_exclude: Option<(String, Gitignore)>,
        global: Gitignore,
        ignorecase: Option<CaseInsensitiveIgnore>,
    ) -> Option<Self> {
        let mut nested: Vec<NestedIgnore> = nested
            .into_iter()
            .filter(|(_, _, matcher)| !matcher.is_empty())
            .map(|(dir, kind, matcher)| NestedIgnore {
                dir: dir.trim_matches('/').to_string(),
                kind,
                matcher,
            })
            .collect();
        // Deepest first, so the innermost ignore file decides — that is the
        // precedence git applies between levels. Within one directory,
        // `.ignore` is consulted before `.gitignore` so it wins.
        nested.sort_by(|a, b| {
            b.dir
                .matches('/')
                .count()
                .cmp(&a.dir.matches('/').count())
                .then_with(|| b.dir.len().cmp(&a.dir.len()))
                .then_with(|| a.kind.cmp(&b.kind))
        });

        let ancestors = ancestors
            .into_iter()
            .filter(|(_, _, matcher)| !matcher.is_empty())
            .map(|(prefix, kind, matcher)| AncestorIgnore {
                prefix,
                kind,
                matcher,
            })
            .collect::<Vec<_>>();
        let repo_exclude = repo_exclude.filter(|(_, matcher)| !matcher.is_empty());

        (!local.is_empty()
            || !nested.is_empty()
            || !ancestors.is_empty()
            || repo_exclude.is_some()
            || !global.is_empty()
            || ignorecase.is_some())
        .then_some(Self {
            // `GitignoreBuilder::new(root)` records `root`, and every caller
            // builds `local` from the served root, so this is that root
            // without threading it through three more signatures.
            root: local.path().to_path_buf(),
            local,
            local_is_filter,
            nested,
            ancestors,
            repo_exclude,
            global,
            ignorecase,
        })
    }

    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let rel = path.to_string_lossy().replace('\\', "/");
        let rel = rel.trim_start_matches("./").trim_start_matches('/');

        // The ignore crate resolves each source independently, then gives
        // `.ignore` files precedence over every `.gitignore`, regardless of
        // directory depth.
        let mut standard_decision = None;
        for kind in [IgnoreKind::DotIgnore, IgnoreKind::GitIgnore] {
            for entry in self.nested.iter().filter(|entry| entry.kind == kind) {
                // Only consult a nested file for paths beneath its directory,
                // relative to the directory where its patterns were written.
                let Some(under) = strip_dir_prefix(rel, &entry.dir) else {
                    continue;
                };
                match entry
                    .matcher
                    .matched_path_or_any_parents(Path::new(under), is_dir)
                {
                    Match::Ignore(_) => {
                        standard_decision = Some(true);
                        break;
                    }
                    Match::Whitelist(_) => {
                        standard_decision = Some(false);
                        break;
                    }
                    Match::None => {}
                }
            }
            if standard_decision.is_some() {
                break;
            }

            for entry in self.ancestors.iter().filter(|entry| entry.kind == kind) {
                let path = if entry.prefix.is_empty() {
                    rel.to_string()
                } else {
                    format!("{}/{rel}", entry.prefix)
                };
                match entry
                    .matcher
                    .matched_path_or_any_parents(Path::new(&path), is_dir)
                {
                    Match::Ignore(_) => {
                        standard_decision = Some(true);
                        break;
                    }
                    Match::Whitelist(_) => {
                        standard_decision = Some(false);
                        break;
                    }
                    Match::None => {}
                }
            }
            if standard_decision.is_some() {
                break;
            }
        }

        // `with_nested` historically takes an already-combined root matcher;
        // nested files must retain their deeper-path precedence over it.
        if standard_decision.is_none() && !self.local_is_filter {
            standard_decision = match self.local.matched_path_or_any_parents(path, is_dir) {
                Match::Ignore(_) => Some(true),
                Match::Whitelist(_) => Some(false),
                Match::None => None,
            };
        }

        if standard_decision.is_none()
            && let Some((prefix, matcher)) = &self.repo_exclude
        {
            let path = if prefix.is_empty() {
                rel.to_string()
            } else {
                format!("{prefix}/{rel}")
            };
            match matcher.matched_path_or_any_parents(Path::new(&path), is_dir) {
                Match::Ignore(_) => standard_decision = Some(true),
                Match::Whitelist(_) => standard_decision = Some(false),
                Match::None => {}
            }
        }

        if standard_decision.is_none() {
            standard_decision = match self.global.matched_path_or_any_parents(path, is_dir) {
                Match::Ignore(_) => Some(true),
                Match::Whitelist(_) => Some(false),
                Match::None => None,
            };
        }

        if standard_decision == Some(true) {
            return true;
        }
        if self.local_is_filter
            && self
                .local
                .matched_path_or_any_parents(path, is_dir)
                .is_ignore()
        {
            return true;
        }
        // Applied last, and to whitelisted paths too, because that is where the
        // indexing walk applies it: a `filter_entry` rejection is not undone by
        // a whitelist rule. Both passes must agree on what the tree contains,
        // or the watcher indexes a file the stale check immediately evicts.
        self.ignorecase
            .as_ref()
            .is_some_and(|ignorecase| ignorecase.excludes(&self.root.join(rel), is_dir))
    }

    /// Fingerprint of the tracked paths behind the tracked-file exemption.
    ///
    /// `None` means this matcher has no case-insensitive exemption, so callers
    /// need not poll anything. Index metadata is used internally to avoid
    /// reparsing an unchanged index, but only path membership contributes to
    /// this value.
    pub fn tracked_membership_fingerprint(&self) -> Option<TrackedMembershipFingerprint> {
        self.ignorecase
            .as_ref()
            .map(CaseInsensitiveIgnore::tracked_membership_fingerprint)
    }

    /// Cheap generation token for the worktree-specific Git index.
    pub fn tracked_index_generation(&self) -> Option<TrackedIndexGeneration> {
        self.ignorecase
            .as_ref()
            .map(CaseInsensitiveIgnore::tracked_index_generation)
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

/// Git's repository-wide ignore rules, matched the way git matches them on a
/// case-insensitive filesystem, with git's tracked-file exemption applied.
///
/// # Why this exists
///
/// git sets `core.ignorecase` when it clones onto a filesystem that does not
/// distinguish case, and from then on it matches ignore rules without regard to
/// case. The `ignore` crate always matches case-sensitively, so on such a
/// repository a rule written `QLogs` does not hide a directory named `qlogs` —
/// and everything inside it gets walked, read and indexed even though `git
/// status` never mentions it. On one Windows enlistment that was a single 13.4
/// GiB build artifact making up 71% of the corpus.
///
/// This only ever *adds* exclusions, and only for paths the case-sensitive pass
/// already let through, so it cannot resurrect a file the normal rules hid.
///
/// # The tracked-file exemption
///
/// Matching without regard to case starts catching files that are already
/// tracked, which git would never hide — on that same enlistment, 273 `.JPG`,
/// `.PNG` and `.RLL` files caught by rules written in lower case. git's rule is
/// that ignore rules only decide the fate of files it does not already track,
/// so tracked paths are exempt here too. With the exemption, exactly one file
/// is excluded: the untracked artifact.
///
/// # Limits
///
/// Only the repository's own rules are matched this way — the root
/// `.gitignore` and `.git/info/exclude`. Rules in nested `.gitignore` files are
/// not, because the walk does not know they exist until it reaches their
/// directory, by which point the parent may already have been pruned; nor is
/// the user's global ignore file, whose rules are not repository state. Missing
/// one only leaves a file visible that git would hide, which is the behaviour
/// without any of this.
pub struct CaseInsensitiveIgnore {
    matcher: Gitignore,
    repo_root: std::path::PathBuf,
    /// Loaded on the first path this matcher actually claims, which for most
    /// repositories is never. Reading it costs 163 ms and ~30 MB on a
    /// 299k-file repository, and nothing at all if no rule ever matches.
    ///
    /// Reloaded when the index it was read from changes. A walk builds this
    /// matcher, uses it and drops it, so a snapshot would do; the file watcher
    /// holds one for the life of the server, and there a `git add -f` or a
    /// `git rm --cached` rewrites only `.git/index` — which is hidden, so no
    /// ignore source changes and nothing republishes the matcher. Frozen, the
    /// exemption would keep answering from the tracked set as it stood at
    /// startup, and the watcher would disagree with a fresh walk about which
    /// files exist until the hourly reconcile.
    tracked: std::sync::RwLock<TrackedCache>,
}

/// The tracked-file set, together with the identity of the index it came from.
#[derive(Default)]
struct TrackedCache {
    /// `None` until something is loaded; `Some` even when the load failed, so
    /// an unreadable index is not retried on every path.
    loaded_from: Option<Option<IndexIdentity>>,
    tracked: Option<crate::git_index::TrackedFiles>,
    /// Fingerprint of only the tracked paths this matcher would otherwise hide.
    fingerprint: Option<(usize, u64, u64)>,
}

/// Compact, order-independent identity of the tracked exemption set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackedMembershipFingerprint(Option<(usize, u64, u64)>);

/// Metadata generation of the worktree-specific Git index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackedIndexGeneration(Option<IndexIdentity>);

#[derive(Clone, Debug, PartialEq, Eq)]
struct IndexIdentity {
    modified: std::time::SystemTime,
    created: Option<std::time::SystemTime>,
    len: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

/// Cheap metadata identity of the index file generation.
///
/// git replaces the index by renaming `index.lock` over it, so any rewrite
/// normally lands as a new file instance as well as a new mtime. Creation time
/// covers that replacement on Windows, and device/inode covers it on Unix,
/// including an A→B→A rewrite inside one filesystem mtime tick.
fn index_identity(repo_root: &Path) -> Option<IndexIdentity> {
    let meta = std::fs::metadata(crate::git_index::index_path(repo_root)?).ok()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(IndexIdentity {
        modified: meta.modified().ok()?,
        created: meta.created().ok(),
        len: meta.len(),
        #[cfg(unix)]
        device: meta.dev(),
        #[cfg(unix)]
        inode: meta.ino(),
    })
}

impl CaseInsensitiveIgnore {
    /// Returns `None` unless `root` is in a git repository that sets
    /// `core.ignorecase` and has repository-wide rules to apply.
    ///
    /// `use_gitignore`, `use_exclude` and `use_parents` mirror the flags the
    /// case-sensitive pass was built with. They are not a convenience: this
    /// matcher is only sound as a *narrowing* of that pass, so it must never
    /// consult a source the case-sensitive pass was told to ignore. With
    /// `--no-ignore-vcs` the case-sensitive pass lets everything through, which
    /// would leave this the only thing excluding — the exact opposite of what
    /// the flag asks for.
    pub fn new(
        root: &Path,
        use_gitignore: bool,
        use_exclude: bool,
        use_parents: bool,
    ) -> Option<Self> {
        use ignore::gitignore::GitignoreBuilder;

        if !use_gitignore && !use_exclude {
            return None;
        }

        let repo_root = git_repo_root(root)?.to_path_buf();
        if !crate::git_index::ignores_case(&repo_root) {
            return None;
        }

        // Everything below is repository-wide, so when the walk starts inside a
        // subdirectory these rules reach it only as parent rules. `--no-ignore-parent`
        // switches those off in the case-sensitive pass, and we must follow.
        // `git_repo_root` returns an ancestor of `root`, so this compares exactly.
        if !use_parents && root != repo_root {
            return None;
        }

        let mut builder = GitignoreBuilder::new(&repo_root);
        builder.case_insensitive(true).ok()?;
        if use_gitignore {
            let _ = builder.add(repo_root.join(GITIGNORE_FILENAME));
        }
        if use_exclude && let Some(exclude) = repo_exclude_path(&repo_root) {
            let _ = builder.add(exclude);
        }
        let matcher = builder.build().ok()?;
        if matcher.is_empty() {
            return None;
        }
        Some(Self {
            matcher,
            repo_root,
            tracked: std::sync::RwLock::new(TrackedCache::default()),
        })
    }

    /// Whether git would hide `path`, which must be absolute.
    pub fn excludes(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(relative) = path.strip_prefix(&self.repo_root) else {
            return false;
        };
        if !self
            .matcher
            .matched_path_or_any_parents(relative, is_dir)
            .is_ignore()
        {
            return false;
        }
        // Only now is the index worth reading.
        let relative = relative.to_string_lossy();
        self.tracked_hides(&relative, is_dir)
    }

    fn tracked_membership_fingerprint(&self) -> TrackedMembershipFingerprint {
        let identity = index_identity(&self.repo_root);
        {
            let cache = self.tracked.read().unwrap();
            if cache.loaded_from.as_ref() == Some(&identity) {
                return TrackedMembershipFingerprint(cache.fingerprint);
            }
        }
        let mut cache = self.tracked.write().unwrap();
        self.reload_tracked_if_needed(&mut cache, identity);
        TrackedMembershipFingerprint(cache.fingerprint)
    }

    fn tracked_index_generation(&self) -> TrackedIndexGeneration {
        TrackedIndexGeneration(index_identity(&self.repo_root))
    }

    /// Whether the tracked-file set leaves `relative` hidden.
    ///
    /// `false` when the index cannot be read: with no way to tell tracked from
    /// untracked, excluding could hide real source, so it declines instead.
    fn tracked_hides(&self, relative: &str, is_dir: bool) -> bool {
        let identity = index_identity(&self.repo_root);
        {
            let cache = self.tracked.read().unwrap();
            if cache.loaded_from.as_ref() == Some(&identity) {
                return Self::hides(cache.tracked.as_ref(), relative, is_dir);
            }
        }
        let mut cache = self.tracked.write().unwrap();
        // Another thread may have reloaded it while this one waited.
        self.reload_tracked_if_needed(&mut cache, identity);
        Self::hides(cache.tracked.as_ref(), relative, is_dir)
    }

    fn reload_tracked_if_needed(&self, cache: &mut TrackedCache, identity: Option<IndexIdentity>) {
        if cache.loaded_from.as_ref() == Some(&identity) {
            return;
        }
        cache.tracked = crate::git_index::load_tracked(&self.repo_root);
        cache.fingerprint = cache.tracked.as_ref().map(|tracked| {
            tracked.fingerprint_matching(|path| {
                self.matcher
                    .matched_path_or_any_parents(Path::new(path), false)
                    .is_ignore()
            })
        });
        cache.loaded_from = Some(identity);
    }

    fn hides(
        tracked: Option<&crate::git_index::TrackedFiles>,
        relative: &str,
        is_dir: bool,
    ) -> bool {
        let Some(tracked) = tracked else {
            return false;
        };
        if is_dir {
            // A rule matching a directory does not hide tracked files inside
            // it, so the walk still has to descend.
            !tracked.contains_any_under(relative)
        } else {
            !tracked.contains(relative)
        }
    }
}

/// Build an [`IgnoreMatcher`] from already-discovered `.gitignore` paths.
///
/// `gitignore_files` / `ignore_files` must be **absolute** paths under `root` —
/// that is what `walker::walk_dir` yields. Each nested file is anchored by
/// stripping `root` from its parent directory, so repo-relative paths would
/// leave every nested rule out of the matcher.
///
/// Every `.ignore` and `.gitignore`, including files directly in `root`, keeps
/// its own directory anchoring. Source classes are resolved separately so
/// `.ignore` outranks `.gitignore` across directory levels, matching
/// `WalkBuilder`. `p4ignore.ini` remains a root-scoped custom source.
///
/// Whether `root` sits inside a git repository, detected by scanning `root` and
/// its ancestors for a `.git` entry so a subdirectory of a repo still counts.
///
/// This mirrors the `ignore` crate's `require_git` gate, which is what decides
/// whether `.gitignore` files are honoured at all.
pub fn in_git_repo(root: &Path) -> bool {
    root.ancestors().any(|dir| dir.join(".git").exists())
}

fn git_repo_root(root: &Path) -> Option<&Path> {
    root.ancestors().find(|dir| dir.join(".git").exists())
}

/// The `info/exclude` file whose rules apply to `root`, if there is one.
///
/// Not `.git/info/exclude`. In a linked worktree or a submodule `.git` is a
/// file holding a `gitdir:` pointer, and that directory in turn holds a
/// `commondir` naming the repository every worktree shares — which is where the
/// one `info/exclude` lives. `WalkBuilder` resolves that chain, so a matcher
/// that stopped at the literal path enforced different rules than the walk in
/// exactly the layouts where the two differ, and the watcher would index a file
/// the next stale check evicts.
pub fn repo_exclude_path(root: &Path) -> Option<PathBuf> {
    let git_dir = crate::git_index::git_dir(git_repo_root(root)?)?;
    let common = crate::git_index::common_git_dir(&git_dir);
    let path = common.join("info").join("exclude");
    path.is_file().then_some(path)
}

/// The parent-directory `.ignore` / `.gitignore` files that apply to `root`,
/// closest directory first, paired with the directory that anchors them.
///
/// `WalkBuilder` applies `.ignore` files from every ancestor. Its git boundary
/// is independent: with the default `require_git`, ancestor `.gitignore` files
/// stop after the nearest repository root; with `--no-require-git` they
/// continue to the filesystem root.
pub fn ancestor_ignore_paths(root: &Path, no_require_git: bool) -> Vec<(PathBuf, IgnoreKind)> {
    let repo_root = git_repo_root(root);
    let mut found = Vec::new();
    for dir in root.ancestors().skip(1) {
        for (kind, path, enabled) in [
            (IgnoreKind::DotIgnore, dir.join(DOT_IGNORE_FILENAME), true),
            (
                IgnoreKind::GitIgnore,
                dir.join(GITIGNORE_FILENAME),
                no_require_git || repo_root.is_some_and(|repo| dir.starts_with(repo)),
            ),
        ] {
            if enabled && path.is_file() {
                found.push((path, kind));
            }
        }
    }
    found
}

/// `.gitignore` and `.git/info/exclude` are **git-gated** to match the indexing
/// walk (`WalkBuilder`'s `require_git` default): they apply only when `root` is
/// inside a git repository, detected by scanning `root` and its ancestors for a
/// `.git` entry so a subdirectory of a repo still counts. `.ignore` always
/// applies. Without this gate the watcher would filter events using rules the
/// indexer never applied.
pub fn matcher_from_ignore_paths(
    root: &Path,
    gitignore_files: &[std::path::PathBuf],
    ignore_files: &[std::path::PathBuf],
) -> Option<IgnoreMatcher> {
    matcher_from_ignore_paths_with_options(root, gitignore_files, ignore_files, false)
}

/// Build a matcher from already-discovered ignore files with the same git gate
/// used by a file walk.
pub fn matcher_from_ignore_paths_with_options(
    root: &Path,
    gitignore_files: &[std::path::PathBuf],
    ignore_files: &[std::path::PathBuf],
    no_require_git: bool,
) -> Option<IgnoreMatcher> {
    use ignore::gitignore::GitignoreBuilder;

    // `root` is inside a git repo if it or any ancestor holds a `.git` entry.
    let repo_root = git_repo_root(root);
    let git_rules_enabled = repo_root.is_some() || no_require_git;

    let mut root_builder = GitignoreBuilder::new(root);
    add_p4ignore_rules(&mut root_builder, root);

    let mut nested: Vec<(String, IgnoreKind, Gitignore)> = Vec::new();

    let sources = [
        (IgnoreKind::GitIgnore, gitignore_files, git_rules_enabled),
        (IgnoreKind::DotIgnore, ignore_files, true),
    ];
    for (kind, paths, enabled) in sources {
        if !enabled {
            continue;
        }
        for path in paths {
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
                    "ignore path {} is not under root {}; \
                     pass absolute paths from the walk, not repo-relative ones",
                    path.display(),
                    root.display()
                );
                continue;
            };
            let rel_dir = rel_dir.to_string_lossy().replace('\\', "/");
            let rel_dir = rel_dir.trim_matches('/');

            let mut builder = GitignoreBuilder::new(dir);
            let _ = builder.add(path);
            if let Ok(matcher) = builder.build() {
                nested.push((rel_dir.to_string(), kind, matcher));
            }
        }
    }

    // WalkBuilder applies `.ignore` files from every ancestor, with its own
    // git boundary — see [`ancestor_ignore_paths`].
    let mut ancestors = Vec::new();
    for (path, kind) in ancestor_ignore_paths(root, no_require_git) {
        let Some(dir) = path.parent() else {
            continue;
        };
        let prefix = root
            .strip_prefix(dir)
            .unwrap_or(root)
            .to_string_lossy()
            .replace('\\', "/");
        let mut builder = GitignoreBuilder::new(dir);
        let _ = builder.add(&path);
        if let Ok(matcher) = builder.build() {
            ancestors.push((prefix, kind, matcher));
        }
    }

    let repo_exclude = repo_root.and_then(|repo| {
        let path = repo_exclude_path(root)?;
        let mut builder = GitignoreBuilder::new(repo);
        let _ = builder.add(&path);
        let matcher = builder.build().ok()?;
        let prefix = root
            .strip_prefix(repo)
            .ok()?
            .to_string_lossy()
            .replace('\\', "/");
        Some((prefix, matcher))
    });

    let local = root_builder.build().ok()?;
    let global = if git_rules_enabled {
        build_global_matcher(root)
    } else {
        GitignoreBuilder::new(root).build().ok()?
    };
    // The same narrowing `walker::walk_dir` and `walk_file_metadata` apply as a
    // `filter_entry`. Serving takes no `--no-ignore-vcs` / `--no-ignore-exclude`
    // / `--no-ignore-parent`, so the flags the walk was built with are the
    // defaults; `no_ignore` is handled by the caller, which does not build a
    // matcher at all in that case.
    let ignorecase = CaseInsensitiveIgnore::new(root, true, true, true);
    IgnoreMatcher::with_all_sources(
        local,
        true,
        nested,
        ancestors,
        repo_exclude,
        global,
        ignorecase,
    )
}

/// Convenience wrapper for callers that only have `.gitignore` paths.
pub fn matcher_from_gitignore_paths(
    root: &Path,
    gitignore_files: &[std::path::PathBuf],
) -> Option<IgnoreMatcher> {
    matcher_from_ignore_paths(root, gitignore_files, &[])
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
            if entry.file_name() == ".gitignore" || entry.file_name() == ".ignore" {
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
        .threads(walker_thread_count())
        .build_parallel();

    let gitignore_files = std::sync::Mutex::new(Vec::<std::path::PathBuf>::new());
    let ignore_files = std::sync::Mutex::new(Vec::<std::path::PathBuf>::new());
    walker.run(|| {
        let found = &gitignore_files;
        let found_ignore = &ignore_files;
        Box::new(move |entry| {
            // Probe directories rather than collecting yielded ignore files;
            // see `ignore_files_in` for why the yielded set is incomplete.
            if let Ok(entry) = entry
                && entry.file_type().is_some_and(|ft| ft.is_dir())
            {
                let (gitignore, dot_ignore) = ignore_files_in(entry.path());
                // Ignore poisoning: a panic in another worker must not
                // cascade into every remaining thread.
                if let Some(path) = gitignore {
                    found.lock().unwrap_or_else(|e| e.into_inner()).push(path);
                }
                if let Some(path) = dot_ignore {
                    found_ignore
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(path);
                }
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
    let mut ignore_files = ignore_files.into_inner().unwrap_or_else(|e| e.into_inner());
    ignore_files.sort();

    matcher_from_ignore_paths(root, &gitignore_files, &ignore_files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn collects_a_gitignore_that_its_own_rules_ignore() {
        // `build/.gitignore` holding `*` matches itself, so the walk never
        // yields it. Missing that rule would leave the watcher indexing a
        // subtree the indexer skips.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        let build = root.join("build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(build.join(".gitignore"), "*\n").unwrap();
        std::fs::write(build.join("artifact.o"), "x").unwrap();

        let matcher = build_matcher(root).expect("matcher should build");
        assert!(
            matcher.is_ignored(Path::new("build/artifact.o"), false),
            "a self-ignoring .gitignore must still contribute its rules"
        );
    }

    #[test]
    fn builds_matcher_from_root_gitignore() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "*.log\ntarget/\n").unwrap();
        std::fs::write(tmp.path().join(P4IGNORE_FILENAME), ".gitignore\n").unwrap();
        let gi = build_matcher(tmp.path()).expect("matcher should build");
        assert!(gi.is_ignored(Path::new("build/output.log"), false));
        assert!(gi.is_ignored(Path::new("target/release/foo"), false));
        assert!(!gi.is_ignored(Path::new("src/main.rs"), false));
    }

    #[test]
    fn malformed_ignore_line_does_not_discard_valid_rules() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "*.tmp\n[\n*.log\n").unwrap();

        let matcher = build_matcher(tmp.path()).expect("valid rules should still build");
        assert!(matcher.is_ignored(Path::new("cache.tmp"), false));
        assert!(matcher.is_ignored(Path::new("trace.log"), false));
    }

    #[test]
    fn builds_matcher_from_dot_ignore_without_a_git_repo() {
        // `.ignore` is not git-gated: it applies with no `.git` present.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".ignore"), "*.log\ntarget/\n").unwrap();
        let gi = build_matcher(tmp.path()).expect("matcher should build from .ignore");
        assert!(gi.is_ignored(Path::new("build/output.log"), false));
        assert!(gi.is_ignored(Path::new("target/release/foo"), false));
        assert!(!gi.is_ignored(Path::new("src/main.rs"), false));
    }

    #[test]
    fn gitignore_is_inert_without_a_git_dir() {
        // No `.git`, so `.gitignore` contributes no rules — matching what the
        // indexing walk does, so the watcher can't filter on rules the indexer
        // never applied.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "*.log\ntarget/\n").unwrap();
        assert!(build_matcher(tmp.path()).is_none());
    }

    #[test]
    fn root_dot_ignore_overrides_root_gitignore() {
        // `.gitignore` excludes the tree; `.ignore` re-includes it via a
        // negation. `.ignore` is applied last, so its rule wins.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "logs/\n").unwrap();
        std::fs::write(tmp.path().join(".ignore"), "!logs/\n").unwrap();
        let gi = build_matcher(tmp.path()).expect("matcher should build");
        assert!(
            !gi.is_ignored(Path::new("logs/today.txt"), false),
            ".ignore negation should override the .gitignore rule"
        );
    }

    #[test]
    fn nested_dot_ignore_overrides_gitignore_in_the_same_directory() {
        // Same directory, two ignore files: `.ignore` must be consulted first
        // so its negation wins over the sibling `.gitignore`.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        let sub = root.join("pkg");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(sub.join(".ignore"), "!keep.log\n").unwrap();

        let gi = build_matcher(root).expect("matcher should build");
        assert!(gi.is_ignored(Path::new("pkg/noisy.log"), false));
        assert!(
            !gi.is_ignored(Path::new("pkg/keep.log"), false),
            "a nested .ignore must outrank a .gitignore in the same directory"
        );
    }

    #[test]
    fn returns_none_when_no_rules() {
        let tmp = TempDir::new().unwrap();
        assert!(build_matcher(tmp.path()).is_none());
    }

    #[test]
    fn the_repository_exclude_is_found_through_a_worktree_pointer() {
        // In a linked worktree `.git` is a file naming the worktree's own git
        // directory, which in turn names the repository every worktree shares
        // — and that is where the one `info/exclude` lives. `WalkBuilder`
        // resolves the whole chain, so a matcher that stopped at the literal
        // `.git/info/exclude` enforced different rules than the walk did, and
        // the watcher indexed files the next stale check evicted.
        let tmp = tempfile::tempdir().unwrap();
        let common = tmp.path().join("main").join(".git");
        std::fs::create_dir_all(common.join("info")).unwrap();
        std::fs::write(common.join("info").join("exclude"), "*.secret\n").unwrap();

        let worktree_git = common.join("worktrees").join("wt");
        std::fs::create_dir_all(&worktree_git).unwrap();
        std::fs::write(
            worktree_git.join("commondir"),
            format!("{}\n", common.display()),
        )
        .unwrap();

        let worktree = tmp.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git.display()),
        )
        .unwrap();

        assert_eq!(
            repo_exclude_path(&worktree).as_deref(),
            Some(common.join("info").join("exclude").as_path()),
            "the exclude file has to be reached through the pointer chain"
        );

        let matcher = matcher_from_ignore_paths(&worktree, &[], &[])
            .expect("the exclude file supplies rules");
        assert!(matcher.is_ignored(Path::new("keys.secret"), false));
        assert!(!matcher.is_ignored(Path::new("main.rs"), false));
    }

    #[test]
    fn a_relative_commondir_resolves_against_the_worktree_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let common = tmp.path().join(".git");
        std::fs::create_dir_all(common.join("info")).unwrap();
        std::fs::write(common.join("info").join("exclude"), "*.bin\n").unwrap();

        // What git actually writes: a path relative to the worktree's git dir.
        let worktree_git = common.join("worktrees").join("wt");
        std::fs::create_dir_all(&worktree_git).unwrap();
        std::fs::write(worktree_git.join("commondir"), "../..\n").unwrap();

        let worktree = tmp.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git.display()),
        )
        .unwrap();

        let found = repo_exclude_path(&worktree).expect("resolved through commondir");
        assert!(
            std::fs::canonicalize(&found).unwrap()
                == std::fs::canonicalize(common.join("info").join("exclude")).unwrap(),
            "got {found:?}"
        );
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
    fn with_nested_keeps_deeper_rules_ahead_of_local_rules() {
        use ignore::gitignore::GitignoreBuilder;

        let tmp = TempDir::new().unwrap();
        let mut local = GitignoreBuilder::new(tmp.path());
        local.add_line(None, "*.log").unwrap();
        let mut nested = GitignoreBuilder::new(tmp.path().join("pkg"));
        nested.add_line(None, "!keep.log").unwrap();
        let global = GitignoreBuilder::new(tmp.path()).build().unwrap();
        let matcher = IgnoreMatcher::with_nested(
            local.build().unwrap(),
            vec![(
                "pkg".to_string(),
                IgnoreKind::GitIgnore,
                nested.build().unwrap(),
            )],
            global,
        )
        .unwrap();

        assert!(!matcher.is_ignored(Path::new("pkg/keep.log"), false));
        assert!(matcher.is_ignored(Path::new("pkg/drop.log"), false));
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
        // `.gitignore` rules are git-gated, as in the indexing walk.
        std::fs::create_dir(root.join(".git")).unwrap();

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
        // Open the git gate, or the `.gitignore` path is skipped before the
        // guard under test is ever reached.
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
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
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join("build/.gitignore"), "*.log\n").unwrap();
        std::fs::write(inner.join(".gitignore"), "!important.log\n").unwrap();
        std::fs::write(root.join("build/noisy.log"), "x").unwrap();
        std::fs::write(inner.join("important.log"), "x").unwrap();

        let matcher = build_matcher(root).expect("matcher should build");
        assert!(matcher.is_ignored(Path::new("build/noisy.log"), false));
        assert!(!matcher.is_ignored(Path::new("build/keep/important.log"), false));
    }
}
