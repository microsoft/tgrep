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

/// Number of parallel walker threads (capped at 12 to avoid diminishing returns).
fn walker_thread_count() -> usize {
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
    pub skipped_binary: usize,
    pub skipped_error: usize,
}

#[derive(Default)]
pub struct WalkOptions {
    pub include_hidden: bool,
    pub no_ignore: bool,
    pub search_binary: bool,
    /// Collect `.gitignore` file paths encountered during the walk.
    pub collect_gitignore_files: bool,
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
    let gitignore_files = std::sync::Mutex::new(Vec::new());
    let skipped_binary = std::sync::atomic::AtomicUsize::new(0);
    let skipped_error = std::sync::atomic::AtomicUsize::new(0);
    let exclude_dirs: std::sync::Arc<Vec<String>> = std::sync::Arc::new(opts.exclude_dirs.clone());
    let search_binary = opts.search_binary;
    let include_hidden = opts.include_hidden;
    let collect_gitignore_files = opts.collect_gitignore_files;
    let root = root.to_path_buf();

    let mut builder = WalkBuilder::new(&root);
    builder
        // When collecting .gitignore paths, keep hidden entries visible to the
        // walker and apply hidden filtering below so .gitignore files are seen
        // while hidden directories/files still match normal index behavior.
        .hidden(!include_hidden && !collect_gitignore_files)
        .git_ignore(!opts.no_ignore)
        .git_global(!opts.no_ignore)
        .git_exclude(!opts.no_ignore)
        .threads(walker_thread_count());
    let walker = builder.build_parallel();

    walker.run(|| {
        let exclude = exclude_dirs.clone();
        let root = root.clone();
        let files = &files;
        let gitignore_files = &gitignore_files;
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

            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                if collect_gitignore_files
                    && entry.path() != root
                    && !include_hidden
                    && entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with('.'))
                {
                    return ignore::WalkState::Skip;
                }
                if should_skip_dir(&entry, &exclude) {
                    return ignore::WalkState::Skip;
                }
                return ignore::WalkState::Continue;
            }

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return ignore::WalkState::Continue;
            }

            let path = entry.path();

            if collect_gitignore_files && entry.file_name() == ".gitignore" {
                gitignore_files.lock().unwrap().push(path.to_path_buf());
                if !include_hidden {
                    return ignore::WalkState::Continue;
                }
            }

            if collect_gitignore_files
                && !include_hidden
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with('.'))
            {
                return ignore::WalkState::Continue;
            }

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
        gitignore_files: gitignore_files.into_inner().unwrap(),
        skipped_binary: skipped_binary.into_inner(),
        skipped_error: skipped_error.into_inner(),
    }
}

/// Build a point-query gitignore matcher from `.gitignore` files discovered by
/// an existing walk, avoiding a second full-tree discovery pass.
pub fn build_gitignore_matcher_from_files(
    root: &Path,
    gitignore_files: &[PathBuf],
) -> Option<crate::gitignore::Gitignore> {
    use ignore::gitignore::GitignoreBuilder;

    let mut builder = GitignoreBuilder::new(root);

    let info_exclude = root.join(".git").join("info").join("exclude");
    if info_exclude.is_file() {
        let _ = builder.add(&info_exclude);
    }

    for path in gitignore_files {
        if path.is_file() {
            let _ = builder.add(path);
        }
    }

    let gi = builder.build().ok()?;
    if gi.is_empty() { None } else { Some(gi) }
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
        .threads(walker_thread_count())
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
                if should_skip_dir(&entry, &exclude) {
                    return ignore::WalkState::Skip;
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
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join("src").join(".gitignore"), "*.tmp\n").unwrap();

        let walk = walk_dir(
            &root,
            &WalkOptions {
                collect_gitignore_files: true,
                ..Default::default()
            },
        );
        let gi = build_gitignore_matcher_from_files(&root, &walk.gitignore_files)
            .expect("matcher should build from discovered .gitignore files");

        assert!(
            gi.matched_path_or_any_parents("build/output.log", false)
                .is_ignore()
        );
        assert!(
            gi.matched_path_or_any_parents("src/cache.tmp", false)
                .is_ignore()
        );
        assert!(
            !gi.matched_path_or_any_parents("src/main.rs", false)
                .is_ignore()
        );
    }

    #[test]
    fn walk_file_metadata_excludes_dirs() {
        let dir = setup_fixture();
        let root = dir.path().join("testdata");

        let all = walk_file_metadata(&root, &[]);
        let excluded = walk_file_metadata(&root, &["vendor".to_string()]);

        assert!(all.iter().any(|f| f.relative_path.starts_with("vendor/")));
        assert!(
            !excluded
                .iter()
                .any(|f| f.relative_path.starts_with("vendor/"))
        );
        assert!(excluded.iter().any(|f| f.relative_path == "src/main.rs"));
    }
}
