//! Keep replaced indexes named until Windows can reclaim them without a POSIX unlink.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const RETIRED_DIR: &str = ".retired";
const GENERATION_PREFIX: &str = "generation-";
const COMMITTED_PREFIX: &str = "committed-";
static BACKUP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const STAGING_DIRS: &[&str] = &[
    ".reload-build",
    ".filename-index-staging",
    ".stale-delta",
    ".stale-merge",
    ".flush-staging",
];

fn is_spill_name(name: &str) -> bool {
    let Some(id) = name
        .strip_prefix("spill-")
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    let (pid, sequence) = id.split_once('-').unwrap_or((id, "0"));
    !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && pid.parse::<u32>().is_ok()
        && !sequence.is_empty()
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
        && sequence.parse::<u64>().is_ok()
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Startup only, with serve.lock held and before any indexing worker starts.
/// Published files and the independently recovered .retired directory are not
/// scratch output, even when their publication was interrupted.
pub(super) fn cleanup_stale_builds(index_dir: &Path) {
    let entries = match fs::read_dir(index_dir) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!(
                "[trace] warning: could not list stale build output in {}: {error}",
                index_dir.display()
            );
            return;
        }
    };
    let mut removed = 0usize;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!(
                    "[trace] warning: could not inspect stale build output in {}: {error}",
                    index_dir.display()
                );
                continue;
            }
        };
        let path = entry.path();
        let result = (|| -> io::Result<()> {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Ok(());
            };
            let spill = is_spill_name(name);
            if !spill && !STAGING_DIRS.contains(&name) {
                return Ok(());
            }
            let metadata = fs::symlink_metadata(&path)?;
            if is_link(&metadata) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("refusing linked build artifact: {}", path.display()),
                ));
            }
            if metadata.is_dir() {
                remove_dir_all(&path)?;
            } else if spill && metadata.is_file() {
                remove_file(&path)?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unexpected build artifact type: {}", path.display()),
                ));
            }
            removed += 1;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!(
                "[trace] warning: stale build cleanup deferred for {}: {error}",
                path.display()
            );
        }
    }
    if removed != 0 {
        eprintln!("[trace] removed {removed} stale build artifact(s)");
    }
}

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
    let entries = match (|| -> io::Result<_> {
        let metadata = fs::symlink_metadata(&parent)?;
        if !metadata.is_dir() || is_link(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not an index retirement directory: {}", parent.display()),
            ));
        }
        fs::read_dir(&parent)
    })() {
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
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || is_link(&metadata) {
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
    let metadata = fs::symlink_metadata(path)?;
    if is_link(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing linked staging directory: {}", path.display()),
        ));
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("not an index staging directory: {}", path.display()),
        ));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if is_link(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing linked staging entry: {}", entry.path().display()),
            ));
        }
        if metadata.is_dir() {
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
    fn stale_build_cleanup_only_removes_recognized_temporary_output() {
        let tmp = tempfile::tempdir().unwrap();
        for name in STAGING_DIRS.iter().copied().chain(["spill-123-4.tmp"]) {
            let path = tmp.path().join(name).join("nested");
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("segment.bin"), b"abandoned").unwrap();
        }
        fs::write(tmp.path().join("spill-123.tmp"), b"legacy spill").unwrap();
        let preserved = [
            "index.bin",
            "lookup.bin",
            "files.bin",
            "meta.json",
            "serve.lock",
            "serve.json",
            "spill-project.tmp",
            "spill-1--2.tmp",
            "spill-4294967296-0.tmp",
            "spill-1-18446744073709551616.tmp",
            "my-staging",
        ];
        for name in preserved {
            fs::write(tmp.path().join(name), b"keep").unwrap();
        }
        let pending = BackupDir::create(tmp.path()).unwrap();
        fs::write(pending.path().join("index.bin"), b"recover").unwrap();

        cleanup_stale_builds(tmp.path());

        for name in STAGING_DIRS
            .iter()
            .copied()
            .chain(["spill-123-4.tmp", "spill-123.tmp"])
        {
            assert!(!tmp.path().join(name).exists(), "{name}");
        }
        for name in preserved {
            assert_eq!(fs::read(tmp.path().join(name)).unwrap(), b"keep", "{name}");
        }
        assert_eq!(
            fs::read(pending.path().join("index.bin")).unwrap(),
            b"recover"
        );
    }

    #[test]
    fn stale_build_cleanup_waits_for_the_index_writer_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let (index_dir, lock) =
            crate::serve::prepare_index_directory(&root, &root.join(".tgrep")).unwrap();
        let stage = index_dir.join(".flush-staging");
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("index.bin"), b"active build").unwrap();

        let error = crate::serve::prepare_index_directory(&root, &index_dir).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("another tgrep server or index build")
        );
        assert_eq!(fs::read(stage.join("index.bin")).unwrap(), b"active build");

        drop(lock);
        let (_index_dir, _lock) = crate::serve::prepare_index_directory(&root, &index_dir).unwrap();
        assert!(!stage.exists());
    }

    #[test]
    fn stale_build_cleanup_rejects_source_containment_and_managed_storage() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = fs::canonicalize(tmp.path()).unwrap();
        let root = parent.join("repo");
        fs::create_dir(&root).unwrap();
        let managed = parent
            .join(tgrep_core::managed::STORE_DIRECTORY)
            .join("0".repeat(64));
        fs::create_dir_all(&managed).unwrap();
        for index_dir in [&parent, &managed] {
            let stage = index_dir.join(".reload-build");
            fs::create_dir(&stage).unwrap();
            fs::write(stage.join("index.bin"), b"keep").unwrap();

            assert!(crate::serve::prepare_index_directory(&root, index_dir).is_err());
            assert_eq!(fs::read(stage.join("index.bin")).unwrap(), b"keep");
            assert!(!index_dir.join("serve.lock").exists());
        }
    }

    #[cfg(any(unix, windows))]
    fn link_directory(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/j"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn stale_build_cleanup_does_not_follow_directory_links() {
        let tmp = tempfile::tempdir().unwrap();
        let index_dir = tmp.path().join("index");
        let outside = tmp.path().join("outside");
        fs::create_dir(&index_dir).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("index.bin"), b"external sentinel").unwrap();
        for name in [".reload-build", "spill-123-0.tmp", ".retired"] {
            link_directory(&outside, &index_dir.join(name));
        }
        fs::create_dir(index_dir.join(".flush-staging")).unwrap();
        link_directory(&outside, &index_dir.join(".flush-staging").join("nested"));

        cleanup_stale_builds(&index_dir);
        cleanup_retired(&index_dir);

        assert_eq!(
            fs::read(outside.join("index.bin")).unwrap(),
            b"external sentinel"
        );
        for name in [".reload-build", "spill-123-0.tmp", ".retired"] {
            assert!(fs::symlink_metadata(index_dir.join(name)).is_ok());
        }

        let other_index = tmp.path().join("other-index");
        let retired = other_index.join(".retired");
        fs::create_dir_all(&retired).unwrap();
        link_directory(&outside, &retired.join("generation-123-0"));
        let committed = retired.join("committed-123-0");
        fs::write(&committed, b"").unwrap();
        cleanup_retired(&other_index);
        assert!(committed.exists());
        assert_eq!(
            fs::read(outside.join("index.bin")).unwrap(),
            b"external sentinel"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn stale_build_cleanup_rejects_aliases_into_managed_storage() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = fs::canonicalize(tmp.path()).unwrap();
        let root = parent.join("repo");
        let managed = parent
            .join(tgrep_core::managed::STORE_DIRECTORY)
            .join("0".repeat(64));
        fs::create_dir(&root).unwrap();
        fs::create_dir_all(managed.join(".stale-merge")).unwrap();
        let sentinel = managed.join(".stale-merge").join("index.bin");
        fs::write(&sentinel, b"protected").unwrap();
        let alias = parent.join("alias");
        link_directory(&managed, &alias);

        let error = crate::serve::prepare_index_directory(&root, &alias).unwrap_err();
        assert!(error.to_string().contains("protected managed reader"));
        assert_eq!(fs::read(sentinel).unwrap(), b"protected");
        assert!(!managed.join("serve.lock").exists());
    }

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
    fn stale_build_cleanup_retries_mapped_output_after_reader_release() {
        let tmp = tempfile::tempdir().unwrap();
        for name in [".reload-build", "spill-123-0.tmp"] {
            let path = tmp.path().join(name);
            fs::create_dir(&path).unwrap();
            let file = path.join("index.bin");
            fs::write(&file, b"mapped build output").unwrap();
            let mapping = map_file(&file);

            cleanup_stale_builds(tmp.path());
            assert_eq!(fs::read(&file).unwrap(), &mapping[..]);

            drop(mapping);
            cleanup_stale_builds(tmp.path());
            assert!(!path.exists());
        }
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
