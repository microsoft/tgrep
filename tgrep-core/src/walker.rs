/// .gitignore-aware file walker using the `ignore` crate (same as ripgrep).
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// The size limit a walk applies when the caller does not name one.
///
/// 64 MiB, which is a deliberate divergence from ripgrep's "no limit". The
/// divergence is affordable because tgrep is not a one-shot scanner: a file
/// that a walk picks up is also a file the index carries and re-reads on every
/// query that its trigrams make a candidate for, so an outlier's cost is paid
/// repeatedly rather than once.
///
/// Measured on a 292,911-file Microsoft Substrate enlistment, where a single
/// 13.41 GiB generated build artifact is **71% of all searchable bytes**:
///
/// | | uncapped | 64 MiB cap |
/// |---|---|---|
/// | cold index build | 214.5 s | 64.2 s |
/// | warm query, same pattern | 21.30 s | 0.55 s |
/// | files the query matched | 2,990 | 2,989 |
///
/// A 39x query win and a 3.3x build win for one lost match in one generated
/// file. The cap excluded 2 files of 292,911.
///
/// Note what this is *not* buying. The cap is no longer what bounds memory —
/// files past [`crate::builder`]'s mapping threshold are memory-mapped, so
/// their pages are file-backed and reclaimable. And size remains a poor proxy
/// for *index* cost, because oversized files are overwhelmingly generated and
/// therefore repetitive: the kernel's 24 MB `dcn_3_2_0_sh_mask.h` contributes
/// 7,263 distinct trigrams, *fewer* than the 10,936 of the 224 KB
/// `fs/ext4/super.c`. What the cap buys is bounded *scan* time, and it buys it
/// where the distribution is worst.
///
/// The cost is real and is the reason this constant is documented at length:
/// an oversized file is counted but its path is never recorded, so a match
/// inside one is reported as no match, with exit status 0. Two things keep
/// that from being silent:
///
/// - `--no-max-filesize` restores the uncapped, ripgrep-identical behaviour.
/// - A file named directly on the command line is never dropped by the
///   *default* cap, only by one the user asked for. Pointing at a file is an
///   unambiguous request to search it.
pub const DEFAULT_MAX_FILE_SIZE: Option<u64> = Some(64 * 1024 * 1024);

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
    /// Files admitted by traversal rules and the size cap before binary
    /// extension filtering. This is the complete set used by `--files`.
    pub listed_files: Vec<PathBuf>,
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

