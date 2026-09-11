//! Keep replaced indexes named until Windows can reclaim them without a POSIX unlink.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const RETIRED_DIR: &str = ".retired";
const GENERATION_PREFIX: &str = "generation-";
const COMMITTED_PREFIX: &str = "committed-";
static BACKUP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) struct BackupDir {
    path: PathBuf,
    committed: PathBuf,
}

impl BackupDir {
    pub(super) fn create(index_dir: &Path) -> io::Result<Self> {
        let parent = index_dir.join(RETIRED_DIR);
        fs::create_dir_all(&parent)?;
        loop {
            let sequence = BACKUP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let id = format!("{}-{sequence}", std::process::id());
            let path = parent.join(format!("{GENERATION_PREFIX}{id}"));
            let committed = parent.join(format!("{COMMITTED_PREFIX}{id}"));
            // A previous process may have had the same PID. Never replace one
            // of its generations, even if a reader still has it mapped.
            if committed.try_exists()? {
                continue;
            }
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path, committed }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn commit(&self) -> io::Result<()> {
        // Renaming a directory containing mapped files can fail on Windows. Use a
        // sibling marker instead, and keep it until the whole directory is
        // reclaimed so failed cleanup remains retryable across server restarts.
        fs::File::create_new(&self.committed).map(drop)
    }
}

/// Called under the index directory's server/publication lock, including on
/// startup and idle auto-save ticks so cleanup does not require another rebuild.
pub(super) fn cleanup_retired(index_dir: &Path) {
    let parent = index_dir.join(RETIRED_DIR);
    let entries = match fs::read_dir(&parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            eprintln!(
                "[trace] warning: could not list retired indexes in {}: {error}",
                parent.display()
            );
            return;
        }
    };
    for entry in entries {
        let result = (|| -> io::Result<()> {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|name| name.strip_prefix(COMMITTED_PREFIX))
            else {
                return Ok(());
            };
            let Some((pid, sequence)) = id.split_once('-') else {
                return Ok(());
            };
            if pid.parse::<u32>().is_err()
                || sequence.parse::<u64>().is_err()
                || !entry.file_type()?.is_file()
            {
                return Ok(());
            }
            let path = parent.join(format!("{GENERATION_PREFIX}{id}"));
            match remove_generation(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    eprintln!(
                        "[trace] retired index cleanup deferred for {}: {error}",
                        path.display()
                    );
                    return Ok(());
                }
            }
            remove_file(&entry.path())
        })();
        if let Err(error) = result {
            eprintln!(
                "[trace] warning: could not clean retired indexes in {}: {error}",
                parent.display()
            );
        }
    }
}

fn remove_generation(path: &Path) -> io::Result<()> {
    let kind = fs::symlink_metadata(path)?.file_type();
    if !kind.is_dir() || kind.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not an index backup directory: {}", path.display()),
        ));
    }
    // Backups contain only these files. Do not follow directory links or
    // recursively remove unexpected contents in a recovery folder.
    for name in super::staged_publish_order() {
        match remove_file(&path.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    fs::remove_dir(path)
}

/// Best-effort cleanup of unpublished build output, without force-unlinking
/// mapped files left by an earlier publication or another process.
pub(super) fn cleanup_staging(path: &Path) {
    match remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => eprintln!(
            "[trace] warning: index staging cleanup deferred for {}: {error}",
            path.display()
        ),
    }
}

