/// .gitignore-aware file walker using the `ignore` crate (same as ripgrep).
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Default maximum file size to index (64 MiB). Larger files are skipped.
///
/// This is a runaway guard, not a cost control. Size is a poor proxy for index
/// cost, because the files that trip a cap are overwhelmingly generated and
/// therefore repetitive: the kernel's 24 MB `dcn_3_2_0_sh_mask.h` contributes
/// 7,263 distinct trigrams, *fewer* than the 10,936 of the 224 KB
/// `fs/ext4/super.c`. Indexing all 489 MB of `drivers/gpu/drm/amd/include/
/// asic_reg` rather than only its sub-1 MiB files grew the index by 5.9 MB —
/// 1.3% of the added content, against 47% for the small files it already held.
///
/// The former 1 MiB cap bought little of that and cost correctness: because
/// oversized files are counted but their paths are never recorded, an indexed
/// search silently returned no match where both `--no-index` and ripgrep found
/// one. What remains here only bounds the pathological case, such as a
/// checked-in database dump that sniffs as text.
///
/// Raising it is not free — [`crate::builder`] reads each file whole, in
/// parallel, so this also raises worst-case transient memory during a build.
/// 64 MiB matches the default external-sort arena, keeping the two bounds in
/// step.
///
/// Search paths that scan the filesystem directly (`--no-index`, `--files`)
/// have no such constraint and default to no limit, matching ripgrep.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;

/// Binary extensions that can be rejected without reading file content.
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "svg", "tiff", "tif", "psd", "raw", "mp3",
    "mp4", "avi", "mkv", "mov", "wav", "flac", "ogg", "wma", "aac", "m4a", "webm", "zip", "tar",
    "gz", "bz2", "xz", "zst", "7z", "rar", "lz4", "lzma", "cab", "exe", "dll", "so", "dylib",
    "obj", "o", "a", "lib", "pdb", "wasm", "class", "jar", "pyc", "pyo", "beam", "pdf", "doc",
    "docx", "xls", "xlsx", "ppt", "pptx", "ttf", "otf", "woff", "woff2", "eot", "bin", "dat", "db",
    "sqlite", "sqlite3",
];

/// Number of parallel walker threads (capped at 12 to avoid diminishing returns).
///
/// Shared with the `.gitignore` enumeration walk in [`crate::gitignore`] so both
/// full-tree walks stay in lock-step: they traverse the same trees under the
/// same I/O constraints, so tuning one without the other would be a bug.
pub(crate) fn walker_thread_count() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get().min(12))
}

/// Check if a directory entry should be skipped based on exclude list.
fn should_skip_dir(entry: &ignore::DirEntry, exclude_dirs: &[String]) -> bool {
    !exclude_dirs.is_empty()
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| exclude_dirs.iter().any(|d| d == name))
}

pub struct WalkResult {
    pub files: Vec<PathBuf>,
    pub gitignore_files: Vec<PathBuf>,
    /// `.ignore` files encountered during the walk. Kept separate from
    /// `gitignore_files` so a matcher can apply them last, giving them
    /// precedence over `.gitignore` — the `ignore` crate's own ordering.
    pub ignore_files: Vec<PathBuf>,
    pub skipped_binary: usize,
    pub skipped_error: usize,
    /// Files rejected only because they exceeded `WalkOptions::max_file_size`.
    ///
    /// Tracked separately from `skipped_binary` so callers can tell the user
    /// that a size limit — not file content — is why a path was not searched.
    /// A silently dropped oversize file is indistinguishable from "no match".
    pub skipped_too_large: usize,
}

