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
/// Each entry is a u32 file_id (4 bytes LE).
///
/// ## `files.bin` — file ID → path mapping
/// Variable-length records: `file_id(u32 LE) + path_len(u16 LE) + path_bytes`.
pub(crate) const LOOKUP_ENTRY_SIZE: usize = 16; // 4 + 8 + 4
pub(crate) const POSTING_ENTRY_SIZE: usize = 4;

/// A single entry in `lookup.bin`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LookupEntry {
    pub trigram: u32,
    pub offset: u64,
    pub length: u32,
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

/// Encode a file entry for `files.bin`.
pub(crate) fn encode_file_entry(file_id: u32, path: &str) -> Vec<u8> {
    let path_bytes = path.as_bytes();
    let path_len = path_bytes.len() as u16;
    let mut buf = Vec::with_capacity(4 + 2 + path_bytes.len());
    buf.extend_from_slice(&file_id.to_le_bytes());
    buf.extend_from_slice(&path_len.to_le_bytes());
    buf.extend_from_slice(path_bytes);
    buf
}

/// Decode file entries from `files.bin` data.
pub(crate) fn decode_file_entries(data: &[u8]) -> Vec<(u32, String)> {
    let mut entries = Vec::new();
    let mut pos = 0;
    while pos + 6 <= data.len() {
        let file_id = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        let path_len = u16::from_le_bytes(data[pos + 4..pos + 6].try_into().unwrap()) as usize;
        pos += 6;
        if pos + path_len > data.len() {
            break;
        }
        let path = String::from_utf8_lossy(&data[pos..pos + path_len]).into_owned();
        entries.push((file_id, path));
        pos += path_len;
    }
    entries
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
        let encoded = encode_file_entry(7, "src/main.rs");
        let mut all = Vec::new();
        all.extend_from_slice(&encoded);
        all.extend_from_slice(&encode_file_entry(12, "README.md"));
        let entries = decode_file_entries(&all);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], (7, "src/main.rs".to_string()));
        assert_eq!(entries[1], (12, "README.md".to_string()));
    }
}
