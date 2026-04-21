/// HybridIndex: merges an on-disk IndexReader with a LiveIndex overlay.
///
/// Queries return the union of results from both layers, with the overlay
/// taking precedence for files that have been modified or deleted.
///
/// **Concurrency**: the on-disk `IndexReader` is held inside an internal
/// `RwLock<Arc<IndexReader>>`, which lets the publish path swap the reader
/// **without** the caller having to hold an exclusive (`&mut`) reference to
/// the `HybridIndex`. This means `tgrep serve` can safely keep search
/// queries running with only an outer read lock during a flush — the brief
/// inner write lock around the `Arc` swap takes microseconds and the old
/// reader's mmap is released only after the last in-flight query drops its
/// `Arc<IndexReader>`.
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::Result;
use crate::live::{self, LiveIndex};
use crate::ondisk::PostingEntry;
use crate::query::{self, QueryPlan};
use crate::reader::IndexReader;

pub struct HybridIndex {
    reader: RwLock<Arc<IndexReader>>,
    pub live: LiveIndex,
    pub root: PathBuf,
}

impl HybridIndex {
    pub fn open(index_dir: &Path, root: &Path) -> Result<Self> {
        let reader = IndexReader::open(index_dir)?;
        // Reject degenerate readers (files present but 0 trigrams) at startup.
        // This matches the retry logic in flush_index_to_disk.
        if reader.is_degenerate() {
            return Err(crate::Error::IndexCorrupted(format!(
                "degenerate reader: {} files but 0 trigrams",
                reader.num_files()
            )));
        }
        // Validate and warm up the lookup mmap so the first searches after
        // startup don't hit cold pages (which caused zero-candidate results
        // on Windows).
        if let Err(msg) = reader.validate_lookup() {
            return Err(crate::Error::IndexCorrupted(msg));
        }
        Ok(Self {
            reader: RwLock::new(Arc::new(reader)),
            live: LiveIndex::new(),
            root: root.to_path_buf(),
        })
    }

    /// Snapshot the current on-disk reader. Cheap (clones an `Arc`).
    fn reader(&self) -> Arc<IndexReader> {
        Arc::clone(&self.reader.read().unwrap())
    }

    /// Atomically replace the on-disk reader with `new_reader`.
    ///
    /// Takes `&self` (not `&mut self`) so that callers can perform the swap
    /// while holding only an outer read lock on the `HybridIndex`, which in
    /// turn means concurrent search queries are not blocked during a flush.
    /// The previous reader's mmap is released when the last in-flight query
    /// drops its `Arc<IndexReader>` — Rust's `File::open` on Windows uses
    /// `FILE_SHARE_DELETE` by default, so renaming the underlying files
    /// before the old mmap is dropped is safe (the old section keeps the
    /// orphaned file content alive until refs drain).
    pub fn swap_reader(&self, new_reader: IndexReader) {
        *self.reader.write().unwrap() = Arc::new(new_reader);
    }

    /// Replace the reader with an empty one and drop this `HybridIndex`'s
    /// reference to the previous reader.
    ///
    /// **Note**: this does *not* guarantee an immediate unmap of the
    /// previous on-disk index files. Because the reader is held inside an
    /// `Arc<IndexReader>`, the underlying mmap section is only released
    /// once the last in-flight reference (e.g. `Arc<IndexReader>` clones
    /// held by concurrent search queries) is dropped. Callers that need
    /// the file handles released before, say, overwriting the underlying
    /// files on platforms that disallow it must additionally ensure no
    /// outstanding readers exist.
    ///
    /// Retained for callers that need the old "drop then re-open" sequence;
    /// new code should prefer `swap_reader` so there is no window during
    /// which the reader is empty.
    pub fn drop_reader(&self) {
        self.swap_reader(IndexReader::empty());
    }

    /// Reopen the on-disk reader from updated index files, keeping the live
    /// overlay intact. Equivalent to `swap_reader(IndexReader::open(..)?)`.
    pub fn reopen_reader(&self, index_dir: &Path) -> Result<()> {
        let new_reader = IndexReader::open(index_dir)?;
        self.swap_reader(new_reader);
        Ok(())
    }