pub struct WalkOptions {
    pub include_hidden: bool,
    pub no_ignore: bool,
    /// Skip the binary-extension rejection list and consider every file text.
    pub search_binary: bool,
    /// Follow symbolic links while walking (ripgrep's `--follow`).
    pub follow_links: bool,
    /// Reject files larger than this many bytes. `None` disables the limit.
    pub max_file_size: Option<u64>,
    /// Collect `.gitignore` and `.ignore` file paths encountered during the
    /// walk (into `WalkResult::gitignore_files` / `WalkResult::ignore_files`).
    pub collect_gitignore_files: bool,
    /// Directory names to exclude from walking (e.g., "vendor", "third_party").
    pub exclude_dirs: Vec<String>,
    /// `--max-depth`: descend at most this many directories below each root.
    pub max_depth: Option<usize>,
    /// `--one-file-system`: don't cross file system boundaries.
    pub same_file_system: bool,
    /// `--ignore-file`: extra ignore files, applied in order (later wins).
    pub ignore_files: Vec<PathBuf>,
    /// `--ignore-file-case-insensitive`: match `--ignore-file` globs without
    /// regard to case.
    pub ignore_files_case_insensitive: bool,
    /// `--no-ignore-dot`: don't respect `.ignore` files.
    pub no_ignore_dot: bool,
    /// `--no-ignore-exclude`: don't respect `.git/info/exclude`.
    pub no_ignore_exclude: bool,
    /// `--no-ignore-global`: don't respect the global gitignore.
    pub no_ignore_global: bool,
    /// `--no-ignore-parent`: don't respect ignore files above each root.
    pub no_ignore_parent: bool,
    /// `--no-ignore-vcs`: don't respect `.gitignore` files.
    pub no_ignore_vcs: bool,
    /// `--no-require-git`: respect gitignore rules outside a git repository.
    pub no_require_git: bool,
    /// `--no-ignore-messages`: swallow errors from unparseable ignore files.
    pub no_ignore_messages: bool,
    /// `--threads`: walker thread count. `None` sizes the pool automatically.
    pub threads: Option<usize>,
}

// Hand-written rather than derived: `Option::default()` is `None`, which would
// silently turn the size limit off for every existing caller that builds this
// struct with `..Default::default()` (the index builder and the server among
// them). The default has to keep bounding the index.
impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            include_hidden: false,
            no_ignore: false,
            search_binary: false,
            follow_links: false,
            max_file_size: Some(DEFAULT_MAX_FILE_SIZE),
            collect_gitignore_files: false,
            exclude_dirs: Vec::new(),
            max_depth: None,
            same_file_system: false,
            ignore_files: Vec::new(),
            ignore_files_case_insensitive: false,
            no_ignore_dot: false,
            no_ignore_exclude: false,
            no_ignore_global: false,
            no_ignore_parent: false,
            no_ignore_vcs: false,
            no_require_git: false,
            no_ignore_messages: false,
            threads: None,
        }
    }
}

/// Check if a file extension indicates a binary format.
fn is_binary_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            let lower = ext.to_ascii_lowercase();
            BINARY_EXTENSIONS.iter().any(|&b| b == lower)
        })
}

/// Render a non-fatal ignore-file error for display.
///
/// The error embeds the absolute path the walker was handed, which on Windows
/// is an extended-length path (`\\?\C:\...`) left over from `canonicalize`.
/// Show it relative to the search root instead, so it reads like the paths
/// printed alongside matches rather than an internal Win32 detail.
fn display_ignore_error(err: &ignore::Error, root: &Path) -> String {
    let ignore::Error::WithPath { path, err: inner } = err else {
        return err.to_string();
    };
    let shown = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();
    let shown = if let Some(rest) = shown.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = shown.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        shown
    };
    format!("{shown}: {inner}")
}

