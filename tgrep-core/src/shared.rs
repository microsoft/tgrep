//! Shared, read-only content-index bases with independent worktree overlays.
//!
//! This is a core building block, not Git change discovery or a multi-worktree
//! server. The caller must keep the base files immutable while mapped and
//! populate each overlay with *all* differences from that base before querying.
//! A worktree also needs its own traversal/visibility state and content cache;
//! neither is represented by these content-index checkpoints.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::hybrid::HybridIndex;
use crate::live::{LiveIndex, OVERLAY_BIT};
use crate::meta::IndexMeta;
use crate::reader::IndexReader;
use crate::trigram::{TrigramMaskMap, TrigramMasks};
use crate::{Error, Result};

const OVERLAY_VERSION: u32 = 1;

/// A validated immutable base shared by any number of worktree views.
///
/// Opening hashes the searchable index once; cloning and creating worktrees
/// share the reader, including its in-memory path table, without copying it.
/// This identity describes index bytes, not a Git revision or freshness proof.
/// The caller owns the association with a Git tree and indexing profile.
#[derive(Clone)]
pub struct SharedBase {
    reader: Arc<IndexReader>,
    id: [u8; 32],
    directory_id: Arc<same_file::Handle>,
}

impl SharedBase {
    /// The shared reader, including its single in-memory path table.
    pub fn reader(&self) -> &Arc<IndexReader> {
        &self.reader
    }

    /// Fingerprint binding checkpoints to these exact searchable index bytes.
    pub fn snapshot_id(&self) -> [u8; 32] {
        self.id
    }

    /// Open a complete, current-format base in an immutable snapshot directory.
    ///
    /// Do not use a directory being rewritten by `tgrep index` or `tgrep serve`.
    /// Legacy and incomplete indexes remain usable through the existing APIs,
    /// but must be rebuilt before they can be used as shared bases.
    pub fn open(index_dir: &Path) -> Result<Self> {
        let index_dir = std::fs::canonicalize(index_dir)?;
        let directory_id = Arc::new(same_file::Handle::from_path(&index_dir)?);
        let reader = Arc::new(IndexReader::open_for_snapshot(&index_dir)?);
        let meta = IndexMeta::load(&index_dir)?;
        if meta.version != crate::meta::INDEX_FORMAT_VERSION
            || !meta.complete
            || !meta.hidden_complete
            || meta.file_table_id != Some(reader.file_table_id())
        {
            return Err(invalid(
                "shared base requires a complete current-format index with matching coverage metadata",
            ));
        }
        if meta.num_files != reader.num_files() as u64
            || meta.num_trigrams != reader.num_trigrams() as u64
        {
            return Err(invalid(
                "shared base metadata counts do not match its index sections",
            ));
        }
        if reader.num_files() >= OVERLAY_BIT as usize {
            return Err(invalid(
                "shared base file IDs overlap the live-index ID range",
            ));
        }
        let mut paths = HashSet::with_capacity(reader.num_files());
        for path in reader.all_paths() {
            validate_path(path)?;
            if !paths.insert(path) {
                return Err(invalid("shared base contains duplicate paths"));
            }
        }
        let id = reader.snapshot_id();
        Ok(Self {
            reader,
            id,
            directory_id,
        })
    }

    /// Create an independent overlay rooted at an existing worktree directory.
    ///
    /// This only shares the base; it does not discover differences. Before
    /// searching, upsert changed/new files and tombstone missing/ineligible
    /// base paths, including committed branch differences and untracked files.
    /// Upserts take complete decoded file contents, not diff hunks.
    pub fn create_worktree(&self, root: &Path) -> Result<HybridIndex> {
        Ok(HybridIndex::from_reader(
            Arc::clone(&self.reader),
            &canonical_root(root)?,
        ))
    }

    /// Atomically replace a worktree's content-overlay checkpoint.
    ///
    /// Only live postings, masks and tombstones are saved; the base is neither
    /// copied nor modified. The checkpoint is bound to the exact base bytes and
    /// canonical worktree root. Its parent directory must already exist and
    /// must be outside the base snapshot directory. Trailing separators and
    /// current-directory suffixes are rejected. Publication stays bound to
    /// the opened directory, even if its original pathname is replaced.
    /// Saving does not prune the live overlay or reset its dirty counter.
    /// Non-Unicode roots are encoded losslessly using platform-native units;
    /// Unicode roots retain the existing JSON string representation.
    ///
    /// Atomicity refers to replacement visibility, not power-loss durability.
    /// File contents are synced before replacement, but the parent directory
    /// is not synced afterwards. Even a successful save may be lost after a
    /// system crash; callers must reconcile or rebuild stale/missing checkpoints.
    pub fn save_overlay(&self, worktree: &HybridIndex, path: &Path) -> Result<()> {
        if !Arc::ptr_eq(&self.reader, &worktree.reader_arc()) {
            return Err(invalid("worktree no longer uses this shared base"));
        }
        let destination = self.checkpoint_destination(path)?;
        let checkpoint = OverlayCheckpoint {
            version: OVERLAY_VERSION,
            base_id: self.id,
            root: CheckpointRoot::from_path(&canonical_root(&worktree.root)?)?,
            overlay: OverlayData::capture(&worktree.live)?,
        };
        checkpoint.persist(&destination)
    }