// Hand-written rather than derived so the size limit is a deliberate choice
// rather than whatever `Option::default()` happens to be. See
// [`DEFAULT_MAX_FILE_SIZE`], which diverges from ripgrep.
impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            include_hidden: false,
            no_ignore: false,
            search_binary: false,
            follow_links: false,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
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
///
/// Public so the watcher can apply the same rule the walk does. A file the
/// walk rejected here must not be inserted into the index by an incremental
/// update, or the two disagree about what the index contains.
pub fn is_binary_extension(path: &Path) -> bool {
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

/// Git's own case-insensitive ignore matching, when the repository asks for it.
///
/// Both walks build this the same way on purpose. They must agree on what the
/// tree contains: the indexing walk decides what goes in, and the stale check
/// decides what is missing and should be evicted. A file one includes and the
/// other does not is indexed and then immediately deleted, every single time —
/// the failure mode pinned by `tgrep-cli/tests/serve_max_filesize.rs`.
///
/// The flags must be the *same* ones handed to `WalkBuilder`, because this is
/// only ever sound as a narrowing of the case-sensitive pass. A source that
/// pass was told to skip must be skipped here too, or a `--no-ignore-*` flag
/// would be silently undone.
///
/// See [`crate::gitignore::CaseInsensitiveIgnore`] for what it matches and why.
fn git_ignorecase_filter(
    root: &Path,
    no_ignore: bool,
    no_ignore_vcs: bool,
    no_ignore_exclude: bool,
    no_ignore_parent: bool,
) -> Option<std::sync::Arc<crate::gitignore::CaseInsensitiveIgnore>> {
    (!no_ignore)
        .then(|| {
            crate::gitignore::CaseInsensitiveIgnore::new(
                root,
                !no_ignore_vcs,
                !no_ignore_exclude,
                !no_ignore_parent,
            )
        })
        .flatten()
        .map(std::sync::Arc::new)
}

/// Walk a directory tree, respecting .gitignore rules (unless disabled).
/// Returns paths of text files suitable for indexing/searching.
///
/// Only rejects files by extension and size here. Content-based binary
/// detection is deferred to the caller (which reads the file anyway),
/// avoiding an extra 8KB read per file during the walk.
pub fn walk_dir(root: &Path, opts: &WalkOptions) -> WalkResult {
    let ignorecase = git_ignorecase_filter(
        root,
        opts.no_ignore,
        opts.no_ignore_vcs,
        opts.no_ignore_exclude,
        opts.no_ignore_parent,
    );
    walk_dir_with_ignorecase(root, opts, ignorecase)
}

/// Walk files using a supplied immutable tracked-file exemption snapshot.
///
/// Callers that build a point-query matcher from the result can share this
/// value so the walk and publication make every decision from one snapshot.
pub fn walk_dir_with_ignorecase(
    root: &Path,
    opts: &WalkOptions,
    ignorecase: Option<std::sync::Arc<crate::gitignore::CaseInsensitiveIgnore>>,
) -> WalkResult {
    let files = std::sync::Mutex::new(Vec::new());
    let listed_files = std::sync::Mutex::new(Vec::new());
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
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            if let Some(ignorecase) = &ignorecase
                && ignorecase.excludes(entry.path(), is_dir)
            {
                return false;
            }
            let Some(matcher) = &p4ignore else {
                return true;
            };
            let Ok(relative) = entry.path().strip_prefix(&p4ignore_root) else {
                return true;
            };
            !matcher.is_ignored(relative, is_dir)
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
        let listed_files = &listed_files;
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

            let binary_extension = !search_binary && is_binary_extension(path);
            let too_large = max_file_size
                .is_some_and(|limit| entry.metadata().is_ok_and(|meta| meta.len() > limit));
            if !too_large {
                listed_files.lock().unwrap().push(path.to_path_buf());
            }

            if binary_extension {
                skipped_binary.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return ignore::WalkState::Continue;
            }

            // Size is checked independently of `search_binary` so `--text`
            // and `--max-filesize` stay orthogonal, the way ripgrep treats
            // `-a` and `--max-filesize`.
            if too_large {
                skipped_too_large.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return ignore::WalkState::Continue;
            }

            files.lock().unwrap().push(entry.into_path());
            ignore::WalkState::Continue
        })
    });

    WalkResult {
        files: files.into_inner().unwrap(),
        listed_files: listed_files.into_inner().unwrap(),
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
    no_require_git: bool,
) -> Option<crate::gitignore::IgnoreMatcher> {
    crate::gitignore::matcher_from_ignore_paths_with_options(
        root,
        gitignore_files,
        ignore_files,
        no_require_git,
    )
}

