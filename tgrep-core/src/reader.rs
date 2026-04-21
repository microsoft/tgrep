/// Mmap-based read-only index reader.
///
/// Uses memory-mapped files for zero-copy access to `lookup.bin` and
/// `index.bin`. Binary searches the sorted lookup table to find
/// posting lists for queried trigrams.
use memmap2::Mmap;
use std::fs::File;
use std::path::Path;

use crate::Result;
use crate::ondisk::{self, LOOKUP_ENTRY_SIZE, LookupEntry, POSTING_ENTRY_SIZE, PostingEntry};

pub struct IndexReader {
    lookup: Option<Mmap>,
    postings: Option<Mmap>,
    file_paths: Vec<String>,
    num_entries: usize,
}

impl IndexReader {
    pub fn open(index_dir: &Path) -> Result<Self> {
        let lookup_path = index_dir.join("lookup.bin");
        let postings_path = index_dir.join("index.bin");
        let files_path = index_dir.join("files.bin");

        if !lookup_path.exists() || !postings_path.exists() || !files_path.exists() {
            return Err(crate::Error::IndexNotFound(index_dir.display().to_string()));
        }

        // Map files first, then use *mmap length* as the source of truth.
        //
        // On Windows, `File::metadata().len()` (fstat) can transiently return
        // zero for a file that was just renamed into place by
        // `move_staged_files`, because NTFS metadata updates are not always
        // immediately visible — even from the same file handle. Since memmap2
        // internally queries `File::metadata().len()` to determine the mapping
        // size, we bypass it by seeking to the end of the file to get the true
        // length, then passing it explicitly via `MmapOptions::len()`.
        //
        // `SetFilePointerEx` (which backs `file.seek(SeekFrom::End(0))`) reads
        // the file object's size directly from the kernel, which is always
        // up-to-date even when the NTFS directory-entry metadata cache has not
        // yet been invalidated.
        use memmap2::MmapOptions;
        use std::io::{Seek, SeekFrom};

        let mut lookup_file = File::open(&lookup_path)?;
        let mut postings_file = File::open(&postings_path)?;

        let lookup_len = lookup_file.seek(SeekFrom::End(0))? as usize;
        let postings_len = postings_file.seek(SeekFrom::End(0))? as usize;

        let (lookup, postings, num_entries) = if lookup_len == 0 || postings_len == 0 {
            (None, None, 0)
        } else {
            // A corrupted lookup.bin whose size is not a multiple of the fixed
            // entry size would cause silent truncation of the trailing entry
            // (and binary search would still see it via integer division).
            // Reject up-front so the failure is loud and obvious.
            if !lookup_len.is_multiple_of(LOOKUP_ENTRY_SIZE) {
                return Err(crate::Error::IndexCorrupted(format!(
                    "lookup.bin size {} is not a multiple of {}",
                    lookup_len, LOOKUP_ENTRY_SIZE
                )));
            }
            // SAFETY: Files are opened read-only and the Mmap lifetime is tied
            // to IndexReader. The close() method drops the mappings before any
            // file overwrites (required on Windows).
            let lk = unsafe { MmapOptions::new().len(lookup_len).map(&lookup_file)? };
            let pk = unsafe { MmapOptions::new().len(postings_len).map(&postings_file)? };
            let n = lk.len() / LOOKUP_ENTRY_SIZE;
            (Some(lk), Some(pk), n)
        };

        // Load file paths. A truncated files.bin used to be silently accepted,
        // resulting in queries that returned empty file paths for high IDs.
        let files_data = std::fs::read(&files_path)?;
        let file_entries = ondisk::decode_file_entries(&files_data)?;

        // Validate that file IDs are dense (0..N) with no duplicates.
        // Without this, a corrupted files.bin declaring an id like
        // u32::MAX would cause `vec![String::new(); max_id + 1]` to attempt
        // a multi-GiB allocation and likely OOM/crash. Current writers
        // always assign dense IDs starting at 0, so this is a strict
        // tightening of the format invariant rather than a behavior change.
        let n = file_entries.len();
        let mut seen = vec![false; n];
        for (id, _) in &file_entries {
            let idx = *id as usize;
            if idx >= n {
                return Err(crate::Error::IndexCorrupted(format!(
                    "files.bin contains out-of-range file_id {id} (entry count = {n}); \
                     IDs must be dense in 0..{n}"
                )));
            }
            if seen[idx] {
                return Err(crate::Error::IndexCorrupted(format!(
                    "files.bin contains duplicate file_id {id}"
                )));
            }
            seen[idx] = true;
        }
        let mut file_paths = vec![String::new(); n];
        for (id, path) in file_entries {
            file_paths[id as usize] = path;
        }

        Ok(Self {
            lookup,
            postings,
            file_paths,
            num_entries,
        })
    }