    fn checkpoint_destination(&self, path: &Path) -> Result<CheckpointDestination> {
        let bytes = path.as_os_str().as_encoded_bytes();
        let suffix = bytes.strip_suffix(b".").unwrap_or(bytes);
        let names_directory = suffix
            .last()
            .is_some_and(|byte| std::path::is_separator(char::from(*byte)));
        let file_name = path
            .file_name()
            .filter(|_| !names_directory)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "overlay checkpoint path must name a file",
                )
            })?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        CheckpointDestination::open(parent, file_name.to_os_string(), &self.directory_id)
    }

    /// Restore a checkpoint without changing the base or another worktree.
    ///
    /// Missing, malformed, incompatible and wrong-worktree checkpoints are
    /// errors, never an empty-overlay fallback. Restored content is only as
    /// fresh as the checkpoint: reconcile changes since it was saved before
    /// making the view available to searches.
    pub fn restore_worktree(&self, root: &Path, path: &Path) -> Result<HybridIndex> {
        let checkpoint: OverlayCheckpoint =
            serde_json::from_reader(BufReader::new(std::fs::File::open(path)?))?;
        if checkpoint.version != OVERLAY_VERSION {
            return Err(invalid("unsupported worktree overlay version"));
        }
        if checkpoint.base_id != self.id {
            return Err(invalid(
                "worktree overlay belongs to a different base snapshot",
            ));
        }
        let root = canonical_root(root)?;
        if checkpoint.root != CheckpointRoot::from_path(&root)? {
            return Err(invalid("worktree overlay belongs to a different root"));
        }
        let live = checkpoint.overlay.into_live()?;
        let mut worktree = HybridIndex::from_reader(Arc::clone(&self.reader), &root);
        worktree.live = live;
        Ok(worktree)
    }
}

struct CheckpointDestination {
    file_name: OsString,
    #[cfg(unix)]
    directory: cap_std::fs::Dir,
    #[cfg(windows)]
    directory: PathBuf,
    #[cfg(windows)]
    _guards: Vec<same_file::Handle>,
}

impl CheckpointDestination {
    #[cfg(unix)]
    fn open(parent: &Path, file_name: OsString, base: &same_file::Handle) -> Result<Self> {
        use cap_std::{ambient_authority, fs::Dir};

        let directory = Dir::open_ambient_dir(parent, ambient_authority())?;
        let mut ancestor = directory.try_clone()?;
        let mut identity = same_file::Handle::from_file(ancestor.try_clone()?.into_std_file())?;
        loop {
            if &identity == base {
                return Err(invalid(
                    "overlay checkpoint must be outside the shared base directory",
                ));
            }
            let parent = ancestor.open_parent_dir(ambient_authority())?;
            let parent_id = same_file::Handle::from_file(parent.try_clone()?.into_std_file())?;
            if identity == parent_id {
                break;
            }
            ancestor = parent;
            identity = parent_id;
        }
        Ok(Self {
            file_name,
            directory,
        })
    }

    #[cfg(windows)]
    fn open(parent: &Path, file_name: OsString, base: &same_file::Handle) -> Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
            FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let directory = std::fs::canonicalize(parent)?;
        let mut current = PathBuf::new();
        let mut guards = Vec::new();
        for component in directory.components() {
            current.push(component);
            if matches!(component, Component::Prefix(_)) {
                continue;
            }
            // Windows replacement uses paths. Deny renames/deletion of every
            // ancestor, then reject links introduced after canonicalization.
            // Attribute-only access does not enforce
            // the delete-sharing restriction, so request directory-list access.
            let handle = std::fs::OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&current)?;
            let metadata = handle.metadata()?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(invalid(
                    "overlay checkpoint parent changed during resolution",
                ));
            }
            let identity = same_file::Handle::from_file(handle)?;
            if &identity == base {
                return Err(invalid(
                    "overlay checkpoint must be outside the shared base directory",
                ));
            }
            guards.push(identity);
        }
        Ok(Self {
            file_name,
            directory,
            _guards: guards,
        })
    }

    #[cfg(not(any(unix, windows)))]
    fn open(_parent: &Path, _file_name: OsString, _base: &same_file::Handle) -> Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory-bound overlay checkpoints are unsupported on this platform",
        )
        .into())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlayCheckpoint {
    version: u32,
    base_id: [u8; 32],
    root: CheckpointRoot,
    overlay: OverlayData,
}