/// Build a point-query matcher sharing the immutable tracked-file exemption
/// used by the walk that discovered these ignore files.
pub fn build_gitignore_matcher_from_files_with_ignorecase(
    root: &Path,
    gitignore_files: &[PathBuf],
    ignore_files: &[PathBuf],
    no_require_git: bool,
    ignorecase: Option<std::sync::Arc<crate::gitignore::CaseInsensitiveIgnore>>,
) -> Option<crate::gitignore::IgnoreMatcher> {
    crate::gitignore::matcher_from_ignore_paths_with_options_and_ignorecase(
        root,
        gitignore_files,
        ignore_files,
        no_require_git,
        ignorecase,
    )
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
    /// Root-relative files admitted before binary extension filtering.
    pub listed_files: Vec<String>,
    pub gitignore_files: Vec<PathBuf>,
    pub ignore_files: Vec<PathBuf>,
    /// Entries or metadata reads the walk could not inspect.
    pub skipped_error: usize,
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
    /// Hand-written rather than derived so the size limit stays an explicit
    /// decision that matches [`WalkOptions::default`] rather than an accident
    /// of `#[derive(Default)]`.
    fn default() -> Self {
        Self {
            exclude_dirs: Vec::new(),
            no_ignore: false,
            no_require_git: false,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
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
    let ignorecase = git_ignorecase_filter(root, opts.no_ignore, false, false, false);
    walk_file_metadata_with_ignorecase(root, opts, ignorecase)
}

/// Walk metadata using an immutable case-insensitive tracked-file exemption
/// that can also be shared with the resulting point-query matcher.
pub fn walk_file_metadata_with_ignorecase(
    root: &Path,
    opts: &MetaWalkOptions,
    ignorecase: Option<std::sync::Arc<crate::gitignore::CaseInsensitiveIgnore>>,
) -> MetaWalkResult {
    let no_ignore = opts.no_ignore;
    let max_file_size = opts.max_file_size;
    let results = std::sync::Mutex::new(Vec::new());
    let listed_files = std::sync::Mutex::new(Vec::new());
    let gitignore_files = std::sync::Mutex::new(Vec::new());
    let ignore_files = std::sync::Mutex::new(Vec::new());
    let skipped_error = std::sync::atomic::AtomicUsize::new(0);
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
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            if let Some(ignorecase) = &ignorecase
                && ignorecase.excludes(entry.path(), is_dir)
            {
                return false;
            }
            let Some(matcher) = &p4ignore else {
                return true;
            };
            let Ok(relative) = entry.path().strip_prefix(&match_root) else {
                return true;
            };
            !matcher.is_ignored(relative, is_dir)
        })
        .threads(walker_thread_count())
        .build_parallel();

    walker.run(|| {
        let exclude = exclude.clone();
        let root = root.clone();
        let results = &results;
        let listed_files = &listed_files;
        let gitignore_files = &gitignore_files;
        let ignore_files = &ignore_files;
        let skipped_error = &skipped_error;
        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    skipped_error.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                }
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
            let rel_path = match path.strip_prefix(&root) {
                Ok(p) => p.to_string_lossy().replace('\\', "/"),
                Err(_) => return ignore::WalkState::Continue,
            };

            if is_binary_extension(path) {
                let too_large = max_file_size
                    .is_some_and(|limit| entry.metadata().is_ok_and(|meta| meta.len() > limit));
                if !too_large {
                    listed_files.lock().unwrap().push(rel_path);
                }
                return ignore::WalkState::Continue;
            }

            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(_) => {
                    skipped_error.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                }
            };
            if max_file_size.is_some_and(|limit| meta.len() > limit) {
                return ignore::WalkState::Continue;
            }
            listed_files.lock().unwrap().push(rel_path.clone());
            let stamp = crate::meta::file_stamp(&meta);
            results.lock().unwrap().push(FileMeta {
                relative_path: rel_path,
                mtime: stamp.mtime,
                size: stamp.size,
            });

            ignore::WalkState::Continue
        })
    });

    MetaWalkResult {
        files: results.into_inner().unwrap(),
        listed_files: listed_files.into_inner().unwrap(),
        gitignore_files: gitignore_files.into_inner().unwrap(),
        ignore_files: ignore_files.into_inner().unwrap(),
        skipped_error: skipped_error.into_inner(),
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

    fn sorted_listed_filenames(result: &WalkResult, root: &Path) -> Vec<String> {
        let mut names: Vec<String> = result
            .listed_files
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
    fn walk_dir_retains_binary_extensions_for_file_listing() {
        let dir = setup_fixture();
        let root = dir.path().join("testdata");
        fs::write(root.join("asset.bin"), [0, 1, 2]).unwrap();

        let result = walk_dir(&root, &WalkOptions::default());
        assert!(!sorted_filenames(&result, &root).contains(&"asset.bin".to_string()));
        assert!(sorted_listed_filenames(&result, &root).contains(&"asset.bin".to_string()));
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
        let gi = build_gitignore_matcher_from_files(
            &root,
            &walk.gitignore_files,
            &walk.ignore_files,
            false,
        )
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
        let gi = build_gitignore_matcher_from_files(
            &root,
            &walk.gitignore_files,
            &walk.ignore_files,
            false,
        )
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
        fs::create_dir(tmp.path().join(".git").join("info")).unwrap();
        fs::write(
            tmp.path().join(".gitignore"),
            "/sub/parent.txt\n/sub/git-precedence.txt\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join(".ignore"),
            "/sub/parent.tmp\n/sub/precedence.txt\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join(".git").join("info").join("exclude"),
            "/sub/info.bin\n",
        )
        .unwrap();
        let root = tmp.path().join("sub");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".gitignore"), "*.log\n!precedence.txt\n").unwrap();
        fs::write(root.join(".ignore"), "!git-precedence.txt\n").unwrap();

        let walk = walk_dir(
            &root,
            &WalkOptions {
                collect_gitignore_files: true,
                ..Default::default()
            },
        );
        let gi = build_gitignore_matcher_from_files(
            &root,
            &walk.gitignore_files,
            &walk.ignore_files,
            false,
        )
        .expect("`.gitignore` should apply in a subdirectory of a git repo");
        assert!(gi.is_ignored(Path::new("build/out.log"), false));
        assert!(gi.is_ignored(Path::new("parent.txt"), false));
        assert!(gi.is_ignored(Path::new("parent.tmp"), false));
        assert!(gi.is_ignored(Path::new("info.bin"), false));
        assert!(gi.is_ignored(Path::new("precedence.txt"), false));
        assert!(!gi.is_ignored(Path::new("git-precedence.txt"), false));
    }

    #[test]
    fn parent_ignore_crosses_repository_boundary_but_gitignore_does_not() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "/repo/from-parent-git.txt\n").unwrap();
        fs::write(tmp.path().join(".ignore"), "/repo/from-parent-dot.txt\n").unwrap();

        let root = tmp.path().join("repo");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("from-parent-git.txt"), "kept\n").unwrap();
        fs::write(root.join("from-parent-dot.txt"), "ignored\n").unwrap();

        let options = MetaWalkOptions::default();
        let walk = walk_file_metadata(&root, &options);
        let paths = walk
            .files
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"from-parent-git.txt"));
        assert!(!paths.contains(&"from-parent-dot.txt"));

        let matcher = build_gitignore_matcher_from_files(
            &root,
            &walk.gitignore_files,
            &walk.ignore_files,
            false,
        )
        .unwrap();
        assert!(!matcher.is_ignored(Path::new("from-parent-git.txt"), false));
        assert!(matcher.is_ignored(Path::new("from-parent-dot.txt"), false));

        let lifted = build_gitignore_matcher_from_files(
            &root,
            &walk.gitignore_files,
            &walk.ignore_files,
            true,
        )
        .unwrap();
        assert!(lifted.is_ignored(Path::new("from-parent-git.txt"), false));
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

        let metadata = walk_file_metadata(
            root,
            &MetaWalkOptions {
                no_require_git: true,
                ..Default::default()
            },
        );
        let matcher = build_gitignore_matcher_from_files(
            root,
            &metadata.gitignore_files,
            &metadata.ignore_files,
            true,
        )
        .expect("the lifted matcher should load .gitignore outside a repository");
        assert!(matcher.is_ignored(Path::new("build/out.rs"), false));
        assert!(matcher.is_ignored(Path::new("noisy.log"), false));
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

        // The default cap is well above this file, so an ordinary large source
        // file is indexed rather than silently dropped.
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
    fn walks_default_to_a_64_mib_size_cap() {
        // Both option structs hand-write `Default` precisely so this cannot
        // drift, and both must agree: a metadata walk that disagreed with the
        // indexing walk classified everything between the two caps as deleted
        // and evicted it (see `tests/serve_max_filesize.rs`).
        assert_eq!(DEFAULT_MAX_FILE_SIZE, Some(64 * 1024 * 1024));
        assert_eq!(WalkOptions::default().max_file_size, DEFAULT_MAX_FILE_SIZE);
        assert_eq!(
            MetaWalkOptions::default().max_file_size,
            DEFAULT_MAX_FILE_SIZE
        );

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // `set_len` rather than writing the bytes: the metadata walk only stats.
        std::fs::File::create(root.join("huge.txt"))
            .unwrap()
            .set_len(65 * 1024 * 1024)
            .unwrap();
        std::fs::File::create(root.join("large.txt"))
            .unwrap()
            .set_len(63 * 1024 * 1024)
            .unwrap();

        // Straddling the cap, so this pins the boundary rather than merely
        // observing that some file survived.
        let files = walk_file_metadata(root, &MetaWalkOptions::default()).files;
        assert_eq!(
            files.iter().map(|f| &f.relative_path).collect::<Vec<_>>(),
            vec!["large.txt"]
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

    // --- git's case-insensitive ignore matching ------------------------------
    //
    // On a case-insensitive filesystem git sets `core.ignorecase` and stops
    // distinguishing case when matching ignore rules. The `ignore` crate does
    // not, so a rule spelled `QLogs` left a `qlogs/` directory fully walked and
    // indexed even though `git status` never mentions it. Matching the way git
    // does starts catching *tracked* files too, which git would never hide, so
    // the tracked-file exemption has to come with it.

    /// A `.git` directory real enough for the walker: a config and an index.
    fn fake_git_repo(root: &Path, ignorecase: bool, tracked: &[&str]) {
        let git = root.join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(
            git.join("config"),
            format!("[core]\n\tignorecase = {ignorecase}\n"),
        )
        .unwrap();

        let mut index = Vec::new();
        index.extend_from_slice(b"DIRC");
        index.extend_from_slice(&2u32.to_be_bytes());
        index.extend_from_slice(&(tracked.len() as u32).to_be_bytes());
        for path in tracked {
            let start = index.len();
            index.extend_from_slice(&[0u8; 60]);
            index.extend_from_slice(&(path.len() as u16).to_be_bytes());
            index.extend_from_slice(path.as_bytes());
            index.push(0);
            while (index.len() - start) % 8 != 0 {
                index.push(0);
            }
        }
        fs::write(git.join("index"), index).unwrap();
    }

    /// The shape of the problem: a rule in one case, a directory in another,
    /// and an untracked artifact inside it.
    ///
    /// Extensions are all ones the walker treats as text. A `.JPG` would be a
    /// truer retelling of the enlistment this came from, but the binary
    /// extension filter drops it before any of this runs, so it would prove
    /// nothing.
    fn ignorecase_fixture(ignorecase: bool, tracked: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fake_git_repo(root, ignorecase, tracked);
        fs::write(root.join(".gitignore"), "QLogs\n*.txt\n").unwrap();
        fs::create_dir_all(root.join("qlogs")).unwrap();
        fs::write(root.join("qlogs/artifact.rs"), "junk").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        // Matched by `*.txt` only when case is ignored.
        fs::write(root.join("src/Kept.TXT"), "tracked").unwrap();
        fs::write(root.join("src/Gone.TXT"), "untracked").unwrap();
        dir
    }

    #[test]
    fn ignorecase_hides_what_git_hides_and_keeps_what_git_tracks() {
        // `src/Kept.TXT` is tracked, so `*.txt` must not hide it even though
        // the rule now matches. `src/Gone.TXT` is identical but untracked, and
        // the whole `qlogs/` directory goes — which is the point.
        let dir = ignorecase_fixture(true, &["src/main.rs", "src/Kept.TXT"]);
        let names = sorted_filenames(&walk_dir(dir.path(), &WalkOptions::default()), dir.path());
        assert!(
            names.contains(&"src/Kept.TXT".to_string()),
            "dropped a tracked file: {names:?}"
        );
        assert!(names.contains(&"src/main.rs".to_string()), "{names:?}");
        assert!(
            !names.contains(&"src/Gone.TXT".to_string()),
            "kept an untracked file the rule matches: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("qlogs/")),
            "kept an untracked file inside an ignored directory: {names:?}"
        );
    }

    #[test]
    fn without_ignorecase_the_walk_is_unchanged() {
        // The gate: a repository that distinguishes case gets git's ordinary
        // case-sensitive behaviour, and pays nothing for this.
        let dir = ignorecase_fixture(false, &["src/main.rs", "src/Kept.TXT"]);
        let names = sorted_filenames(&walk_dir(dir.path(), &WalkOptions::default()), dir.path());
        assert!(
            names.contains(&"qlogs/artifact.rs".to_string()),
            "{names:?}"
        );
        assert!(names.contains(&"src/Kept.TXT".to_string()), "{names:?}");
        assert!(names.contains(&"src/Gone.TXT".to_string()), "{names:?}");
    }

    #[test]
    fn a_tracked_file_keeps_its_ignored_directory_walkable() {
        // A directory cannot be pruned just because a rule matches it: git
        // still reports the tracked files inside.
        let dir = ignorecase_fixture(true, &["qlogs/kept.rs"]);
        fs::write(dir.path().join("qlogs/kept.rs"), "tracked").unwrap();
        let names = sorted_filenames(&walk_dir(dir.path(), &WalkOptions::default()), dir.path());
        assert!(
            names.contains(&"qlogs/kept.rs".to_string()),
            "pruned a directory holding a tracked file: {names:?}"
        );
        assert!(
            !names.contains(&"qlogs/artifact.rs".to_string()),
            "{names:?}"
        );
    }

    /// The watcher cannot walk per event, so it asks the same question as a
    /// point query. If the two disagree the watcher subscribes to and indexes a
    /// tree the walk excluded, and the next stale check evicts every file it
    /// added — on the enlistment this came from, a 13.4 GiB build artifact
    /// making up 71% of the corpus, re-read and re-evicted on every pass.
    #[test]
    fn the_point_query_matcher_hides_exactly_what_the_walk_hides() {
        let dir = ignorecase_fixture(true, &["src/main.rs", "src/Kept.TXT"]);
        let root = dir.path();
        let matcher = crate::gitignore::matcher_from_ignore_paths(
            root,
            std::slice::from_ref(&root.join(".gitignore")),
            &[],
        )
        .expect("the fixture has rules");

        assert!(
            matcher.is_ignored(Path::new("qlogs"), true),
            "a directory the walk prunes must not be subscribed to"
        );
        assert!(
            matcher.is_ignored(Path::new("qlogs/artifact.rs"), false),
            "nor may a file inside it be indexed"
        );
        assert!(
            matcher.is_ignored(Path::new("src/Gone.TXT"), false),
            "an untracked file the rule matches once case is ignored"
        );
        // The tracked-file exemption comes with it, or the watcher would drop
        // events for files git never hides.
        assert!(
            !matcher.is_ignored(Path::new("src/Kept.TXT"), false),
            "a tracked file must stay visible"
        );
        assert!(!matcher.is_ignored(Path::new("src/main.rs"), false));
    }

    #[test]
    fn the_point_query_matcher_follows_the_case_sensitivity_gate() {
        // The other direction, which is the one that loses files: a repository
        // that distinguishes case must not have anything hidden from it.
        let dir = ignorecase_fixture(false, &["src/main.rs", "src/Kept.TXT"]);
        let root = dir.path();
        let matcher = crate::gitignore::matcher_from_ignore_paths(
            root,
            std::slice::from_ref(&root.join(".gitignore")),
            &[],
        )
        .expect("the fixture has rules");

        assert!(!matcher.is_ignored(Path::new("qlogs"), true));
        assert!(!matcher.is_ignored(Path::new("qlogs/artifact.rs"), false));
        assert!(!matcher.is_ignored(Path::new("src/Gone.TXT"), false));
    }

    #[test]
    fn public_matcher_lazily_freezes_tracked_exemptions_on_first_match() {
        let dir = ignorecase_fixture(true, &["src/main.rs", "src/Kept.TXT"]);
        let root = dir.path();
        let matcher = crate::gitignore::matcher_from_ignore_paths(
            root,
            std::slice::from_ref(&root.join(".gitignore")),
            &[],
        )
        .expect("the fixture has rules");

        // Construction alone must not read the index. The first matching query
        // observes this update and freezes that membership for later queries.
        fake_git_repo(root, true, &["src/main.rs", "src/Kept.TXT", "src/Gone.TXT"]);
        assert!(
            !matcher.is_ignored(Path::new("src/Gone.TXT"), false),
            "the first matching query must initialize from the current index"
        );

        fake_git_repo(root, true, &["src/main.rs"]);
        assert!(
            !matcher.is_ignored(Path::new("src/Kept.TXT"), false),
            "the lazy snapshot must remain immutable after initialization"
        );
    }

    /// Reconciliation deliberately opts into the opposite behavior: the walk
    /// and matcher publication must not change answers halfway through a pass.
    #[test]
    fn frozen_tracked_exemption_is_immutable_and_detects_changes() {
        let dir = ignorecase_fixture(true, &["src/main.rs", "src/Kept.TXT"]);
        let root = dir.path();
        let ignorecase =
            crate::gitignore::CaseInsensitiveIgnore::frozen_snapshot(root, true, true, true)
                .map(std::sync::Arc::new);
        let matcher = crate::gitignore::matcher_from_ignore_paths_with_options_and_ignorecase(
            root,
            std::slice::from_ref(&root.join(".gitignore")),
            &[],
            false,
            ignorecase,
        )
        .expect("the fixture has rules");

        assert!(matcher.is_ignored(Path::new("src/Gone.TXT"), false));
        fake_git_repo(root, true, &["src/main.rs", "src/Kept.TXT", "src/Gone.TXT"]);

        assert!(
            matcher.is_ignored(Path::new("src/Gone.TXT"), false),
            "a frozen matcher must preserve the pass snapshot"
        );
        assert_ne!(
            matcher.tracked_membership_fingerprint(),
            matcher.current_tracked_membership_fingerprint(),
            "semantic polling must detect changes without mutating decisions"
        );
    }

    #[test]
    fn tracked_fingerprint_includes_whitelisted_files_under_ignored_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fake_git_repo(root, true, &["qlogs/kept.rs"]);
        fs::write(root.join(".gitignore"), "QLogs/\n!qlogs/kept.rs\n").unwrap();
        fs::create_dir_all(root.join("qlogs")).unwrap();
        fs::write(root.join("qlogs/kept.rs"), "tracked").unwrap();

        let ignorecase =
            crate::gitignore::CaseInsensitiveIgnore::frozen_snapshot(root, true, true, true)
                .map(std::sync::Arc::new);
        let matcher = crate::gitignore::matcher_from_ignore_paths_with_options_and_ignorecase(
            root,
            std::slice::from_ref(&root.join(".gitignore")),
            &[],
            false,
            ignorecase,
        )
        .expect("the fixture has rules");
        let baseline = matcher.tracked_membership_fingerprint();

        fake_git_repo(root, true, &[]);
        assert_ne!(
            baseline,
            matcher.current_tracked_membership_fingerprint(),
            "polling must notice when an ignored ancestor loses its tracked descendant"
        );
    }

    #[test]
    fn no_ignore_turns_the_whole_thing_off() {
        let dir = ignorecase_fixture(true, &["src/main.rs"]);
        let names = sorted_filenames(
            &walk_dir(
                dir.path(),
                &WalkOptions {
                    no_ignore: true,
                    ..Default::default()
                },
            ),
            dir.path(),
        );
        assert!(
            names.contains(&"qlogs/artifact.rs".to_string()),
            "{names:?}"
        );
    }

    #[test]
    fn no_ignore_vcs_turns_the_whole_thing_off_too() {
        // The rules only live in `.gitignore` here, and `--no-ignore-vcs` tells
        // the case-sensitive pass to skip that file. If the case-insensitive
        // pass kept reading it, it would become the *only* thing excluding and
        // the flag would exclude more than not passing it at all.
        let dir = ignorecase_fixture(true, &["src/main.rs"]);
        let names = sorted_filenames(
            &walk_dir(
                dir.path(),
                &WalkOptions {
                    no_ignore_vcs: true,
                    ..Default::default()
                },
            ),
            dir.path(),
        );
        assert!(
            names.contains(&"qlogs/artifact.rs".to_string()),
            "{names:?}"
        );
        assert!(names.contains(&"src/Gone.TXT".to_string()), "{names:?}");
    }

    #[test]
    fn no_ignore_exclude_leaves_the_repository_exclude_file_unread() {
        // Same argument as above, for the other source this matcher reads. The
        // rules live only in `.git/info/exclude`, so honouring the flag must
        // leave nothing behind to match with.
        let dir = ignorecase_fixture(true, &["src/main.rs"]);
        fs::remove_file(dir.path().join(".gitignore")).unwrap();
        fs::create_dir_all(dir.path().join(".git/info")).unwrap();
        fs::write(dir.path().join(".git/info/exclude"), "QLogs\n").unwrap();

        let with_exclude =
            sorted_filenames(&walk_dir(dir.path(), &WalkOptions::default()), dir.path());
        assert!(
            !with_exclude.contains(&"qlogs/artifact.rs".to_string()),
            "the exclude file should have hidden this: {with_exclude:?}"
        );

        let names = sorted_filenames(
            &walk_dir(
                dir.path(),
                &WalkOptions {
                    no_ignore_exclude: true,
                    ..Default::default()
                },
            ),
            dir.path(),
        );
        assert!(
            names.contains(&"qlogs/artifact.rs".to_string()),
            "{names:?}"
        );
    }

    #[test]
    fn no_ignore_parent_leaves_a_subdirectory_walk_alone() {
        // Walking `src/` reaches the repository-wide rules only as parent rules,
        // which is exactly what `--no-ignore-parent` switches off.
        let dir = ignorecase_fixture(true, &["src/main.rs"]);
        let src = dir.path().join("src");
        let names = sorted_filenames(
            &walk_dir(
                &src,
                &WalkOptions {
                    no_ignore_parent: true,
                    ..Default::default()
                },
            ),
            &src,
        );
        assert!(
            names.contains(&"Gone.TXT".to_string()),
            "applied a parent rule the flag disabled: {names:?}"
        );
    }

    #[test]
    fn an_unreadable_index_excludes_nothing() {
        // Without the index there is no way to tell tracked from untracked,
        // and guessing wrong would hide real source. Decline instead.
        let dir = ignorecase_fixture(true, &[]);
        fs::write(dir.path().join(".git/index"), b"not an index").unwrap();
        let names = sorted_filenames(&walk_dir(dir.path(), &WalkOptions::default()), dir.path());
        assert!(
            names.contains(&"qlogs/artifact.rs".to_string()),
            "{names:?}"
        );
        assert!(names.contains(&"src/Kept.TXT".to_string()), "{names:?}");
    }

    #[test]
    fn both_walks_agree_on_what_the_tree_contains() {
        // The indexing walk decides what goes into the index and the metadata
        // walk decides what is missing from it. If they disagree, every build
        // indexes a file the next stale check evicts.
        let dir = ignorecase_fixture(true, &["src/main.rs", "src/Kept.TXT"]);
        let indexed = sorted_filenames(&walk_dir(dir.path(), &WalkOptions::default()), dir.path());
        let mut seen: Vec<String> = walk_file_metadata(dir.path(), &MetaWalkOptions::default())
            .files
            .iter()
            .map(|f| f.relative_path.replace('\\', "/"))
            .filter(|p| !p.starts_with(".git/"))
            .collect();
        seen.sort();
        assert_eq!(indexed, seen);
    }
}
