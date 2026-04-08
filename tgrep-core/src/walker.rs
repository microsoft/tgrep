/// .gitignore-aware file walker using the `ignore` crate (same as ripgrep).
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Maximum file size to index (1 MB). Larger files are skipped.
const MAX_FILE_SIZE: u64 = 1_048_576;

/// Binary extensions that can be rejected without reading file content.
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "svg", "tiff", "tif", "psd", "raw", "mp3",
    "mp4", "avi", "mkv", "mov", "wav", "flac", "ogg", "wma", "aac", "m4a", "webm", "zip", "tar",
    "gz", "bz2", "xz", "zst", "7z", "rar", "lz4", "lzma", "cab", "exe", "dll", "so", "dylib",
    "obj", "o", "a", "lib", "pdb", "wasm", "class", "jar", "pyc", "pyo", "beam", "pdf", "doc",
    "docx", "xls", "xlsx", "ppt", "pptx", "ttf", "otf", "woff", "woff2", "eot", "bin", "dat", "db",
    "sqlite", "sqlite3",
];

pub struct WalkResult {
    pub files: Vec<PathBuf>,
    pub skipped_binary: usize,
    pub skipped_error: usize,
}

#[derive(Default)]
pub struct WalkOptions {
    pub include_hidden: bool,
    pub no_ignore: bool,
    pub search_binary: bool,
    /// Directory names to exclude from walking (e.g., "vendor", "third_party").
    pub exclude_dirs: Vec<String>,
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

/// Walk a directory tree, respecting .gitignore rules (unless disabled).
/// Returns paths of text files suitable for indexing/searching.
///
/// Only rejects files by extension and size here. Content-based binary
/// detection is deferred to the caller (which reads the file anyway),
/// avoiding an extra 8KB read per file during the walk.
pub fn walk_dir(root: &Path, opts: &WalkOptions) -> WalkResult {
    let files = std::sync::Mutex::new(Vec::new());
    let skipped_binary = std::sync::atomic::AtomicUsize::new(0);
    let skipped_error = std::sync::atomic::AtomicUsize::new(0);
    let exclude_dirs: std::sync::Arc<Vec<String>> = std::sync::Arc::new(opts.exclude_dirs.clone());
    let search_binary = opts.search_binary;

    let walker = WalkBuilder::new(root)
        .hidden(!opts.include_hidden)
        .git_ignore(!opts.no_ignore)
        .git_global(!opts.no_ignore)
        .git_exclude(!opts.no_ignore)
        .threads(std::thread::available_parallelism().map_or(4, |n| n.get().min(12)))
        .build_parallel();

    walker.run(|| {
        let exclude = exclude_dirs.clone();
        let files = &files;
        let skipped_binary = &skipped_binary;
        let skipped_error = &skipped_error;
        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    skipped_error.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                }
            };

            // Skip excluded directories and their subtrees
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                if !exclude.is_empty() {
                    if let Some(name) = entry.file_name().to_str() {
                        if exclude.iter().any(|d| d == name) {
                            return ignore::WalkState::Skip;
                        }
                    }
                }
                return ignore::WalkState::Continue;
            }

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return ignore::WalkState::Continue;
            }

            let path = entry.path();

            if !search_binary {
                if is_binary_extension(path) {
                    skipped_binary.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                }

                if let Ok(meta) = entry.metadata()
                    && meta.len() > MAX_FILE_SIZE
                {
                    skipped_binary.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                }
            }

            files.lock().unwrap().push(entry.into_path());
            ignore::WalkState::Continue
        })
    });

    WalkResult {
        files: files.into_inner().unwrap(),
        skipped_binary: skipped_binary.into_inner(),
        skipped_error: skipped_error.into_inner(),
    }
}

/// Filesystem metadata for a single file (no content read).
pub struct FileMeta {
    pub relative_path: String,
    pub mtime: u64,
    pub size: u64,
}

/// Walk a directory tree collecting only filesystem metadata (mtime, size).
/// No file content is read — this is used for stale file detection on startup.
pub fn walk_file_metadata(root: &Path, exclude_dirs: &[String]) -> Vec<FileMeta> {
    let results = std::sync::Mutex::new(Vec::new());
    let exclude: std::sync::Arc<Vec<String>> = std::sync::Arc::new(exclude_dirs.to_vec());

    let walker = WalkBuilder::new(root)
        .hidden(true) // skip hidden by default
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .threads(std::thread::available_parallelism().map_or(4, |n| n.get().min(12)))
        .build_parallel();

    walker.run(|| {
        let exclude = exclude.clone();
        let results = &results;
        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return ignore::WalkState::Continue,
            };

            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                if !exclude.is_empty() {
                    if let Some(name) = entry.file_name().to_str() {
                        if exclude.iter().any(|d| d == name) {
                            return ignore::WalkState::Skip;
                        }
                    }
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

            let rel_path = match path.strip_prefix(root) {
                Ok(p) => p.to_string_lossy().replace('\\', "/"),
                Err(_) => return ignore::WalkState::Continue,
            };

            if let Ok(meta) = entry.metadata() {
                if meta.len() > MAX_FILE_SIZE {
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

    results.into_inner().unwrap()
}
