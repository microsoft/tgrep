/// On-disk binary format for the trigram index.
///
/// Three files compose the index:
///
/// ## `lookup.bin` — sorted trigram → postings pointer
/// Fixed-size 16-byte entries sorted by trigram hash for binary search.
/// ```text
/// ┌────────────────┬────────────────┬────────────────┐
/// │ trigram_hash   │ offset         │ length         │
/// │ u32 (4B LE)   │ u64 (8B LE)    │ u32 (4B LE)    │
/// └────────────────┴────────────────┴────────────────┘
/// ```
///
/// ## `index.bin` — concatenated posting lists
/// Each entry is 6 bytes: `file_id(u32) + loc_mask(u8) + next_mask(u8)`.
///
/// ## `files.bin` — file ID → path mapping
/// Variable-length records: `file_id(u32 LE) + path_len(u16 LE) + path_bytes`.
pub(crate) const LOOKUP_ENTRY_SIZE: usize = 16; // 4 + 8 + 4
pub(crate) const POSTING_ENTRY_SIZE: usize = 6; // 4 + 1 + 1

/// A single entry in `lookup.bin`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LookupEntry {
    pub trigram: u32,
    pub offset: u64,
    pub length: u32,
}

/// A single posting entry in `index.bin` (v2 format with masks).
/// 6 bytes on disk: file_id(4) + loc_mask(1) + next_mask(1).
#[derive(Debug, Clone, Copy)]
pub struct PostingEntry {
    pub file_id: u32,
    /// Bit i is set if the trigram occurs at some byte offset where offset % 8 == i.
    pub loc_mask: u8,
    /// 8-bit Bloom filter of characters immediately following this trigram.
    pub next_mask: u8,
}

impl PostingEntry {
    pub fn encode(&self) -> [u8; POSTING_ENTRY_SIZE] {
        let mut buf = [0u8; POSTING_ENTRY_SIZE];
        buf[0..4].copy_from_slice(&self.file_id.to_le_bytes());
        buf[4] = self.loc_mask;
        buf[5] = self.next_mask;
        buf
    }

    pub fn decode(buf: &[u8; POSTING_ENTRY_SIZE]) -> Self {
        Self {
            file_id: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            loc_mask: buf[4],
            next_mask: buf[5],
        }
    }
}

impl LookupEntry {
    pub fn encode(&self) -> [u8; LOOKUP_ENTRY_SIZE] {
        let mut buf = [0u8; LOOKUP_ENTRY_SIZE];
        buf[0..4].copy_from_slice(&self.trigram.to_le_bytes());
        buf[4..12].copy_from_slice(&self.offset.to_le_bytes());
        buf[12..16].copy_from_slice(&self.length.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8; LOOKUP_ENTRY_SIZE]) -> Self {
        Self {
            trigram: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            offset: u64::from_le_bytes(buf[4..12].try_into().unwrap()),
            length: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
        }
    }
}

/// Maximum supported path length (in bytes) for entries in `files.bin`.
///
/// Limited by the `u16` length prefix in the on-disk format.
pub(crate) const MAX_PATH_LEN: usize = u16::MAX as usize;

/// Encode a file entry for `files.bin`.
///
/// Returns `Error::IndexCorrupted` if the path exceeds [`MAX_PATH_LEN`] bytes,
/// since the on-disk format uses a `u16` length prefix and a silent truncation
/// here would corrupt the entire trailing portion of `files.bin` (the decoder
/// reads variable-length records sequentially).
pub(crate) fn encode_file_entry(file_id: u32, path: &str) -> crate::Result<Vec<u8>> {
    let path_bytes = path.as_bytes();
    if path_bytes.len() > MAX_PATH_LEN {
        // Avoid embedding a 64KiB+ path into the error message (memory
        // pressure and log spam, and potentially echoes untrusted content).
        // Include only the length and a short prefix for diagnostics.
        const PREVIEW: usize = 80;
        let preview_end = path
            .char_indices()
            .take_while(|(i, _)| *i < PREVIEW)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        return Err(crate::Error::IndexCorrupted(format!(
            "path too long for index ({} bytes, max {}): \"{}…\"",
            path_bytes.len(),
            MAX_PATH_LEN,
            &path[..preview_end],
        )));
    }
    let path_len = path_bytes.len() as u16;
    let mut buf = Vec::with_capacity(4 + 2 + path_bytes.len());
    buf.extend_from_slice(&file_id.to_le_bytes());
    buf.extend_from_slice(&path_len.to_le_bytes());
    buf.extend_from_slice(path_bytes);
    Ok(buf)
}

