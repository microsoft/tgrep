/// LiveIndex: a mutable, in-memory trigram index overlay.
///
/// Used by the server to track files that have changed since the on-disk
/// index was built. Supports insert, update, and delete operations.
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::ondisk::PostingEntry;
use crate::trigram;

/// Bit flag to distinguish live index file IDs from reader file IDs.
pub const OVERLAY_BIT: u32 = 1 << 31;

/// Raw clone of LiveIndex data for background checkpoint processing.
/// Allows the expensive ID remapping to happen outside any lock.
pub struct RawIndexSnapshot {
    pub inverted: HashMap<u32, HashSet<u32>>,
    pub file_paths: HashMap<u32, String>,
}

impl RawIndexSnapshot {
    /// Remap raw data into disk-ready format (sequential IDs, sorted postings).
    pub fn into_disk_format(self) -> (Vec<String>, HashMap<u32, Vec<u32>>) {
        LiveIndex::remap_snapshot(&self.inverted, &self.file_paths)
    }
}

pub struct LiveIndex {
    /// Trigram → set of file IDs (with OVERLAY_BIT set).
    inverted: HashMap<u32, HashSet<u32>>,
    /// Per-(trigram, file_id) masks for mask-aware filtering.
    masks: HashMap<(u32, u32), trigram::TrigramMasks>,
    /// File ID → relative path.
    file_paths: HashMap<u32, String>,
    /// Path → file ID (for updates/deletes).
    path_to_id: HashMap<String, u32>,
    /// Paths that have been deleted (remove from reader results).
    deleted_paths: HashSet<String>,
    /// Next file ID counter (before applying OVERLAY_BIT).
    next_id: AtomicU32,
    /// Number of mutations since last save.
    dirty_count: u32,
}

impl Default for LiveIndex {
    fn default() -> Self {
        Self {
            inverted: HashMap::new(),
            masks: HashMap::new(),
            file_paths: HashMap::new(),
            path_to_id: HashMap::new(),
            deleted_paths: HashSet::new(),
            next_id: AtomicU32::new(0),
            dirty_count: 0,
        }
    }
}