    /// An empty reader that returns no results. Useful as a placeholder
    /// when callers need to release the previous mmap before swapping in a
    /// freshly-built reader.
    pub fn empty() -> Self {
        Self {
            lookup: None,
            postings: None,
            file_paths: Vec::new(),
            num_entries: 0,
        }
    }

    /// Release mmap handles so the underlying files can be overwritten (Windows).
    pub fn close(&mut self) {
        self.lookup = None;
        self.postings = None;
        self.file_paths.clear();
        self.num_entries = 0;
    }

    /// Binary search the lookup table for a trigram hash.
    /// Returns the posting list (file IDs only) or an empty vec if not found.
    pub fn lookup_trigram(&self, trigram: u32) -> Vec<u32> {
        self.lookup_trigram_with_masks(trigram)
            .into_iter()
            .map(|e| e.file_id)
            .collect()
    }

    /// Binary search the lookup table for a trigram hash.
    /// Returns full posting entries with masks.
    pub fn lookup_trigram_with_masks(&self, trigram: u32) -> Vec<PostingEntry> {
        if self.lookup.is_none() {
            return Vec::new();
        }
        let idx = self.binary_search(trigram);
        match idx {
            Some(i) => {
                let entry = self.read_lookup_entry(i);
                self.read_posting_entries(entry.offset, entry.length)
            }
            None => Vec::new(),
        }
    }

    /// Get file path by ID.
    pub fn file_path(&self, file_id: u32) -> Option<&str> {
        self.file_paths.get(file_id as usize).map(|s| s.as_str())
    }

    /// Total number of indexed files.
    pub fn num_files(&self) -> usize {
        self.file_paths.len()
    }

    /// Total number of unique trigrams.
    pub fn num_trigrams(&self) -> usize {
        self.num_entries
    }

    /// Returns `true` when the reader has files but zero trigram entries.
    ///
    /// This is a degenerate state that should never occur for a well-formed
    /// index with > 0 files — every indexed file produces at least one
    /// trigram. When detected after a flush, it indicates the mmap was
    /// opened against stale / zero-length metadata (observed on Windows
    /// NTFS after rapid file renames) and the reader should be reopened.
    pub fn is_degenerate(&self) -> bool {
        !self.file_paths.is_empty() && self.num_entries == 0
    }

