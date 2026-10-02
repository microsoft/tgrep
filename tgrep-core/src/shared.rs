//! Shared, read-only content-index bases with independent worktree overlays.
//!
//! This is a core building block, not Git change discovery or a multi-worktree
//! server. The caller must keep the base files immutable while mapped and
//! populate each overlay with *all* differences from that base before querying.
//! A worktree also needs its own traversal/visibility state and content cache;
//! neither is represented by these content-index checkpoints.

use std::collections::{BTreeMap, HashSet};
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
    index_dir: PathBuf,
}

impl SharedBase {
    /// Open a complete, current-format base in an immutable snapshot directory.
    ///
    /// Do not use a directory being rewritten by `tgrep index` or `tgrep serve`.
    /// Legacy and incomplete indexes remain usable through the existing APIs,
    /// but must be rebuilt before they can be used as shared bases.
    pub fn open(index_dir: &Path) -> Result<Self> {
        let index_dir = std::fs::canonicalize(index_dir)?;
        let reader = HybridIndex::open_reader(&index_dir)?;
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
            index_dir,
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
    /// must be outside the base snapshot directory.
    /// Saving does not prune the live overlay or reset its dirty counter.
    pub fn save_overlay(&self, worktree: &HybridIndex, path: &Path) -> Result<()> {
        if !Arc::ptr_eq(&self.reader, &worktree.reader_arc()) {
            return Err(invalid("worktree no longer uses this shared base"));
        }
        let checkpoint = OverlayCheckpoint {
            version: OVERLAY_VERSION,
            base_id: self.id,
            root: canonical_root(&worktree.root)?,
            overlay: OverlayData::capture(&worktree.live)?,
        };
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = std::fs::canonicalize(parent)?;
        if parent.starts_with(&self.index_dir) {
            return Err(invalid(
                "overlay checkpoint must be outside the shared base directory",
            ));
        }
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        {
            let mut writer = BufWriter::new(temporary.as_file_mut());
            serde_json::to_writer(&mut writer, &checkpoint)?;
            writer.flush()?;
        }
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
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
        if checkpoint.root != root {
            return Err(invalid("worktree overlay belongs to a different root"));
        }
        let live = checkpoint.overlay.into_live()?;
        let mut worktree = HybridIndex::from_reader(Arc::clone(&self.reader), &root);
        worktree.live = live;
        Ok(worktree)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlayCheckpoint {
    version: u32,
    base_id: [u8; 32],
    root: PathBuf,
    overlay: OverlayData,
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

fn validate_path(path: &str) -> Result<()> {
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