#[cfg(not(windows))]
pub(super) fn remove_file(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

#[cfg(windows)]
pub(super) fn remove_file(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_INFO, FILE_FLAG_OPEN_REPARSE_POINT, FileDispositionInfo,
        SetFileInformationByHandle,
    };

    let file = fs::OpenOptions::new()
        .access_mode(DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the handle stays open for the call and the buffer has the exact
    // layout/size required by FileDispositionInfo. Unlike std::fs deletion,
    // this never falls back to POSIX semantics when a mapped file rejects it.
    let result = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            std::ptr::from_ref(&disposition).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn remove_dir_all(path: &Path) -> io::Result<()> {
    fs::remove_dir_all(path)
}

#[cfg(windows)]
fn remove_dir_all(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::FileTypeExt;

    let kind = fs::symlink_metadata(path)?.file_type();
    if kind.is_symlink_dir() {
        return fs::remove_dir(path);
    }
    if !kind.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("not an index staging directory: {}", path.display()),
        ));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() || kind.is_symlink_dir() {
            remove_dir_all(&entry.path())?;
        } else {
            remove_file(&entry.path())?;
        }
    }
    fs::remove_dir(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_retired_preserves_pending_publications() {
        let tmp = tempfile::tempdir().unwrap();
        let pending = BackupDir::create(tmp.path()).unwrap();
        let committed = BackupDir::create(tmp.path()).unwrap();
        assert_ne!(pending.path(), committed.path());
        fs::write(pending.path().join("index.bin"), b"recover me").unwrap();
        fs::write(committed.path().join("index.bin"), b"obsolete").unwrap();
        committed.commit().unwrap();
        let committed_path = committed.path().to_path_buf();
        let committed_marker = committed.committed.clone();
        drop(committed);

        cleanup_retired(tmp.path());

        assert!(!committed_path.exists());
        assert!(!committed_marker.exists());
        assert_eq!(
            fs::read(pending.path().join("index.bin")).unwrap(),
            b"recover me"
        );
    }

    #[test]
    fn cleanup_retired_preserves_unexpected_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let backup = BackupDir::create(tmp.path()).unwrap();
        fs::write(backup.path().join("unexpected.txt"), b"keep me").unwrap();
        backup.commit().unwrap();

        cleanup_retired(tmp.path());

        assert_eq!(
            fs::read(backup.path().join("unexpected.txt")).unwrap(),
            b"keep me"
        );
        assert!(backup.committed.is_file());
    }

    #[test]
    fn cleanup_retired_finishes_after_directory_was_already_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let backup = BackupDir::create(tmp.path()).unwrap();
        backup.commit().unwrap();
        fs::remove_dir(backup.path()).unwrap();

        cleanup_retired(tmp.path());

        assert!(!backup.committed.exists());
    }

    #[test]
    fn remove_file_reports_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            remove_file(&tmp.path().join("missing.bin"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[cfg(windows)]
    fn map_file(path: &Path) -> memmap2::Mmap {
        let file = fs::File::open(path).unwrap();
        // SAFETY: these fixtures are only renamed or passed to deletion that
        // refuses mapped files; no code modifies their contents.
        unsafe { memmap2::Mmap::map(&file).unwrap() }
    }

    #[cfg(windows)]
    #[test]
    fn remove_file_refuses_mapped_files_until_unmapped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.bin");
        fs::write(&path, b"mapped postings").unwrap();
        let mapping = map_file(&path);

        assert!(remove_file(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), &mapping[..]);

        drop(mapping);
        remove_file(&path).unwrap();
        assert!(!path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn staging_cleanup_refuses_mapped_files_until_unmapped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("staging").join("nested");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("index.bin"), b"mapped postings").unwrap();
        let mapping = map_file(&path.join("index.bin"));

        assert!(remove_dir_all(&tmp.path().join("staging")).is_err());
        assert_eq!(fs::read(path.join("index.bin")).unwrap(), &mapping[..]);

        drop(mapping);
        remove_dir_all(&tmp.path().join("staging")).unwrap();
        assert!(!path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn cleanup_retired_retries_generations_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let mut readers = Vec::new();
        for generation in 0..3 {
            let backup = BackupDir::create(tmp.path()).unwrap();
            let bytes = format!("postings generation {generation}");
            fs::write(backup.path().join("index.bin"), bytes.as_bytes()).unwrap();
            let mapping = map_file(&backup.path().join("index.bin"));
            backup.commit().unwrap();
            let path = backup.path().join("index.bin");
            readers.push((mapping, path, bytes));
            cleanup_retired(tmp.path());
            for (mapping, path, bytes) in &readers {
                assert_eq!(fs::read(path).unwrap(), bytes.as_bytes());
                assert_eq!(&mapping[..], bytes.as_bytes());
            }
        }

        while let Some((mapping, path, _)) = readers.pop() {
            drop(mapping);
            cleanup_retired(tmp.path());
            assert!(!path.exists());
            for (_, path, bytes) in &readers {
                assert_eq!(fs::read(path).unwrap(), bytes.as_bytes());
            }
        }
        assert_eq!(
            fs::read_dir(tmp.path().join(RETIRED_DIR)).unwrap().count(),
            0
        );
    }
}
