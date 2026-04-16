/// HybridIndex: merges an on-disk IndexReader with a LiveIndex overlay.
///
/// Queries return the union of results from both layers, with the overlay
/// taking precedence for files that have been modified or deleted.
use std::path::{Path, PathBuf};

use crate::Result;
use crate::live::{self, LiveIndex};
use crate::ondisk::PostingEntry;
use crate::query::{self, QueryPlan};
use crate::reader::IndexReader;

pub struct HybridIndex {
    reader: IndexReader,
    pub live: LiveIndex,
    pub root: PathBuf,
}

impl HybridIndex {
    pub fn open(index_dir: &Path, root: &Path) -> Result<Self> {
        let reader = IndexReader::open(index_dir)?;
        Ok(Self {
            reader,
            live: LiveIndex::new(),
            root: root.to_path_buf(),
        })
    }

    /// Release the mmap reader handles so index files can be overwritten (Windows).
    pub fn drop_reader(&mut self) {
        self.reader.close();
    }

    /// Reopen the on-disk reader from updated index files, keeping the live
    /// overlay intact. Use after `drop_reader` + file replacement so that
    /// subsequent snapshots still include the on-disk data.
    pub fn reopen_reader(&mut self, index_dir: &Path) -> Result<()> {
        self.reader = IndexReader::open(index_dir)?;
        Ok(())
    }

    /// Look up candidate file IDs for a trigram, merging reader + overlay.
    pub fn lookup_trigram(&self, trigram: u32) -> Vec<u32> {
        let mut reader_ids = self.reader.lookup_trigram(trigram);
        let live_ids = self.live.lookup_trigram(trigram);

        // Filter out reader IDs for files that are deleted or overridden in overlay
        reader_ids.retain(|&fid| {
            if let Some(path) = self.reader.file_path(fid) {
                !self.live.is_deleted(path) && !self.live_has_path(path)
            } else {
                false
            }
        });

        reader_ids.extend(live_ids);
        reader_ids
    }

    /// Look up candidate posting entries with masks, merging reader + overlay.
    pub fn lookup_trigram_with_masks(&self, trigram: u32) -> Vec<PostingEntry> {
        let mut reader_entries = self.reader.lookup_trigram_with_masks(trigram);
        let live_entries = self.live.lookup_trigram_with_masks(trigram);

        reader_entries.retain(|e| {
            if let Some(path) = self.reader.file_path(e.file_id) {
                !self.live.is_deleted(path) && !self.live_has_path(path)
            } else {
                false
            }
        });

        reader_entries.extend(live_entries);
        reader_entries
    }

    /// Resolve a file ID to a path (works for both reader and overlay IDs).
    pub fn file_path(&self, file_id: u32) -> Option<&str> {
        if live::LiveIndex::is_overlay_id(file_id) {
            self.live.file_path(file_id)
        } else {
            self.reader.file_path(file_id)
        }
    }