impl LiveIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or update a file in the live index.
    pub fn upsert_file(&mut self, rel_path: &str, content: &[u8]) {
        let per_tri = Self::compute_trigram_masks(content);
        self.commit_upsert(rel_path, per_tri);
    }

    /// Extract and merge trigrams+masks for a file's contents. Pure
    /// computation — safe to call outside any index lock so callers
    /// (e.g. the file watcher) can do the heavy work without blocking
    /// concurrent searches.
    pub fn compute_trigram_masks(content: &[u8]) -> HashMap<u32, trigram::TrigramMasks> {
        let tri_masks = trigram::extract_with_masks(content);

        let mut per_tri: HashMap<u32, trigram::TrigramMasks> = HashMap::new();
        for &(tri, m) in tri_masks.iter() {
            let entry = per_tri.entry(tri).or_default();
            entry.loc_mask |= m.loc_mask;
            entry.next_mask |= m.next_mask;
        }

        // Skip the lowercase pass when content is already ASCII-lowercase —
        // it would just re-emit the same trigrams. Mirrors the on-disk
        // builder's optimization so watcher-driven reindexes don't pay for
        // a redundant full scan + allocation on lowercase-heavy files.
        let lower = content.to_ascii_lowercase();
        if lower != content {
            let lower_tri_masks = trigram::extract_with_masks(&lower);
            for &(tri, m) in lower_tri_masks.iter() {
                let entry = per_tri.entry(tri).or_default();
                entry.loc_mask |= m.loc_mask;
                entry.next_mask |= m.next_mask;
            }
        }
        per_tri
    }

    /// Commit a pre-computed set of (trigram, masks) entries for a file.
    /// Intended to run under the index write lock after the caller has
    /// already done the expensive extraction outside the lock.
    pub fn commit_upsert(&mut self, rel_path: &str, per_tri: HashMap<u32, trigram::TrigramMasks>) {
        // Remove old entry if exists
        if let Some(&old_id) = self.path_to_id.get(rel_path) {
            self.remove_file_by_id(old_id);
        }

        let raw_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let file_id = raw_id | OVERLAY_BIT;

        for (tri, m) in per_tri {
            self.inverted.entry(tri).or_default().insert(file_id);
            self.masks.insert((tri, file_id), m);
        }

        self.file_paths.insert(file_id, rel_path.to_string());
        self.path_to_id.insert(rel_path.to_string(), file_id);
        self.deleted_paths.remove(rel_path);
        self.dirty_count += 1;
    }

    /// Insert or update a file with pre-computed trigrams.
    /// This avoids extracting trigrams while holding the write lock.
    ///
    /// Note: this fast path does NOT populate `self.masks`. Pre-computed
    /// trigrams carry no mask information, so a stored entry would only
    /// ever be the "no-filter" sentinel `(u8::MAX, u8::MAX)` — which is
    /// exactly what `lookup_trigram_with_masks` already returns by default
    /// when a `(trigram, file_id)` entry is missing. Skipping the insert
    /// avoids an enormous mask map (one entry per trigram per file) during
    /// bulk indexing without changing query results.
    pub fn upsert_file_with_trigrams(&mut self, rel_path: &str, trigrams: Vec<u32>) {
        // Remove old entry if exists
        if let Some(&old_id) = self.path_to_id.get(rel_path) {
            self.remove_file_by_id(old_id);
        }

        let raw_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let file_id = raw_id | OVERLAY_BIT;

        for &tri in &trigrams {
            self.inverted.entry(tri).or_default().insert(file_id);
        }

        self.file_paths.insert(file_id, rel_path.to_string());
        self.path_to_id.insert(rel_path.to_string(), file_id);
        self.deleted_paths.remove(rel_path);
        self.dirty_count += 1;
    }

    /// Mark a file as deleted.
    pub fn delete_file(&mut self, rel_path: &str) {
        if let Some(&file_id) = self.path_to_id.get(rel_path) {
            self.remove_file_by_id(file_id);
        }
        self.deleted_paths.insert(rel_path.to_string());
        self.dirty_count += 1;
    }

    /// Look up a trigram in the live overlay (file IDs only).
    pub fn lookup_trigram(&self, trigram: u32) -> Vec<u32> {
        self.inverted
            .get(&trigram)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Look up a trigram with masks in the live overlay.
    pub fn lookup_trigram_with_masks(&self, trigram: u32) -> Vec<PostingEntry> {
        self.inverted
            .get(&trigram)
            .map(|set| {
                set.iter()
                    .map(|&fid| {
                        let m = self.masks.get(&(trigram, fid)).copied().unwrap_or(
                            trigram::TrigramMasks {
                                loc_mask: u8::MAX,
                                next_mask: u8::MAX,
                            },
                        );
                        PostingEntry {
                            file_id: fid,
                            loc_mask: m.loc_mask,
                            next_mask: m.next_mask,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get file path for a live overlay file ID.
    pub fn file_path(&self, file_id: u32) -> Option<&str> {
        self.file_paths.get(&file_id).map(|s| s.as_str())
    }

    /// Check if a path has been deleted from the overlay.
    pub fn is_deleted(&self, path: &str) -> bool {
        self.deleted_paths.contains(path)
    }

    /// Check if the overlay has an active entry for this path.
    pub fn has_path(&self, path: &str) -> bool {
        self.path_to_id.contains_key(path)
    }

    /// Number of files in the overlay.
    pub fn num_files(&self) -> usize {
        self.file_paths.len()
    }

    /// Number of unique trigrams in the overlay.
    pub fn num_trigrams(&self) -> usize {
        self.inverted.len()
    }

    /// Number of mutations since last reset.
    pub fn dirty_count(&self) -> u32 {
        self.dirty_count
    }

    /// Reset the dirty counter (e.g., after saving).
    pub fn reset_dirty_count(&mut self) {
        self.dirty_count = 0;
    }

    /// Shrink the top-level overlay maps to fit their current contents.
    /// Useful after a large prune (e.g., immediately after the post-indexing
    /// flush moves hundreds of thousands of files from the live overlay onto
    /// disk) to release the indexing-time HashMap capacity back to the
    /// allocator.
    ///
    /// Intentionally only shrinks the top-level maps. The per-trigram
    /// posting `HashSet`s are typically emptied by `prune_persisted_entries`
    /// (which removes them from `inverted` outright when they go empty), so
    /// iterating every remaining set to call `shrink_to_fit` adds work
    /// proportional to the trigram count under the global write lock with
    /// little memory benefit. Callers that need the deeper compaction can
    /// use a follow-up pass.
    pub fn shrink_to_fit(&mut self) {
        self.inverted.shrink_to_fit();
        self.masks.shrink_to_fit();
        self.file_paths.shrink_to_fit();
        self.path_to_id.shrink_to_fit();
        self.deleted_paths.shrink_to_fit();
    }

    /// Get all live file IDs.
    pub fn all_file_ids(&self) -> Vec<u32> {
        self.file_paths.keys().copied().collect()
    }

    /// Export all file paths in insertion order (by raw ID).
    pub fn all_paths_ordered(&self) -> Vec<&str> {
        let mut pairs: Vec<_> = self.file_paths.iter().collect();
        pairs.sort_by_key(|&(&id, _)| id & !OVERLAY_BIT);
        pairs.into_iter().map(|(_, p)| p.as_str()).collect()
    }

    /// Get a reference to the inverted index.
    pub fn inverted_index(&self) -> &HashMap<u32, HashSet<u32>> {
        &self.inverted
    }

    /// Check if a file ID belongs to the live overlay.
    pub fn is_overlay_id(file_id: u32) -> bool {
        file_id & OVERLAY_BIT != 0
    }

    /// Update the live index for a file on disk. Reads the file and upserts.
    pub fn update_from_disk(&mut self, root: &Path, rel_path: &str) {
        let full_path = root.join(rel_path);
        match std::fs::read(&full_path) {
            Ok(data) => {
                if trigram::is_binary(&data) {
                    self.delete_file(rel_path);
                } else {
                    self.upsert_file(rel_path, &data);
                }
            }
            Err(_) => {
                // File doesn't exist or can't be read → treat as deleted
                self.delete_file(rel_path);
            }
        }
    }

    /// Fast clone of raw data for background checkpoint.
    /// Only clones the HashMaps — no remapping or sorting. Very fast under lock.
    pub fn clone_raw_data(&self) -> RawIndexSnapshot {
        RawIndexSnapshot {
            inverted: self.inverted.clone(),
            file_paths: self.file_paths.clone(),
        }
    }

    /// Snapshot the inverted index data for disk serialization.
    /// Returns (ordered_paths, remapped_inverted_index) with sequential file IDs.
    pub fn snapshot_for_disk(&self) -> (Vec<String>, HashMap<u32, Vec<u32>>) {
        Self::remap_snapshot(&self.inverted, &self.file_paths)
    }

    /// Remap raw data into disk-ready format (sequential IDs, sorted postings).
    pub(crate) fn remap_snapshot(
        inverted: &HashMap<u32, HashSet<u32>>,
        file_paths: &HashMap<u32, String>,
    ) -> (Vec<String>, HashMap<u32, Vec<u32>>) {
        let mut pairs: Vec<_> = file_paths.iter().collect();
        pairs.sort_by_key(|&(&id, _)| id & !OVERLAY_BIT);
        let paths: Vec<&str> = pairs.iter().map(|(_, p)| p.as_str()).collect();

        let mut path_to_new_id: HashMap<&str, u32> = HashMap::new();
        for (new_id, &path) in paths.iter().enumerate() {
            path_to_new_id.insert(path, new_id as u32);
        }

        let mut remapped: HashMap<u32, Vec<u32>> = HashMap::new();
        for (&trigram, file_ids) in inverted {
            let mut posting = Vec::new();
            for &fid in file_ids {
                if let Some(path) = file_paths.get(&fid)
                    && let Some(&new_id) = path_to_new_id.get(path.as_str())
                {
                    posting.push(new_id);
                }
            }
            if !posting.is_empty() {
                posting.sort_unstable();
                remapped.insert(trigram & !OVERLAY_BIT, posting);
            }
        }

        let owned_paths: Vec<String> = paths.into_iter().map(|s| s.to_string()).collect();
        (owned_paths, remapped)
    }

    /// Remove an overlay entry by path *without* marking it as deleted.
    /// Used after a flush: the file is now in the on-disk reader, so we
    /// remove the redundant overlay copy while keeping the reader copy
    /// visible (no tombstone).
    pub fn remove_overlay_entry(&mut self, path: &str) {
        if let Some(&file_id) = self.path_to_id.get(path) {
            self.remove_file_by_id(file_id);
        }
    }

    /// Remove many overlay entries in one pass. Vastly faster than calling
    /// `remove_overlay_entry` in a loop on large overlays: a single retain
    /// over `inverted` and a single retain over `masks`, vs O(N) retains
    /// each touching every trigram. When *all* overlay entries are being
    /// removed (the common case after a successful bulk flush), the maps
    /// are simply cleared instead.
    pub fn batch_remove_overlay_entries(&mut self, paths: &[String]) {
        if paths.is_empty() {
            return;
        }
        // Resolve paths to file_ids; also remove from file_paths / path_to_id.
        let mut ids: HashSet<u32> = HashSet::with_capacity(paths.len());
        for p in paths {
            if let Some(file_id) = self.path_to_id.remove(p) {
                self.file_paths.remove(&file_id);
                ids.insert(file_id);
            }
        }
        if ids.is_empty() {
            return;
        }
        // Fast path: removing the entire overlay. Drop everything wholesale —
        // O(map size) drop instead of O(map size * removed paths) retains.
        if self.path_to_id.is_empty() {
            self.inverted.clear();
            self.masks.clear();
            return;
        }
        // Single retain over inverted: remove all matching ids from each
        // posting set in one pass; drop empty posting sets.
        self.inverted.retain(|_, set| {
            set.retain(|fid| !ids.contains(fid));
            !set.is_empty()
        });
        // Single retain over masks: drop any (tri, fid) where fid was removed.
        self.masks.retain(|&(_, fid), _| !ids.contains(&fid));
    }

    /// Return all paths currently in the overlay.
    pub fn overlay_paths(&self) -> Vec<String> {
        self.path_to_id.keys().cloned().collect()
    }

    fn remove_file_by_id(&mut self, file_id: u32) {
        // Remove from inverted index and masks
        let mut trigrams_to_clean = Vec::new();
        self.inverted.retain(|&tri, set| {
            set.remove(&file_id);
            if set.is_empty() {
                trigrams_to_clean.push(tri);
                false
            } else {
                true
            }
        });
        // Clean up mask entries for this file
        self.masks.retain(|&(_, fid), _| fid != file_id);

        if let Some(path) = self.file_paths.remove(&file_id) {
            self.path_to_id.remove(&path);
        }
    }
}
