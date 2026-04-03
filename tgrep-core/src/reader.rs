/// Mmap-based read-only index reader.
///
/// Uses memory-mapped files for zero-copy access to `lookup.bin` and
/// `index.bin`. Binary searches the sorted lookup table to find
/// posting lists for queried trigrams.
use memmap2::Mmap;
use std::fs::File;
use std::path::Path;

use crate::Result;
use crate::ondisk::{self, LOOKUP_ENTRY_SIZE, LookupEntry, POSTING_ENTRY_SIZE};

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

        let lookup_meta = std::fs::metadata(&lookup_path)?;
        let postings_meta = std::fs::metadata(&postings_path)?;

        // Handle empty index (no files indexed yet) — mmap requires non-zero length
        let (lookup, postings, num_entries) = if lookup_meta.len() == 0 || postings_meta.len() == 0
        {
            (None, None, 0)
        } else {
            let lookup_file = File::open(&lookup_path)?;
            let postings_file = File::open(&postings_path)?;
            // SAFETY: Files are opened read-only and the Mmap lifetime is tied to
            // IndexReader. The close() method drops the mappings before any file
            // overwrites (required on Windows).
            let lk = unsafe { Mmap::map(&lookup_file)? };
            let pk = unsafe { Mmap::map(&postings_file)? };
            let n = lk.len() / LOOKUP_ENTRY_SIZE;
            (Some(lk), Some(pk), n)
        };

        // Load file paths
        let files_data = std::fs::read(&files_path)?;
        let file_entries = ondisk::decode_file_entries(&files_data);
        let mut file_paths = vec![String::new(); file_entries.len()];
        for (id, path) in file_entries {
            if (id as usize) < file_paths.len() {
                file_paths[id as usize] = path;
            }
        }

        Ok(Self {
            lookup,
            postings,
            file_paths,
            num_entries,
        })
    }

    /// Release mmap handles so the underlying files can be overwritten (Windows).
    pub fn close(&mut self) {
        self.lookup = None;
        self.postings = None;
        self.file_paths.clear();
        self.num_entries = 0;
    }

    /// Binary search the lookup table for a trigram hash.
    /// Returns the posting list (file IDs) or an empty vec if not found.
    pub fn lookup_trigram(&self, trigram: u32) -> Vec<u32> {
        if self.lookup.is_none() {
            return Vec::new();
        }
        let idx = self.binary_search(trigram);
        match idx {
            Some(i) => {
                let entry = self.read_lookup_entry(i);
                self.read_posting_list(entry.offset, entry.length)
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
            let postings = self.read_posting_list(entry.offset, entry.length);
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

    fn read_posting_list(&self, offset: u64, length: u32) -> Vec<u32> {
        let postings = match self.postings.as_ref() {
            Some(p) => p,
            None => return Vec::new(),
        };
        let mut result = Vec::with_capacity(length as usize);
        let start = offset as usize;
        for i in 0..length as usize {
            let pos = start + i * POSTING_ENTRY_SIZE;
            if pos + POSTING_ENTRY_SIZE <= postings.len() {
                let fid =
                    u32::from_le_bytes(postings[pos..pos + POSTING_ENTRY_SIZE].try_into().unwrap());
                result.push(fid);
            }
        }
        result
    }
}