    /// Look up candidate file IDs for a trigram, merging reader + overlay.
    pub fn lookup_trigram(&self, trigram: u32) -> Vec<u32> {
        let reader = self.reader();
        let mut reader_ids = reader.lookup_trigram(trigram);
        let live_ids = self.live.lookup_trigram(trigram);

        // Filter out reader IDs for files that are deleted or overridden in overlay
        reader_ids.retain(|&fid| {
            if let Some(path) = reader.file_path(fid) {
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
        let reader = self.reader();
        let mut reader_entries = reader.lookup_trigram_with_masks(trigram);
        let live_entries = self.live.lookup_trigram_with_masks(trigram);

        reader_entries.retain(|e| {
            if let Some(path) = reader.file_path(e.file_id) {
                !self.live.is_deleted(path) && !self.live_has_path(path)
            } else {
                false
            }
        });

        reader_entries.extend(live_entries);
        reader_entries
    }

    /// Resolve a file ID to a path (works for both reader and overlay IDs).
    ///
    /// Returns an owned `String` so the result is safe to use after the
    /// internal reader is swapped out by a concurrent flush.
    pub fn file_path(&self, file_id: u32) -> Option<String> {
        if live::LiveIndex::is_overlay_id(file_id) {
            self.live.file_path(file_id).map(|s| s.to_string())
        } else {
            self.reader().file_path(file_id).map(|s| s.to_string())
        }
    }

    /// Get all file IDs from both layers (overlay takes precedence).
    pub fn all_file_ids(&self) -> Vec<u32> {
        let reader = self.reader();
        let mut ids: Vec<u32> = reader
            .all_file_ids()
            .into_iter()
            .filter(|&fid| {
                if let Some(path) = reader.file_path(fid) {
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
        // Snapshot reader once to ensure all trigram lookups use the same
        // reader version — prevents race conditions during concurrent flushes.
        let reader = self.reader();
        query::execute_plan(plan, &|tri| self.lookup_trigram_using_reader(tri, &reader))
    }

    /// Execute a query plan with mask-aware filtering.
    pub fn execute_query_with_masks(&self, plan: &QueryPlan) -> Vec<u32> {
        if plan.is_match_all() {
            return self.all_file_ids();
        }
        // Snapshot reader once to ensure all trigram lookups use the same
        // reader version — prevents race conditions during concurrent flushes.
        let reader = self.reader();
        query::execute_plan_with_masks(plan, &|tri| {
            self.lookup_trigram_with_masks_using_reader(tri, &reader)
        })
    }

    /// Look up candidate file IDs for a trigram using a specific reader snapshot.
    /// Ensures all trigrams in a query are evaluated against the same reader version.
    fn lookup_trigram_using_reader(&self, trigram: u32, reader: &Arc<IndexReader>) -> Vec<u32> {
        let mut reader_ids = reader.lookup_trigram(trigram);
        let live_ids = self.live.lookup_trigram(trigram);

        reader_ids.retain(|&fid| {
            if let Some(path) = reader.file_path(fid) {
                !self.live.is_deleted(path) && !self.live_has_path(path)
            } else {
                false
            }
        });

        reader_ids.extend(live_ids);
        reader_ids
    }

    /// Look up candidate posting entries with masks using a specific reader snapshot.
    /// Ensures all trigrams in a query are evaluated against the same reader version.
    fn lookup_trigram_with_masks_using_reader(
        &self,
        trigram: u32,
        reader: &Arc<IndexReader>,
    ) -> Vec<PostingEntry> {
        let mut reader_entries = reader.lookup_trigram_with_masks(trigram);
        let live_entries = self.live.lookup_trigram_with_masks(trigram);

        reader_entries.retain(|e| {
            if let Some(path) = reader.file_path(e.file_id) {
                !self.live.is_deleted(path) && !self.live_has_path(path)
            } else {
                false
            }
        });

        reader_entries.extend(live_entries);
        reader_entries
    }

    /// Total number of files across both layers.
    pub fn num_files(&self) -> usize {
        self.all_file_ids().len()
    }

    /// Total unique trigrams across both reader and live overlay.
    pub fn num_trigrams(&self) -> usize {
        let reader_count = self.reader().num_trigrams();
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
        self.reader().all_paths().iter().cloned().collect()
    }

    /// Number of files in the on-disk reader.
    pub fn reader_file_count(&self) -> usize {
        self.reader().num_files()
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
        let reader = self.reader();
        let reader_paths: std::collections::HashSet<&str> =
            reader.all_paths().iter().map(|s| s.as_str()).collect();

        // Fast path: after a successful bulk flush every overlay path is
        // already in the new reader. Swap all overlay maps out for empty
        // ones (microseconds) and let a background thread drop the old
        // contents — keeping the multi-second drop work off the index
        // write lock so concurrent searches stay responsive.
        if self.live.try_drop_all_persisted(&reader_paths) {
            return;
        }

        // Selective fall-back: prune only the overlay entries that are now
        // in the reader, leaving fresher mutations alone.
        let to_remove: Vec<String> = self
            .live
            .overlay_paths()
            .into_iter()
            .filter(|p| reader_paths.contains(p.as_str()))
            .collect();
        self.live.batch_remove_overlay_entries(&to_remove);
    }

    /// Produce a full snapshot merging reader + overlay for disk serialization.
    /// Reader files not superseded by overlay are included with remapped IDs.
    /// Preserves loc_mask and next_mask from both the on-disk reader and the
    /// live overlay so that Bloom-filter optimizations survive flush cycles.
    pub fn full_snapshot(
        &self,
    ) -> (
        Vec<String>,
        std::collections::HashMap<u32, Vec<PostingEntry>>,
    ) {
        use std::collections::HashMap;

        let reader = self.reader();

        // Phase 1: Build merged file list (reader files not in overlay + overlay files)
        let mut paths: Vec<String> = Vec::new();
        let mut reader_id_map: HashMap<u32, u32> = HashMap::new();

        // Add reader files (skip those superseded by overlay or deleted)
        for (old_id, path) in reader.all_paths().iter().enumerate() {
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

        // Phase 2: Build merged inverted index with masks
        let mut inverted: HashMap<u32, Vec<PostingEntry>> = HashMap::new();

        // Reader trigram postings with masks (remapped, excluding superseded files)
        for (trigram, posting) in reader.all_trigram_postings_with_masks() {
            let remapped: Vec<PostingEntry> = posting
                .into_iter()
                .filter_map(|entry| {
                    reader_id_map
                        .get(&entry.file_id)
                        .map(|&new_id| PostingEntry {
                            file_id: new_id,
                            loc_mask: entry.loc_mask,
                            next_mask: entry.next_mask,
                        })
                })
                .collect();
            if !remapped.is_empty() {
                inverted.entry(trigram).or_default().extend(remapped);
            }
        }

        // Overlay trigram postings with masks (remapped)
        let overlay_inverted = self.live.inverted_index();
        for (&trigram, file_ids) in overlay_inverted {
            let tri_clean = trigram & !crate::live::OVERLAY_BIT;
            let remapped: Vec<PostingEntry> = file_ids
                .iter()
                .filter_map(|&fid| {
                    self.live.file_path(fid).and_then(|p| {
                        overlay_path_to_new_id.get(p).map(|&new_id| {
                            let m = self.live.get_masks(trigram, fid);
                            PostingEntry {
                                file_id: new_id,
                                loc_mask: m.loc_mask,
                                next_mask: m.next_mask,
                            }
                        })
                    })
                })
                .collect();
            if !remapped.is_empty() {
                inverted.entry(tri_clean).or_default().extend(remapped);
            }
        }

        // Sort all posting lists by file_id
        for posting in inverted.values_mut() {
            posting.sort_by_key(|e| e.file_id);
        }

        (paths, inverted)
    }
}
