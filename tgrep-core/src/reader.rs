/// Read-only index reader.
///
/// Memory-maps `lookup.bin` for zero-copy binary search on the sorted
/// trigram table. Reads posting lists from `index.bin` on demand to
/// keep resident memory bounded — only the lookup table stays mapped.
use memmap2::Mmap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::Result;
use crate::ondisk::{self, LOOKUP_ENTRY_SIZE, LookupEntry, POSTING_ENTRY_SIZE, PostingEntry};

pub struct IndexReader {
    lookup: Option<Mmap>,
    /// File handle for on-demand posting list reads (not mmap'd).
    postings_file: Option<Mutex<File>>,
    /// Path to index.bin for bulk reads (all_trigram_postings).
    postings_path: Option<PathBuf>,
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
        let (lookup, pf, pp, num_entries) = if lookup_meta.len() == 0 || postings_meta.len() == 0 {
            (None, None, None, 0)
        } else {
            let lookup_file = File::open(&lookup_path)?;
            let pf = File::open(&postings_path)?;
            // SAFETY: File is opened read-only and the Mmap lifetime is tied to
            // IndexReader. The close() method drops the mapping before any file
            // overwrites (required on Windows).
            let lk = unsafe { Mmap::map(&lookup_file)? };
            let n = lk.len() / LOOKUP_ENTRY_SIZE;
            (Some(lk), Some(Mutex::new(pf)), Some(postings_path), n)
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
            postings_file: pf,
            postings_path: pp,
            file_paths,
            num_entries,
        })
    }

    /// Release mmap and file handles so the underlying files can be overwritten (Windows).
    pub fn close(&mut self) {
        self.lookup = None;
        self.postings_file = None;
        self.postings_path = None;
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

    /// Get all file IDs present in this reader.
    pub fn all_file_ids(&self) -> Vec<u32> {
        (0..self.file_paths.len() as u32).collect()
    }

    /// Get all file paths in this reader (for skip-set construction).
    pub fn all_paths(&self) -> &[String] {
        &self.file_paths
    }

    /// Read all trigram entries and their posting lists.
    /// Used by `full_snapshot()` during flush — bulk-reads the entire postings
    /// file into memory for efficient sequential decode, then drops it.
    pub fn all_trigram_postings(&self) -> Vec<(u32, Vec<u32>)> {
        if self.num_entries == 0 {
            return Vec::new();
        }

        // Bulk-read the entire postings file — temporary memory, dropped after decode
        let postings_data = match &self.postings_path {
            Some(path) => match std::fs::read(path) {
                Ok(data) => data,
                Err(_) => return Vec::new(),
            },
            None => return Vec::new(),
        };

        let mut result = Vec::with_capacity(self.num_entries);
        for i in 0..self.num_entries {
            let entry = self.read_lookup_entry(i);
            let start = entry.offset as usize;
            let mut file_ids = Vec::with_capacity(entry.length as usize);
            for j in 0..entry.length as usize {
                let pos = start + j * POSTING_ENTRY_SIZE;
                if pos + POSTING_ENTRY_SIZE <= postings_data.len() {
                    let buf: &[u8; POSTING_ENTRY_SIZE] = postings_data
                        [pos..pos + POSTING_ENTRY_SIZE]
                        .try_into()
                        .unwrap();
                    file_ids.push(PostingEntry::decode(buf).file_id);
                }
            }
            result.push((entry.trigram, file_ids));
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
        let file_mutex = match self.postings_file.as_ref() {
            Some(f) => f,
            None => return Vec::new(),
        };
        let byte_len = length as usize * POSTING_ENTRY_SIZE;
        let mut buf = vec![0u8; byte_len];

        let mut file = file_mutex.lock().unwrap();
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return Vec::new();
        }
        if file.read_exact(&mut buf).is_err() {
            return Vec::new();
        }

        let mut result = Vec::with_capacity(length as usize);
        for i in 0..length as usize {
            let pos = i * POSTING_ENTRY_SIZE;
            let entry_buf: &[u8; POSTING_ENTRY_SIZE] =
                buf[pos..pos + POSTING_ENTRY_SIZE].try_into().unwrap();
            result.push(PostingEntry::decode(entry_buf));
        }
        result
    }
}