/// Walk a directory tree, respecting .gitignore rules (unless disabled).
/// Returns paths of text files suitable for indexing/searching.
///
/// Only rejects files by extension and size here. Content-based binary
/// detection is deferred to the caller (which reads the file anyway),
/// avoiding an extra 8KB read per file during the walk.
pub fn walk_dir(root: &Path, opts: &WalkOptions) -> WalkResult {
    let files = std::sync::Mutex::new(Vec::new());
    let gitignore_files = std::sync::Mutex::new(Vec::new());
    let ignore_files = std::sync::Mutex::new(Vec::new());
    let skipped_binary = std::sync::atomic::AtomicUsize::new(0);
    let skipped_error = std::sync::atomic::AtomicUsize::new(0);
    let skipped_too_large = std::sync::atomic::AtomicUsize::new(0);
    let exclude_dirs: std::sync::Arc<Vec<String>> = std::sync::Arc::new(opts.exclude_dirs.clone());
    let search_binary = opts.search_binary;
    let max_file_size = opts.max_file_size;
    let include_hidden = opts.include_hidden;
    let collect_gitignore_files = opts.collect_gitignore_files;
    let no_ignore_messages = opts.no_ignore_messages;
    let root = root.to_path_buf();
    let p4ignore = (!opts.no_ignore)
        .then(|| crate::gitignore::build_p4ignore_matcher(&root))
        .flatten()
        .map(std::sync::Arc::new);
    let p4ignore_root = root.clone();

    let mut builder = WalkBuilder::new(&root);
    builder
        .hidden(!include_hidden)
        .follow_links(opts.follow_links)
        .max_depth(opts.max_depth)
        .same_file_system(opts.same_file_system)
        .ignore(!opts.no_ignore && !opts.no_ignore_dot)
        .parents(!opts.no_ignore && !opts.no_ignore_parent)
        .require_git(!opts.no_require_git)
        .git_ignore(!opts.no_ignore && !opts.no_ignore_vcs)
        .git_global(!opts.no_ignore && !opts.no_ignore_global)
        .git_exclude(!opts.no_ignore && !opts.no_ignore_exclude)
        .ignore_case_insensitive(opts.ignore_files_case_insensitive)
        .filter_entry(move |entry| {
            if entry.file_name() == ".gitignore" {
                return true;
            }
            let Some(matcher) = &p4ignore else {
                return true;
            };
            let Ok(relative) = entry.path().strip_prefix(&p4ignore_root) else {
                return true;
            };
            !matcher.is_ignored(
                relative,
                entry.file_type().is_some_and(|kind| kind.is_dir()),
            )
        })
        .threads(opts.threads.unwrap_or_else(walker_thread_count).max(1));
    // `--ignore-file` is applied even under `--no-ignore`: the user asked for
    // these rules explicitly, so only `--no-ignore-files` turns them off.
    for path in &opts.ignore_files {
        if let Some(err) = builder.add_ignore(path)
            && !opts.no_ignore_messages
        {
            // `err` already renders as `<path>: line N: ...`, so printing the
            // path again would repeat it.
            eprintln!("tgrep: {err}");
        }
    }
    let walker = builder.build_parallel();

    walker.run(|| {
        let exclude = exclude_dirs.clone();
        let files = &files;
        let gitignore_files = &gitignore_files;
        let ignore_files = &ignore_files;
        let skipped_binary = &skipped_binary;
        let skipped_error = &skipped_error;
        let skipped_too_large = &skipped_too_large;
        let walk_root = &root;
        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    skipped_error.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                }
            };
            // An entry can carry a non-fatal error from parsing an ignore file
            // found while descending into it. ripgrep reports these (gated on
            // the same flags) and keeps walking, rather than treating a
            // malformed `.gitignore` as a reason to stop.
            if let Some(err) = entry.error()
                && !no_ignore_messages
            {
                eprintln!("tgrep: {}", display_ignore_error(err, walk_root));
            }

            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                if should_skip_dir(&entry, &exclude) {
                    return ignore::WalkState::Skip;
                }
                // Probe each directory we descend into for its ignore files
                // instead of collecting the ones the walk yields, which omits
                // any ignore file matched by its own rules. See
                // `gitignore::ignore_files_in`.
                if collect_gitignore_files {
                    let (gitignore, dot_ignore) = crate::gitignore::ignore_files_in(entry.path());
                    if let Some(path) = gitignore {
                        gitignore_files.lock().unwrap().push(path);
                    }
                    if let Some(path) = dot_ignore {
                        ignore_files.lock().unwrap().push(path);
                    }
                }
                return ignore::WalkState::Continue;
            }

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return ignore::WalkState::Continue;
            }

            let path = entry.path();

            if !search_binary && is_binary_extension(path) {
                skipped_binary.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return ignore::WalkState::Continue;
            }

            // Size is checked independently of `search_binary` so `--text`
            // and `--max-filesize` stay orthogonal, the way ripgrep treats
            // `-a` and `--max-filesize`.
            if let Some(limit) = max_file_size
                && let Ok(meta) = entry.metadata()
                && meta.len() > limit
            {
                skipped_too_large.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return ignore::WalkState::Continue;
            }

            files.lock().unwrap().push(entry.into_path());
            ignore::WalkState::Continue
        })
    });

    WalkResult {
        files: files.into_inner().unwrap(),
        gitignore_files: gitignore_files.into_inner().unwrap(),
        ignore_files: ignore_files.into_inner().unwrap(),
        skipped_binary: skipped_binary.into_inner(),
        skipped_error: skipped_error.into_inner(),
        skipped_too_large: skipped_too_large.into_inner(),
    }
}