impl OverlayCheckpoint {
    #[cfg(any(unix, windows))]
    fn persist(&self, destination: &CheckpointDestination) -> Result<()> {
        #[cfg(unix)]
        let mut temporary = {
            use cap_std::fs::{Permissions, PermissionsExt};

            let temporary = cap_tempfile::TempFile::new(&destination.directory)?;
            // cap-tempfile defaults to 0666; keep checkpoint contents private.
            temporary
                .as_file()
                .set_permissions(Permissions::from_mode(0o600))?;
            temporary
        };
        #[cfg(windows)]
        let mut temporary = tempfile::NamedTempFile::new_in(&destination.directory)?;
        {
            let mut writer = BufWriter::new(temporary.as_file_mut());
            serde_json::to_writer(&mut writer, self)?;
            writer.flush()?;
        }
        temporary.as_file().sync_all()?;
        #[cfg(unix)]
        temporary.replace(&destination.file_name)?;
        #[cfg(windows)]
        temporary
            .persist(destination.directory.join(&destination.file_name))
            .map_err(|error| error.error)?;
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn persist(&self, _destination: &CheckpointDestination) -> Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory-bound overlay checkpoints are unsupported on this platform",
        )
        .into())
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
enum CheckpointRoot {
    Unicode(String),
    Native(NativeRoot),
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "encoding",
    content = "units",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
enum NativeRoot {
    UnixBytes(Vec<u8>),
    WindowsWide(Vec<u16>),
}

impl CheckpointRoot {
    fn from_path(path: &Path) -> Result<Self> {
        if let Some(path) = path.to_str() {
            return Ok(Self::Unicode(path.to_string()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Ok(Self::Native(NativeRoot::UnixBytes(
                path.as_os_str().as_bytes().to_vec(),
            )))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Ok(Self::Native(NativeRoot::WindowsWide(
                path.as_os_str().encode_wide().collect(),
            )))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "non-Unicode checkpoint roots are unsupported on this platform",
            )
            .into())
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlayData {
    files: Vec<OverlayFile>,
    deleted: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlayFile {
    path: String,
    // (trigram, location mask, next-byte mask); no base or live file IDs.
    trigrams: Vec<(u32, u8, u8)>,
}

impl OverlayData {
    fn capture(live: &LiveIndex) -> Result<Self> {
        let mut files: BTreeMap<&str, Vec<(u32, u8, u8)>> = live
            .all_paths_ordered()
            .into_iter()
            .map(|path| (path, Vec::new()))
            .collect();
        for path in files.keys() {
            validate_path(path)?;
        }
        for (&trigram, ids) in live.inverted_index() {
            for &id in ids {
                let path = live
                    .file_path(id)
                    .ok_or_else(|| invalid("overlay posting has no file path"))?;
                let masks = live.get_masks(trigram, id);
                validate_trigram(trigram, masks.loc_mask)?;
                files
                    .get_mut(path)
                    .ok_or_else(|| invalid("overlay posting has no active file"))?
                    .push((trigram, masks.loc_mask, masks.next_mask));
            }
        }
        let mut deleted = live.tombstone_paths();
        for path in &deleted {
            validate_path(path)?;
        }
        deleted.sort_unstable();
        Ok(Self {
            files: files
                .into_iter()
                .map(|(path, mut trigrams)| {
                    trigrams.sort_unstable();
                    OverlayFile {
                        path: path.to_string(),
                        trigrams,
                    }
                })
                .collect(),
            deleted,
        })
    }

    fn into_live(self) -> Result<LiveIndex> {
        if self.files.len() >= OVERLAY_BIT as usize {
            return Err(invalid("too many files in worktree overlay"));
        }
        let mut live = LiveIndex::new();
        let mut paths = HashSet::new();
        for file in self.files {
            validate_path(&file.path)?;
            if !paths.insert(file.path.clone()) {
                return Err(invalid("duplicate path in worktree overlay"));
            }
            let mut trigrams = TrigramMaskMap::default();
            for (trigram, loc_mask, next_mask) in file.trigrams {
                validate_trigram(trigram, loc_mask)?;
                if trigrams
                    .insert(
                        trigram,
                        TrigramMasks {
                            loc_mask,
                            next_mask,
                        },
                    )
                    .is_some()
                {
                    return Err(invalid("duplicate trigram in worktree overlay"));
                }
            }
            live.commit_upsert(&file.path, trigrams);
        }
        for path in self.deleted {
            validate_path(&path)?;
            if !paths.insert(path.clone()) {
                return Err(invalid(
                    "duplicate or conflicting tombstone in worktree overlay",
                ));
            }
            live.delete_file(&path);
        }
        live.reset_dirty_count();
        Ok(live)
    }
}

fn canonical_root(root: &Path) -> Result<PathBuf> {
    let root = std::fs::canonicalize(root)?;
    if !root.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "worktree root must be a directory",
        )
        .into());
    }
    Ok(root)
}

pub(crate) fn validate_path(path: &str) -> Result<()> {
    if path.contains(['\\', '\0'])
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
        || Path::new(path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid(
            "index path must be a normalized worktree-relative path",
        ));
    }
    Ok(())
}

fn validate_trigram(trigram: u32, loc_mask: u8) -> Result<()> {
    if trigram > 0x00ff_ffff || loc_mask == 0 {
        return Err(invalid(
            "invalid trigram or location mask in worktree overlay",
        ));
    }
    Ok(())
}

fn invalid(message: &str) -> Error {
    Error::IndexCorrupted(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn base_at(index_dir: &Path) -> SharedBase {
        SharedBase {
            reader: Arc::new(IndexReader::empty()),
            id: [0; 32],
            directory_id: Arc::new(same_file::Handle::from_path(index_dir).unwrap()),
        }
    }

    fn checkpoint_at(root: &Path) -> OverlayCheckpoint {
        OverlayCheckpoint {
            version: OVERLAY_VERSION,
            base_id: [0; 32],
            root: CheckpointRoot::from_path(&canonical_root(root).unwrap()).unwrap(),
            overlay: OverlayData {
                files: Vec::new(),
                deleted: vec!["removed.rs".to_string()],
            },
        }
    }

    #[test]
    fn relative_checkpoint_destination_is_pinned_to_its_canonical_parent() {
        let working_dir = std::env::current_dir().unwrap();
        let directory = tempfile::tempdir_in(&working_dir).unwrap();
        let index_dir = directory.path().join("base");
        let checkpoint_dir = directory.path().join("checkpoints");
        fs::create_dir(&index_dir).unwrap();
        fs::create_dir(&checkpoint_dir).unwrap();
        let base = base_at(&index_dir);
        let relative = checkpoint_dir
            .strip_prefix(&working_dir)
            .unwrap()
            .join("overlay.json");
        assert!(!relative.is_absolute());
        let destination = base.checkpoint_destination(&relative).unwrap();
        assert_eq!(destination.file_name, "overlay.json");
        checkpoint_at(directory.path())
            .persist(&destination)
            .unwrap();
        assert!(checkpoint_dir.join("overlay.json").is_file());
    }

    #[test]
    fn checkpoint_destination_requires_a_filename() {
        let directory = tempfile::tempdir().unwrap();
        let base = base_at(directory.path());
        for path in ["", ".", ".."] {
            assert!(base.checkpoint_destination(Path::new(path)).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn renamed_checkpoint_directory_is_not_replaced_by_its_old_path() {
        let directory = tempfile::tempdir().unwrap();
        let index_dir = directory.path().join("base");
        let checkpoint_dir = directory.path().join("checkpoints");
        let moved_dir = directory.path().join("moved");
        fs::create_dir(&index_dir).unwrap();
        fs::create_dir(&checkpoint_dir).unwrap();
        let base = base_at(&index_dir);
        let destination = base
            .checkpoint_destination(&checkpoint_dir.join("overlay.json"))
            .unwrap();

        fs::rename(&checkpoint_dir, &moved_dir).unwrap();
        fs::create_dir(&checkpoint_dir).unwrap();
        fs::write(
            checkpoint_dir.join("overlay.json"),
            b"replacement directory",
        )
        .unwrap();
        checkpoint_at(directory.path())
            .persist(&destination)
            .unwrap();
        assert!(moved_dir.join("overlay.json").is_file());
        assert_eq!(
            fs::read(checkpoint_dir.join("overlay.json")).unwrap(),
            b"replacement directory"
        );
    }

    #[cfg(windows)]
    #[test]
    fn checkpoint_directory_and_ancestors_cannot_be_renamed_during_publication() {
        let directory = tempfile::tempdir().unwrap();
        let index_dir = directory.path().join("base");
        let ancestor = directory.path().join("ancestor");
        let checkpoint_dir = ancestor.join("checkpoints");
        fs::create_dir(&index_dir).unwrap();
        fs::create_dir_all(&checkpoint_dir).unwrap();
        let base = base_at(&index_dir);
        let destination = base
            .checkpoint_destination(&checkpoint_dir.join("overlay.json"))
            .unwrap();
        for path in [&ancestor, &checkpoint_dir] {
            let error = fs::rename(path, path.with_extension("moved")).unwrap_err();
            assert!(matches!(error.raw_os_error(), Some(5 | 32)), "{error}");
        }
        checkpoint_at(directory.path())
            .persist(&destination)
            .unwrap();
        assert!(checkpoint_dir.join("overlay.json").is_file());
        drop(destination);
        fs::rename(&checkpoint_dir, checkpoint_dir.with_extension("moved")).unwrap();
        fs::rename(&ancestor, ancestor.with_extension("moved")).unwrap();
    }

    #[test]
    fn checkpoint_rejects_a_renamed_base_and_its_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let index_dir = directory.path().join("base");
        let moved_dir = directory.path().join("moved-base");
        fs::create_dir_all(index_dir.join("child")).unwrap();
        let base = base_at(&index_dir);
        fs::rename(&index_dir, &moved_dir).unwrap();
        for parent in [&moved_dir, &moved_dir.join("child")] {
            assert!(
                base.checkpoint_destination(&parent.join("overlay.json"))
                    .is_err()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_unix_root_encoding_preserves_exact_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let first = PathBuf::from(OsString::from_vec(vec![b'/', b'a', 0xfe]));
        let second = PathBuf::from(OsString::from_vec(vec![b'/', b'a', 0xff]));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        let encoded = CheckpointRoot::from_path(&first).unwrap();
        let value = serde_json::to_value(&encoded).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"encoding": "unix-bytes", "units": [47, 97, 254]})
        );
        let restored: CheckpointRoot = serde_json::from_value(value).unwrap();
        assert!(restored == encoded);
        assert!(restored != CheckpointRoot::from_path(&second).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn retargeting_parent_alias_does_not_redirect_checkpoint_into_base() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let index_dir = directory.path().join("base");
        let checkpoint_dir = directory.path().join("checkpoints");
        let alias = directory.path().join("alias");
        fs::create_dir(&index_dir).unwrap();
        fs::create_dir(&checkpoint_dir).unwrap();
        let protected = index_dir.join("index.bin");
        fs::write(&protected, b"base postings").unwrap();
        symlink(&checkpoint_dir, &alias).unwrap();
        let base = base_at(&index_dir);
        let input = alias.join("index.bin");
        let destination = base.checkpoint_destination(&input).unwrap();

        fs::remove_file(&alias).unwrap();
        symlink(&index_dir, &alias).unwrap();
        assert!(base.checkpoint_destination(&input).is_err());
        checkpoint_at(directory.path())
            .persist(&destination)
            .unwrap();
        assert_eq!(fs::read(&protected).unwrap(), b"base postings");
        assert!(checkpoint_dir.join("index.bin").is_file());
        assert_eq!(fs::read_dir(&checkpoint_dir).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn replacing_canonical_parent_with_base_symlink_does_not_redirect_publication() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        for destination_is_directory in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let index_dir = directory.path().join("base");
            let checkpoint_dir = directory.path().join("checkpoints");
            let moved_dir = directory.path().join("moved");
            fs::create_dir(&index_dir).unwrap();
            fs::create_dir(&checkpoint_dir).unwrap();
            let protected = index_dir.join("index.bin");
            fs::write(&protected, b"base postings").unwrap();
            if destination_is_directory {
                fs::create_dir(checkpoint_dir.join("index.bin")).unwrap();
            }
            let base = base_at(&index_dir);
            let destination = base
                .checkpoint_destination(&checkpoint_dir.join("index.bin"))
                .unwrap();

            fs::rename(&checkpoint_dir, &moved_dir).unwrap();
            symlink(&index_dir, &checkpoint_dir).unwrap();
            assert!(
                base.checkpoint_destination(&checkpoint_dir.join("index.bin"))
                    .is_err()
            );
            let result = checkpoint_at(directory.path()).persist(&destination);
            assert_eq!(result.is_err(), destination_is_directory);
            assert_eq!(fs::read(&protected).unwrap(), b"base postings");
            assert_eq!(fs::read_dir(&index_dir).unwrap().count(), 1);
            assert_eq!(fs::read_dir(&moved_dir).unwrap().count(), 1);
            if !destination_is_directory {
                let metadata = fs::metadata(moved_dir.join("index.bin")).unwrap();
                assert!(metadata.is_file());
                assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            }
        }
    }
}