/// Decode file entries from `files.bin` data.
///
/// Returns `Error::IndexCorrupted` if `data` is truncated mid-record (i.e.
/// not enough bytes for a declared path or a partial header), so that callers
/// don't silently load a partial file table that would cause queries to drop
/// matches.
pub(crate) fn decode_file_entries(data: &[u8]) -> crate::Result<Vec<(u32, String)>> {
    let mut entries = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        if pos + 6 > data.len() {
            return Err(crate::Error::IndexCorrupted(format!(
                "files.bin truncated: {} trailing bytes < 6-byte header",
                data.len() - pos
            )));
        }
        let file_id = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        let path_len = u16::from_le_bytes(data[pos + 4..pos + 6].try_into().unwrap()) as usize;
        pos += 6;
        if pos + path_len > data.len() {
            return Err(crate::Error::IndexCorrupted(format!(
                "files.bin truncated: declared path_len {} exceeds remaining {} bytes",
                path_len,
                data.len() - pos
            )));
        }
        let path = String::from_utf8_lossy(&data[pos..pos + path_len]).into_owned();
        entries.push((file_id, path));
        pos += path_len;
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lookup_roundtrip() {
        let entry = LookupEntry {
            trigram: 0x746865,
            offset: 1024,
            length: 42,
        };
        let encoded = entry.encode();
        let decoded = LookupEntry::decode(&encoded);
        assert_eq!(decoded.trigram, entry.trigram);
        assert_eq!(decoded.offset, entry.offset);
        assert_eq!(decoded.length, entry.length);
    }

    #[test]
    fn test_file_entry_roundtrip() {
        let encoded = encode_file_entry(7, "src/main.rs").unwrap();
        let mut all = Vec::new();
        all.extend_from_slice(&encoded);
        all.extend_from_slice(&encode_file_entry(12, "README.md").unwrap());
        let entries = decode_file_entries(&all).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], (7, "src/main.rs".to_string()));
        assert_eq!(entries[1], (12, "README.md".to_string()));
    }

    #[test]
    fn test_encode_file_entry_rejects_oversized_path() {
        // A path longer than u16::MAX bytes must error rather than silently
        // truncate the on-disk `path_len` field.
        let huge = "a".repeat(MAX_PATH_LEN + 1);
        let err = encode_file_entry(0, &huge).unwrap_err();
        match err {
            crate::Error::IndexCorrupted(_) => {}
            other => panic!("expected IndexCorrupted, got {other:?}"),
        }
    }

    #[test]
    fn test_encode_file_entry_accepts_max_path() {
        let max = "a".repeat(MAX_PATH_LEN);
        let buf = encode_file_entry(0, &max).expect("max-length path should encode");
        let entries = decode_file_entries(&buf).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, 0);
        assert_eq!(entries[0].1.len(), MAX_PATH_LEN);
    }

    #[test]
    fn test_decode_file_entries_rejects_truncated_header() {
        // Two well-formed entries followed by 3 stray bytes (incomplete header).
        let mut all = encode_file_entry(1, "a.rs").unwrap();
        all.extend_from_slice(&encode_file_entry(2, "b.rs").unwrap());
        all.extend_from_slice(&[0u8, 0, 0]);
        let err = decode_file_entries(&all).unwrap_err();
        assert!(matches!(err, crate::Error::IndexCorrupted(_)));
    }

    #[test]
    fn test_decode_file_entries_rejects_truncated_path() {
        // Header claims 100-byte path but only 5 bytes follow.
        let mut buf = Vec::new();
        buf.extend_from_slice(&7u32.to_le_bytes());
        buf.extend_from_slice(&100u16.to_le_bytes());
        buf.extend_from_slice(b"hello");
        let err = decode_file_entries(&buf).unwrap_err();
        assert!(matches!(err, crate::Error::IndexCorrupted(_)));
    }

    #[test]
    fn test_decode_file_entries_empty_ok() {
        let entries = decode_file_entries(&[]).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_posting_entry_roundtrip() {
        let entry = PostingEntry {
            file_id: 42,
            loc_mask: 0b10101010,
            next_mask: 0b11001100,
        };
        let encoded = entry.encode();
        let decoded = PostingEntry::decode(&encoded);
        assert_eq!(decoded.file_id, entry.file_id);
        assert_eq!(decoded.loc_mask, entry.loc_mask);
        assert_eq!(decoded.next_mask, entry.next_mask);
    }
}