    /// Validate that the lookup table is sorted by trigram hash and that
    /// posting-list ranges stay within `index.bin` bounds.
    ///
    /// Returns `Ok(())` when the table is well-formed. As a side-effect, this
    /// sequentially reads every lookup entry, warming the mmap pages into the
    /// OS page cache so that subsequent binary searches never hit cold pages.
    pub fn validate_lookup(&self) -> std::result::Result<(), String> {
        let lookup = match self.lookup.as_ref() {
            Some(l) => l,
            None => return Ok(()), // empty index, nothing to validate
        };
        let postings_len = self.postings.as_ref().map_or(0, |p| p.len());
        let mut prev_trigram: Option<u32> = None;
        for i in 0..self.num_entries {
            let entry = self.read_lookup_entry(i);
            if let Some(prev) = prev_trigram
                && entry.trigram <= prev
            {
                return Err(format!(
                    "lookup.bin not sorted at entry {i}: trigram {:#x} <= prev {:#x}",
                    entry.trigram, prev
                ));
            }
            let byte_len = (entry.length as usize).checked_mul(POSTING_ENTRY_SIZE);
            let end = byte_len.and_then(|bl| (entry.offset as usize).checked_add(bl));
            match end {
                Some(e) if e <= postings_len => {}
                _ => {
                    return Err(format!(
                        "lookup entry {i} (trigram {:#x}): posting range \
                         [offset={}, length={}] exceeds index.bin length {postings_len}",
                        entry.trigram, entry.offset, entry.length
                    ));
                }
            }
            prev_trigram = Some(entry.trigram);
        }
        // Touch last byte of lookup to ensure the final page is paged in.
        if !lookup.is_empty() {
            std::hint::black_box(lookup[lookup.len() - 1]);
        }
        Ok(())
    }

    /// Get all file IDs present in this reader.
    pub fn all_file_ids(&self) -> Vec<u32> {
        (0..self.file_paths.len() as u32).collect()
    }

    /// Get all file paths in this reader (for skip-set construction).
    pub fn all_paths(&self) -> &[String] {
        &self.file_paths
    }

    /// Iterate all trigram entries in the lookup table.
    /// Returns (trigram_hash, posting_list) for each entry.
    pub fn all_trigram_postings(&self) -> Vec<(u32, Vec<u32>)> {
        let mut result = Vec::with_capacity(self.num_entries);
        for i in 0..self.num_entries {
            let entry = self.read_lookup_entry(i);
            let postings = self.read_posting_entries(entry.offset, entry.length);
            let file_ids: Vec<u32> = postings.into_iter().map(|e| e.file_id).collect();
            result.push((entry.trigram, file_ids));
        }
        result
    }

    /// Iterate all trigram entries with full posting data (including masks).
    /// Returns (trigram_hash, Vec<PostingEntry>) preserving loc_mask/next_mask.
    pub fn all_trigram_postings_with_masks(&self) -> Vec<(u32, Vec<PostingEntry>)> {
        let mut result = Vec::with_capacity(self.num_entries);
        for i in 0..self.num_entries {
            let entry = self.read_lookup_entry(i);
            let postings = self.read_posting_entries(entry.offset, entry.length);
            result.push((entry.trigram, postings));
        }
        result
    }