/// Build a point-query ignore matcher from `.gitignore` and `.ignore` files
/// discovered by an existing walk, avoiding a second full-tree discovery pass.
///
/// `.ignore` is applied after `.gitignore` so it takes precedence, matching the
/// `ignore` crate's ordering in the indexing walk.
pub fn build_gitignore_matcher_from_files(
    root: &Path,
    gitignore_files: &[PathBuf],
    ignore_files: &[PathBuf],
) -> Option<crate::gitignore::IgnoreMatcher> {
    crate::gitignore::matcher_from_ignore_paths(root, gitignore_files, ignore_files)
}

/// Filesystem metadata for a single file (no content read).
pub struct FileMeta {
    pub relative_path: String,
    pub mtime: u64,
    pub size: u64,
}

/// Result of [`walk_file_metadata`]: per-file metadata plus the `.gitignore` /
/// `.ignore` files discovered along the way, from which a watcher ignore
/// matcher can be built (see [`build_gitignore_matcher_from_files`]) without a
/// second full-tree walk.
pub struct MetaWalkResult {
    pub files: Vec<FileMeta>,
    pub gitignore_files: Vec<PathBuf>,
    pub ignore_files: Vec<PathBuf>,
}

/// Options for [`walk_file_metadata`].
///
/// A struct rather than positional `bool`s: the metadata walk needs both
/// `no_ignore` and `no_require_git`, and two adjacent bare booleans at a call
/// site are trivially transposed.
#[derive(Debug, Clone)]
pub struct MetaWalkOptions {
    /// Directory names to prune from the walk.
    pub exclude_dirs: Vec<String>,
    /// `--no-ignore`: don't respect any ignore files.
    pub no_ignore: bool,
    /// `--no-require-git`: respect gitignore rules outside a git repository.
    ///
    /// Must track the indexing walk's setting. If the two disagree, startup
    /// stale-file detection compares a file set built under one rule against an
    /// index built under the other, and every extra file looks new.
    pub no_require_git: bool,
    /// Skip files larger than this. `None` means no limit.
    ///
    /// Must track [`WalkOptions::max_file_size`] for the same reason as
    /// `no_require_git`. A file the index holds but this walk skips is absent
    /// from [`MetaWalkResult::files`], which the stale check reads as *deleted*
    /// — so a lower limit here silently evicts oversized files from an index
    /// that was built with a higher one.
    pub max_file_size: Option<u64>,
}

impl Default for MetaWalkOptions {
    /// Hand-written rather than derived: `#[derive(Default)]` would make
    /// `max_file_size` `None`, which means *no limit* and so would quietly
    /// invert the default rather than matching [`WalkOptions::default`].
    fn default() -> Self {
        Self {
            exclude_dirs: Vec::new(),
            no_ignore: false,
            no_require_git: false,
            max_file_size: Some(DEFAULT_MAX_FILE_SIZE),
        }
    }
}