    /// Get all file IDs from both layers (overlay takes precedence).
    pub fn all_file_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .reader
            .all_file_ids()
            .into_iter()
            .filter(|&fid| {
                if let Some(path) = self.reader.file_path(fid) {
                    !self.live.is_deleted(path) && !self.live_has_path(path)
                } else {
                    false
                }
            })
            .collect();
        ids.extend(self.live.all_file_ids());
        ids
    }

    /// Execute a query plan against the hybrid index.
    pub fn execute_query(&self, plan: &QueryPlan) -> Vec<u32> {
        if plan.is_match_all() {
            return self.all_file_ids();
        }
        query::execute_plan(plan, &|tri| self.lookup_trigram(tri))
    }

    /// Execute a query plan with mask-aware filtering.
    pub fn execute_query_with_masks(&self, plan: &QueryPlan, pattern: &str) -> Vec<u32> {
        if plan.is_match_all() {
            return self.all_file_ids();
        }
        query::execute_plan_with_masks(plan, pattern, &|tri| self.lookup_trigram_with_masks(tri))
    }

    /// Total number of files across both layers.
    pub fn num_files(&self) -> usize {
        self.all_file_ids().len()
    }

    /// Total unique trigrams across both reader and live overlay.
    pub fn num_trigrams(&self) -> usize {
        let reader_count = self.reader.num_trigrams();
        let live_count = self.live.num_trigrams();
        if reader_count == 0 {
            return live_count;
        }
        if live_count == 0 {
            return reader_count;
        }
        // Both have data — return the larger as a reasonable estimate
        // (exact count would require merging the trigram sets)
        reader_count.max(live_count)
    }

    /// Full path on disk for a file ID.
    pub fn full_path(&self, file_id: u32) -> Option<PathBuf> {
        self.file_path(file_id).map(|rel| {
            self.root
                .join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
        })
    }

    fn live_has_path(&self, path: &str) -> bool {
        self.live.has_path(path)
    }

    /// Get all paths from the on-disk reader (for skip-set construction).
    pub fn reader_paths(&self) -> std::collections::HashSet<String> {
        self.reader.all_paths().iter().cloned().collect()
    }

    /// Number of files in the on-disk reader.
    pub fn reader_file_count(&self) -> usize {
        self.reader.num_files()
    }

    /// Remove overlay entries whose paths already exist in the on-disk reader.
    ///
    /// After a flush + `reopen_reader`, the reader contains a superset of the
    /// snapshot data.  Any overlay entry that is also present in the reader is
    /// now redundant — removing it avoids duplicate work during queries and
    /// prevents unbounded overlay growth.  Entries added *after* the snapshot
    /// (e.g. by the file-watcher) are preserved because they are **not** in
    /// the reader yet.
    pub fn prune_persisted_entries(&mut self) {
        let reader_paths: std::collections::HashSet<&str> =
            self.reader.all_paths().iter().map(|s| s.as_str()).collect();
        let to_remove: Vec<String> = self
            .live
            .overlay_paths()
            .into_iter()
            .filter(|p| reader_paths.contains(p.as_str()))
            .collect();
        for path in &to_remove {
            self.live.remove_overlay_entry(path);
        }
    }

    /// Produce a full snapshot merging reader + overlay for disk serialization.
    /// Reader files not superseded by overlay are included with remapped IDs.
    pub fn full_snapshot(&self) -> (Vec<String>, std::collections::HashMap<u32, Vec<u32>>) {
        use std::collections::HashMap;

        // Phase 1: Build merged file list (reader files not in overlay + overlay files)
        let mut paths: Vec<String> = Vec::new();
        let mut reader_id_map: HashMap<u32, u32> = HashMap::new();

        // Add reader files (skip those superseded by overlay or deleted)
        for (old_id, path) in self.reader.all_paths().iter().enumerate() {
            let old_id = old_id as u32;
            if self.live.is_deleted(path) || self.live.has_path(path) {
                continue; // superseded or deleted
            }
            let new_id = paths.len() as u32;
            reader_id_map.insert(old_id, new_id);
            paths.push(path.clone());
        }

        // Add overlay files
        let overlay_paths = self.live.all_paths_ordered();
        let mut overlay_path_to_new_id: HashMap<&str, u32> = HashMap::new();
        for &op in &overlay_paths {
            let new_id = paths.len() as u32;
            overlay_path_to_new_id.insert(op, new_id);
            paths.push(op.to_string());
        }

        // Phase 2: Build merged inverted index
        let mut inverted: HashMap<u32, Vec<u32>> = HashMap::new();

        // Reader trigram postings (remapped, excluding superseded files)
        for (trigram, posting) in self.reader.all_trigram_postings() {
            let remapped: Vec<u32> = posting
                .into_iter()
                .filter_map(|old_id| reader_id_map.get(&old_id).copied())
                .collect();
            if !remapped.is_empty() {
                inverted.entry(trigram).or_default().extend(remapped);
            }
        }

        // Overlay trigram postings (remapped)
        let overlay_inverted = self.live.inverted_index();
        for (&trigram, file_ids) in overlay_inverted {
            let tri_clean = trigram & !crate::live::OVERLAY_BIT;
            let remapped: Vec<u32> = file_ids
                .iter()
                .filter_map(|&fid| {
                    self.live
                        .file_path(fid)
                        .and_then(|p| overlay_path_to_new_id.get(p).copied())
                })
                .collect();
            if !remapped.is_empty() {
                inverted.entry(tri_clean).or_default().extend(remapped);
            }
        }

        // Sort all posting lists
        for posting in inverted.values_mut() {
            posting.sort_unstable();
        }

        (paths, inverted)
    }
}
