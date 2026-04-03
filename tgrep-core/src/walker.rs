/// .gitignore-aware file walker using the `ignore` crate (same as ripgrep).
use ignore::WalkBuilder;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::trigram;

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

/// Read only the first 8KB of a file to check for binary content.
fn is_binary_file(path: &Path) -> std::io::Result<bool> {
    let mut f = File::open(path)?;
    let mut buf = [0u8; 8192];
    let n = f.read(&mut buf)?;
    Ok(trigram::is_binary(&buf[..n]))
}

/// Walk a directory tree, respecting .gitignore rules (unless disabled).
/// Returns paths of text files suitable for indexing/searching.
pub fn walk_dir(root: &Path, opts: &WalkOptions) -> WalkResult {
    let mut files = Vec::new();
    let mut skipped_binary = 0;
    let mut skipped_error = 0;

    let walker = WalkBuilder::new(root)
        .hidden(!opts.include_hidden)
        .git_ignore(!opts.no_ignore)
        .git_global(!opts.no_ignore)
        .git_exclude(!opts.no_ignore)
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                skipped_error += 1;
                continue;
            }
        };

        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let path = entry.into_path();

        if !opts.search_binary {
            // Fast path: reject known binary extensions without I/O
            if is_binary_extension(&path) {
                skipped_binary += 1;
                continue;
            }

            // Skip files larger than MAX_FILE_SIZE
            match std::fs::metadata(&path) {
                Ok(meta) if meta.len() > MAX_FILE_SIZE => {
                    skipped_binary += 1;
                    continue;
                }
                Err(_) => {
                    skipped_error += 1;
                    continue;
                }
                _ => {}
            }

            // Content-based binary check: read only first 8KB
            match is_binary_file(&path) {
                Ok(true) => {
                    skipped_binary += 1;
                    continue;
                }
                Err(_) => {
                    skipped_error += 1;
                    continue;
                }
                _ => {}
            }
        }

        files.push(path);
    }

    WalkResult {
        files,
        skipped_binary,
        skipped_error,
    }
}