    fn binary_search(&self, trigram: u32) -> Option<usize> {
        let mut lo = 0usize;
        let mut hi = self.num_entries;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let entry = self.read_lookup_entry(mid);
            match entry.trigram.cmp(&trigram) {
                std::cmp::Ordering::Equal => return Some(mid),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    fn read_lookup_entry(&self, index: usize) -> LookupEntry {
        let lookup = self.lookup.as_ref().unwrap();
        let start = index * LOOKUP_ENTRY_SIZE;
        let buf: &[u8; LOOKUP_ENTRY_SIZE] =
            lookup[start..start + LOOKUP_ENTRY_SIZE].try_into().unwrap();
        LookupEntry::decode(buf)
    }

    fn read_posting_entries(&self, offset: u64, length: u32) -> Vec<PostingEntry> {
        let postings = match self.postings.as_ref() {
            Some(p) => p,
            None => return Vec::new(),
        };
        let mut result = Vec::with_capacity(length as usize);
        let start = offset as usize;
        for i in 0..length as usize {
            let pos = start + i * POSTING_ENTRY_SIZE;
            if pos + POSTING_ENTRY_SIZE <= postings.len() {
                let buf: &[u8; POSTING_ENTRY_SIZE] =
                    postings[pos..pos + POSTING_ENTRY_SIZE].try_into().unwrap();
                result.push(PostingEntry::decode(buf));
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    /// Write a minimal trio of index files so `IndexReader::open` runs the
    /// validation paths we want to exercise.
    fn write_index(dir: &Path, lookup_bytes: &[u8], postings_bytes: &[u8], files_bytes: &[u8]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut f = std::fs::File::create(dir.join("lookup.bin")).unwrap();
        f.write_all(lookup_bytes).unwrap();
        let mut f = std::fs::File::create(dir.join("index.bin")).unwrap();
        f.write_all(postings_bytes).unwrap();
        let mut f = std::fs::File::create(dir.join("files.bin")).unwrap();
        f.write_all(files_bytes).unwrap();
    }

    fn open_err(dir: &Path) -> crate::Error {
        match IndexReader::open(dir) {
            Ok(_) => panic!("expected IndexReader::open to fail"),
            Err(e) => e,
        }
    }

    #[test]
    fn open_rejects_lookup_with_non_multiple_size() {
        let tmp = TempDir::new().unwrap();
        // 1 byte short of a single LOOKUP_ENTRY_SIZE-sized record.
        let lookup = vec![0u8; LOOKUP_ENTRY_SIZE - 1];
        // postings non-empty so we get past the `len() == 0` short-circuit.
        let postings = vec![0u8; POSTING_ENTRY_SIZE];
        write_index(tmp.path(), &lookup, &postings, &[]);
        match open_err(tmp.path()) {
            crate::Error::IndexCorrupted(msg) => {
                assert!(
                    msg.contains("lookup.bin"),
                    "expected lookup.bin diagnostic, got: {msg}"
                );
            }
            other => panic!("expected IndexCorrupted, got {other:?}"),
        }
    }

    #[test]
    fn open_accepts_empty_index() {
        // Both lookup.bin and postings.bin are zero-length: legitimate case
        // for a freshly-created server with no files indexed yet.
        let tmp = TempDir::new().unwrap();
        write_index(tmp.path(), &[], &[], &[]);
        let reader = IndexReader::open(tmp.path()).expect("empty index should open");
        assert_eq!(reader.num_entries, 0);
        assert!(reader.file_paths.is_empty());
    }

    #[test]
    fn open_rejects_files_with_out_of_range_id() {
        let tmp = TempDir::new().unwrap();
        // Single record whose declared file_id (5) is past entry count (1).
        // Without the dense-id check this would attempt to allocate a
        // `Vec<String>` of size 6 for a single entry — and a u32::MAX id
        // would attempt a multi-GiB allocation.
        let entry = ondisk::encode_file_entry(5, "a.rs").unwrap();
        write_index(tmp.path(), &[], &[], &entry);
        match open_err(tmp.path()) {
            crate::Error::IndexCorrupted(msg) => assert!(msg.contains("out-of-range")),
            other => panic!("expected IndexCorrupted, got {other:?}"),
        }
    }

    #[test]
    fn open_rejects_files_with_duplicate_id() {
        let tmp = TempDir::new().unwrap();
        let mut buf = ondisk::encode_file_entry(0, "a.rs").unwrap();
        buf.extend_from_slice(&ondisk::encode_file_entry(0, "b.rs").unwrap());
        write_index(tmp.path(), &[], &[], &buf);
        match open_err(tmp.path()) {
            crate::Error::IndexCorrupted(msg) => assert!(msg.contains("duplicate")),
            other => panic!("expected IndexCorrupted, got {other:?}"),
        }
    }

    #[test]
    fn open_accepts_dense_files_table() {
        let tmp = TempDir::new().unwrap();
        let mut buf = ondisk::encode_file_entry(0, "a.rs").unwrap();
        buf.extend_from_slice(&ondisk::encode_file_entry(1, "b.rs").unwrap());
        buf.extend_from_slice(&ondisk::encode_file_entry(2, "c.rs").unwrap());
        write_index(tmp.path(), &[], &[], &buf);
        let reader = IndexReader::open(tmp.path()).expect("dense IDs should be accepted");
        assert_eq!(reader.file_paths.len(), 3);
        assert_eq!(reader.file_paths[0], "a.rs");
        assert_eq!(reader.file_paths[1], "b.rs");
        assert_eq!(reader.file_paths[2], "c.rs");
    }

    #[test]
    fn is_degenerate_detects_files_without_trigrams() {
        let tmp = TempDir::new().unwrap();
        // Files present but lookup/postings are empty → degenerate reader
        let files = ondisk::encode_file_entry(0, "a.rs").unwrap();
        write_index(tmp.path(), &[], &[], &files);
        let reader = IndexReader::open(tmp.path()).expect("should open");
        assert!(reader.is_degenerate(), "files but no trigrams = degenerate");
    }

    #[test]
    fn is_degenerate_false_for_empty_index() {
        let tmp = TempDir::new().unwrap();
        write_index(tmp.path(), &[], &[], &[]);
        let reader = IndexReader::open(tmp.path()).expect("should open");
        assert!(!reader.is_degenerate(), "empty index is not degenerate");
    }

    /// Build a well-formed lookup + postings for testing validate_lookup.
    fn make_sorted_index(trigrams: &[u32]) -> (Vec<u8>, Vec<u8>) {
        let mut lookup_buf = Vec::new();
        let mut postings_buf = Vec::new();
        for &tri in trigrams {
            let offset = postings_buf.len() as u64;
            // One posting entry per trigram for simplicity
            let pe = PostingEntry {
                file_id: 0,
                loc_mask: 0xFF,
                next_mask: 0xFF,
            };
            postings_buf.extend_from_slice(&pe.encode());
            let le = LookupEntry {
                trigram: tri,
                offset,
                length: 1,
            };
            lookup_buf.extend_from_slice(&le.encode());
        }
        (lookup_buf, postings_buf)
    }

    #[test]
    fn validate_lookup_accepts_sorted_table() {
        let tmp = TempDir::new().unwrap();
        let (lookup, postings) = make_sorted_index(&[100, 200, 300]);
        let files = ondisk::encode_file_entry(0, "a.rs").unwrap();
        write_index(tmp.path(), &lookup, &postings, &files);
        let reader = IndexReader::open(tmp.path()).unwrap();
        assert!(reader.validate_lookup().is_ok());
    }

    #[test]
    fn validate_lookup_rejects_unsorted_table() {
        let tmp = TempDir::new().unwrap();
        let (lookup, postings) = make_sorted_index(&[200, 100, 300]); // unsorted!
        let files = ondisk::encode_file_entry(0, "a.rs").unwrap();
        write_index(tmp.path(), &lookup, &postings, &files);
        let reader = IndexReader::open(tmp.path()).unwrap();
        let err = reader.validate_lookup().unwrap_err();
        assert!(
            err.contains("not sorted"),
            "expected sort error, got: {err}"
        );
    }

    #[test]
    fn validate_lookup_rejects_out_of_bounds_postings() {
        let tmp = TempDir::new().unwrap();
        // Create lookup that points past the end of postings
        let le = LookupEntry {
            trigram: 100,
            offset: 0,
            length: 999, // way past the single entry we provide
        };
        let mut lookup = Vec::new();
        lookup.extend_from_slice(&le.encode());
        let pe = PostingEntry {
            file_id: 0,
            loc_mask: 0xFF,
            next_mask: 0xFF,
        };
        let mut postings = Vec::new();
        postings.extend_from_slice(&pe.encode());
        let files = ondisk::encode_file_entry(0, "a.rs").unwrap();
        write_index(tmp.path(), &lookup, &postings, &files);
        let reader = IndexReader::open(tmp.path()).unwrap();
        let err = reader.validate_lookup().unwrap_err();
        assert!(err.contains("exceeds"), "expected bounds error, got: {err}");
    }

    #[test]
    fn validate_lookup_empty_is_ok() {
        let tmp = TempDir::new().unwrap();
        write_index(tmp.path(), &[], &[], &[]);
        let reader = IndexReader::open(tmp.path()).unwrap();
        assert!(reader.validate_lookup().is_ok());
    }
}