/// Walk a directory tree collecting filesystem metadata (mtime, size) plus the
/// `.gitignore` / `.ignore` files encountered. No file content is read — this
/// is used for stale file detection on startup.
///
/// Hidden entries are skipped, matching the indexing walk. Ignore files are
/// still found because every directory the walk descends into is probed
/// explicitly via [`crate::gitignore::ignore_files_in`], which also catches
/// ignore files that their own rules would have filtered out of the walk.
pub fn walk_file_metadata(root: &Path, opts: &MetaWalkOptions) -> MetaWalkResult {
    let no_ignore = opts.no_ignore;
    let max_file_size = opts.max_file_size;
    let results = std::sync::Mutex::new(Vec::new());
    let gitignore_files = std::sync::Mutex::new(Vec::new());
    let ignore_files = std::sync::Mutex::new(Vec::new());
    let exclude: std::sync::Arc<Vec<String>> = std::sync::Arc::new(opts.exclude_dirs.clone());
    let p4ignore = (!no_ignore)
        .then(|| crate::gitignore::build_p4ignore_matcher(root))
        .flatten()
        .map(std::sync::Arc::new);
    let match_root = root.to_path_buf();
    let root = root.to_path_buf();

    let walker = WalkBuilder::new(&root)
        .hidden(true)
        .require_git(!opts.no_require_git)
        .git_ignore(!no_ignore)
        .git_global(!no_ignore)
        .git_exclude(!no_ignore)
        .filter_entry(move |entry| {
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

    walker.run(|| {
        let exclude = exclude.clone();
        let root = root.clone();
        let results = &results;
        let gitignore_files = &gitignore_files;
        let ignore_files = &ignore_files;
        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return ignore::WalkState::Continue,
            };

            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                if should_skip_dir(&entry, &exclude) {
                    return ignore::WalkState::Skip;
                }
                // Probing each descended directory finds ignore files the walk
                // itself filters out; see `gitignore::ignore_files_in`. It also
                // keeps `hidden(true)` intact, so the metadata set is unchanged.
                let (gitignore, dot_ignore) = crate::gitignore::ignore_files_in(entry.path());
                if let Some(path) = gitignore {
                    gitignore_files.lock().unwrap().push(path);
                }
                if let Some(path) = dot_ignore {
                    ignore_files.lock().unwrap().push(path);
                }
                return ignore::WalkState::Continue;
            }

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return ignore::WalkState::Continue;
            }

            let path = entry.path();

            if is_binary_extension(path) {
                return ignore::WalkState::Continue;
            }

            let rel_path = match path.strip_prefix(&root) {
                Ok(p) => p.to_string_lossy().replace('\\', "/"),
                Err(_) => return ignore::WalkState::Continue,
            };

            if let Ok(meta) = entry.metadata() {
                if max_file_size.is_some_and(|limit| meta.len() > limit) {
                    return ignore::WalkState::Continue;
                }
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                results.lock().unwrap().push(FileMeta {
                    relative_path: rel_path,
                    mtime,
                    size: meta.len(),
                });
            }

            ignore::WalkState::Continue
        })
    });

    MetaWalkResult {
        files: results.into_inner().unwrap(),
        gitignore_files: gitignore_files.into_inner().unwrap(),
        ignore_files: ignore_files.into_inner().unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Create a temp directory with a structure for exclude testing:
    ///   testdata/
    ///     src/
    ///       main.rs
    ///     vendor/
    ///       dep.rs
    ///     third_party/
    ///       lib.rs
    ///     README.md
    fn setup_fixture() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("testdata");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("vendor")).unwrap();
        fs::create_dir_all(root.join("third_party")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("vendor/dep.rs"), "pub fn dep() {}").unwrap();
        fs::write(root.join("third_party/lib.rs"), "pub fn lib() {}").unwrap();
        fs::write(root.join("README.md"), "# hello").unwrap();
        dir
    }

    fn sorted_filenames(result: &WalkResult, root: &Path) -> Vec<String> {
        let mut names: Vec<String> = result
            .files
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn walk_dir_no_excludes_returns_all_files() {
        let dir = setup_fixture();
        let root = dir.path().join("testdata");
        let result = walk_dir(&root, &WalkOptions::default());
        let names = sorted_filenames(&result, &root);
        assert_eq!(
            names,
            vec![
                "README.md",
                "src/main.rs",
                "third_party/lib.rs",
                "vendor/dep.rs"
            ]
        );
    }

    #[test]
    fn walk_dir_exclude_single_dir() {
        let dir = setup_fixture();
        let root = dir.path().join("testdata");
        let result = walk_dir(
            &root,
            &WalkOptions {
                exclude_dirs: vec!["vendor".to_string()],
                ..Default::default()
            },
        );
        let names = sorted_filenames(&result, &root);
        assert!(names.contains(&"src/main.rs".to_string()));
        assert!(names.contains(&"third_party/lib.rs".to_string()));
        assert!(!names.contains(&"vendor/dep.rs".to_string()));
    }

    #[test]
    fn walk_dir_exclude_multiple_dirs() {
        let dir = setup_fixture();
        let root = dir.path().join("testdata");
        let result = walk_dir(
            &root,
            &WalkOptions {
                exclude_dirs: vec!["vendor".to_string(), "third_party".to_string()],
                ..Default::default()
            },
        );
        let names = sorted_filenames(&result, &root);
        assert_eq!(names, vec!["README.md", "src/main.rs"]);
    }

    #[test]
    fn walk_dir_exclude_nonexistent_dir_is_noop() {
        let dir = setup_fixture();
        let root = dir.path().join("testdata");
        let all = walk_dir(&root, &WalkOptions::default());
        let with_bogus = walk_dir(
            &root,
            &WalkOptions {
                exclude_dirs: vec!["nonexistent".to_string()],
                ..Default::default()
            },
        );
        assert_eq!(
            sorted_filenames(&all, &root),
            sorted_filenames(&with_bogus, &root),
        );
    }

    #[test]
    fn walk_dir_exclude_skips_nested_files() {
        let dir = setup_fixture();
        let root = dir.path().join("testdata");
        // Add a nested file inside vendor
        fs::create_dir_all(root.join("vendor/sub")).unwrap();
        fs::write(root.join("vendor/sub/nested.rs"), "fn nested() {}").unwrap();

        let result = walk_dir(
            &root,
            &WalkOptions {
                exclude_dirs: vec!["vendor".to_string()],
                ..Default::default()
            },
        );
        let names = sorted_filenames(&result, &root);
        assert!(!names.iter().any(|n| n.starts_with("vendor/")));
    }

    #[test]
    fn walk_dir_can_collect_gitignore_files_without_indexing_them() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("testdata");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(
            root.join(crate::gitignore::P4IGNORE_FILENAME),
            ".gitignore\np4ignore.ini\n",
        )
        .unwrap();
        fs::write(root.join("src").join(".gitignore"), "*.tmp\n").unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();

        let result = walk_dir(
            &root,
            &WalkOptions {
                collect_gitignore_files: true,
                ..Default::default()
            },
        );
        let names = sorted_filenames(&result, &root);
        let mut gitignores: Vec<_> = result
            .gitignore_files
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        gitignores.sort();

        assert_eq!(names, vec!["src/main.rs"]);
        assert_eq!(gitignores, vec![".gitignore", "src/.gitignore"]);
    }

    #[test]
    fn walk_dir_collects_and_indexes_gitignore_when_hidden_included() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("testdata");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();

        let result = walk_dir(
            &root,
            &WalkOptions {
                include_hidden: true,
                collect_gitignore_files: true,
                ..Default::default()
            },
        );
        let names = sorted_filenames(&result, &root);
        let gitignores: Vec<_> = result
            .gitignore_files
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();

        assert_eq!(names, vec![".gitignore", "src/main.rs"]);
        assert_eq!(gitignores, vec![".gitignore"]);
    }

    #[test]
    fn build_gitignore_matcher_from_discovered_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("testdata");
        fs::create_dir_all(root.join("src")).unwrap();
        // `.gitignore` rules only apply inside a git repo.
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join("src").join(".gitignore"), "*.tmp\n").unwrap();

        let walk = walk_dir(
            &root,
            &WalkOptions {
                collect_gitignore_files: true,
                ..Default::default()
            },
        );
        let gi =
            build_gitignore_matcher_from_files(&root, &walk.gitignore_files, &walk.ignore_files)
                .expect("matcher should build from discovered .gitignore files");

        assert!(gi.is_ignored(Path::new("build/output.log"), false));
        assert!(gi.is_ignored(Path::new("src/cache.tmp"), false));
        assert!(!gi.is_ignored(Path::new("src/main.rs"), false));
    }

    #[test]
    fn walk_dir_collects_dot_ignore_files_separately() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("testdata");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join(".ignore"), "*.log\n").unwrap();
        fs::write(root.join("src").join(".ignore"), "*.tmp\n").unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();

        let result = walk_dir(
            &root,
            &WalkOptions {
                collect_gitignore_files: true,
                ..Default::default()
            },
        );
        let mut ignores: Vec<_> = result
            .ignore_files
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        ignores.sort();

        // `.ignore` files are collected but not indexed, and stay out of
        // `gitignore_files` so precedence can be applied.
        assert_eq!(sorted_filenames(&result, &root), vec!["src/main.rs"]);
        assert_eq!(ignores, vec![".ignore", "src/.ignore"]);
        assert!(result.gitignore_files.is_empty());
    }

    #[test]
    fn dot_ignore_applies_without_a_git_repo() {
        // No `.git`, so `.gitignore` is inert but `.ignore` still applies —
        // this is what the indexing walk does.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("testdata");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".ignore"), "*.log\nbuild/\n").unwrap();
        fs::write(root.join(".gitignore"), "*.rs\n").unwrap();

        let walk = walk_dir(
            &root,
            &WalkOptions {
                collect_gitignore_files: true,
                ..Default::default()
            },
        );
        let gi =
            build_gitignore_matcher_from_files(&root, &walk.gitignore_files, &walk.ignore_files)
                .expect("matcher should build from discovered .ignore files");

        assert!(gi.is_ignored(Path::new("server/output.log"), false));
        assert!(gi.is_ignored(Path::new("build/artifact.bin"), false));
        // Git-gated: no `.git`, so the `.gitignore` rule must not apply.
        assert!(!gi.is_ignored(Path::new("src/main.rs"), false));
    }

    #[test]
    fn gitignore_applies_in_subdir_of_git_repo() {
        // `.git` lives in an ancestor of `root`, not `root` itself, as when
        // serving a subdirectory of a repository.
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        let root = tmp.path().join("sub");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();

        let walk = walk_dir(
            &root,
            &WalkOptions {
                collect_gitignore_files: true,
                ..Default::default()
            },
        );
        let gi =
            build_gitignore_matcher_from_files(&root, &walk.gitignore_files, &walk.ignore_files)
                .expect("`.gitignore` should apply in a subdirectory of a git repo");
        assert!(gi.is_ignored(Path::new("build/out.log"), false));
    }

    #[test]
    fn walk_file_metadata_collects_ignore_files_but_omits_them_from_metadata() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("testdata");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join(".ignore"), "*.tmp\n").unwrap();
        fs::write(root.join("src").join(".ignore"), "*.bak\n").unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();

        let result = walk_file_metadata(&root, &MetaWalkOptions::default());

        // The dot-files themselves stay out of the metadata set, exactly as
        // they did under the previous `hidden(true)` walk.
        let names: Vec<_> = result
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        assert_eq!(names, vec!["src/main.rs"]);

        let rel = |v: &[PathBuf]| {
            let mut out: Vec<_> = v
                .iter()
                .map(|p| {
                    p.strip_prefix(&root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect();
            out.sort();
            out
        };
        assert_eq!(rel(&result.gitignore_files), vec![".gitignore"]);
        assert_eq!(rel(&result.ignore_files), vec![".ignore", "src/.ignore"]);
    }

    #[test]
    fn walk_file_metadata_excludes_dirs() {
        let dir = setup_fixture();
        let root = dir.path().join("testdata");

        let all = walk_file_metadata(&root, &MetaWalkOptions::default()).files;
        let excluded = walk_file_metadata(
            &root,
            &MetaWalkOptions {
                exclude_dirs: vec!["vendor".to_string()],
                ..Default::default()
            },
        )
        .files;

        assert!(all.iter().any(|f| f.relative_path.starts_with("vendor/")));
        assert!(
            !excluded
                .iter()
                .any(|f| f.relative_path.starts_with("vendor/"))
        );
        assert!(excluded.iter().any(|f| f.relative_path == "src/main.rs"));
    }

    #[test]
    fn walks_respect_root_p4ignore() {
        let dir = setup_fixture();
        let root = dir.path().join("testdata");
        fs::write(
            root.join(crate::gitignore::P4IGNORE_FILENAME),
            "vendor\nthird_party\\*.rs\n",
        )
        .unwrap();

        let walk = walk_dir(&root, &WalkOptions::default());
        let names = sorted_filenames(&walk, &root);
        assert!(!names.iter().any(|name| name.starts_with("vendor/")));
        assert!(!names.contains(&"third_party/lib.rs".to_string()));
        assert!(names.contains(&"src/main.rs".to_string()));

        let metadata = walk_file_metadata(&root, &MetaWalkOptions::default()).files;
        assert!(
            !metadata
                .iter()
                .any(|file| file.relative_path.starts_with("vendor/"))
        );
        assert!(
            !metadata
                .iter()
                .any(|file| file.relative_path == "third_party/lib.rs")
        );
    }

    #[test]
    fn no_ignore_disables_p4ignore_for_walks_and_metadata() {
        let dir = setup_fixture();
        let root = dir.path().join("testdata");
        fs::write(root.join(crate::gitignore::P4IGNORE_FILENAME), "vendor\n").unwrap();

        let walk = walk_dir(
            &root,
            &WalkOptions {
                no_ignore: true,
                ..Default::default()
            },
        );
        assert!(
            sorted_filenames(&walk, &root)
                .iter()
                .any(|name| name == "vendor/dep.rs")
        );

        let metadata = walk_file_metadata(
            &root,
            &MetaWalkOptions {
                no_ignore: true,
                ..Default::default()
            },
        )
        .files;
        assert!(
            metadata
                .iter()
                .any(|file| file.relative_path == "vendor/dep.rs")
        );
    }

    /// `--no-require-git` lifts the `ignore` crate's rule that `.gitignore`
    /// only applies inside a git repository. Without it, non-git enlistments
    /// (Perforce/Source Depot trees, source drops) index their build output.
    #[test]
    fn no_require_git_applies_gitignore_outside_a_git_repo() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Deliberately no `.git` anywhere.
        fs::create_dir_all(root.join("build")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join(".gitignore"), "build/\n*.log\n").unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("build").join("out.rs"), "generated\n").unwrap();
        fs::write(root.join("noisy.log"), "log\n").unwrap();

        let gated = sorted_filenames(&walk_dir(root, &WalkOptions::default()), root);
        assert!(
            gated.contains(&"build/out.rs".to_string()),
            "default is git-gated, matching ripgrep: {gated:?}"
        );
        assert!(gated.contains(&"noisy.log".to_string()));

        let lifted = sorted_filenames(
            &walk_dir(
                root,
                &WalkOptions {
                    no_require_git: true,
                    ..Default::default()
                },
            ),
            root,
        );
        assert_eq!(
            lifted,
            vec!["src/main.rs".to_string()],
            "`.gitignore` must apply once the git gate is lifted"
        );
    }

    /// The metadata walk feeds startup stale-file detection. If it stayed
    /// git-gated while the index build honoured `--no-require-git`, every
    /// ignored file would look new on every start.
    #[test]
    fn metadata_walk_honours_no_require_git() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("build").join("out.rs"), "generated\n").unwrap();

        let gated: Vec<_> = walk_file_metadata(root, &MetaWalkOptions::default())
            .files
            .into_iter()
            .map(|f| f.relative_path)
            .collect();
        assert!(
            gated.iter().any(|p| p.starts_with("build/")),
            "default stays git-gated: {gated:?}"
        );

        let lifted: Vec<_> = walk_file_metadata(
            root,
            &MetaWalkOptions {
                no_require_git: true,
                ..Default::default()
            },
        )
        .files
        .into_iter()
        .map(|f| f.relative_path)
        .collect();
        assert_eq!(lifted, vec!["src/main.rs".to_string()]);
    }

    #[test]
    fn metadata_walk_honours_a_raised_max_file_size() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("small.txt"), "small").unwrap();
        std::fs::write(root.join("big.txt"), vec![b'x'; 2 * 1024 * 1024]).unwrap();

        let names = |opts: &MetaWalkOptions| {
            let mut v: Vec<String> = walk_file_metadata(root, opts)
                .files
                .into_iter()
                .map(|f| f.relative_path)
                .collect();
            v.sort();
            v
        };

        // A cap below the file size hides it. Pinned explicitly rather than
        // taken from the default, so the test states the behaviour it checks
        // instead of tracking whatever the default happens to be.
        assert_eq!(
            names(&MetaWalkOptions {
                max_file_size: Some(1024 * 1024),
                ..Default::default()
            }),
            vec!["small.txt"]
        );

        // The default is well above a couple of megabytes, so an ordinary
        // large source file is indexed rather than silently dropped.
        assert_eq!(
            names(&MetaWalkOptions::default()),
            vec!["big.txt", "small.txt"]
        );

        // Raising the cap must reveal it. If it does not, the stale check reads
        // the file as deleted and evicts it from an index built with this cap.
        assert_eq!(
            names(&MetaWalkOptions {
                max_file_size: Some(10 * 1024 * 1024),
                ..Default::default()
            }),
            vec!["big.txt", "small.txt"]
        );

        // `None` means no limit, matching `WalkOptions`.
        assert_eq!(
            names(&MetaWalkOptions {
                max_file_size: None,
                ..Default::default()
            }),
            vec!["big.txt", "small.txt"]
        );
    }

    #[test]
    fn meta_walk_options_default_matches_the_indexing_walk() {
        // A derived `Default` would give `None` here, which means *no limit* —
        // the opposite of the indexing walk's default.
        assert_eq!(
            MetaWalkOptions::default().max_file_size,
            WalkOptions::default().max_file_size
        );
    }
}
