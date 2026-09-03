use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use crate::Result;

const META_FILENAME: &str = "meta.json";
const FILESTAMPS_FILENAME: &str = "filestamps.json";
const CONTENT_ID_DOMAIN: &[u8] =
    b"tgrep/content-id/v1\0decode_for_index-output\0binary-and-posting-semantics-v2";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMeta {
    pub version: u32,
    pub num_files: u64,
    pub num_trigrams: u64,
    pub created_at: u64,
    pub updated_at: u64,
    pub root_path: String,
    /// Whether the index covers the full repo. `false` means the server was
    /// stopped during background indexing and the index is partial.
    #[serde(default = "default_complete")]
    pub complete: bool,
}

fn default_complete() -> bool {
    true
}

impl IndexMeta {
    pub fn new(root_path: &str, num_files: u64, num_trigrams: u64) -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            version: 2,
            num_files,
            num_trigrams,
            created_at: now,
            updated_at: now,
            root_path: root_path.to_string(),
            complete: true,
        }
    }

    pub fn save(&self, index_dir: &Path) -> Result<()> {
        let path = index_dir.join(META_FILENAME);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load(index_dir: &Path) -> Result<Self> {
        let path = index_dir.join(META_FILENAME);
        if !path.exists() {
            return Err(crate::Error::IndexNotFound(index_dir.display().to_string()));
        }
        let data = std::fs::read_to_string(path)?;
        let meta: Self = serde_json::from_str(&data)?;
        Ok(meta)
    }
}

/// Per-file stamp for change detection (mtime + size).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileStamp {
    pub mtime: u64,
    pub size: u64,
}

/// Identity of the decoded bytes used to build one path's postings.
///
/// The domain prefix must change if decoding, binary classification, or posting
/// semantics change in a way that makes identities from an older index unsafe
/// to compare with newly decoded bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentId([u8; 16]);

impl ContentId {
    pub fn from_indexed_bytes(bytes: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(CONTENT_ID_DOMAIN);
        hasher.update(bytes);
        let mut id = [0; 16];
        id.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
        Self(id)
    }

    fn from_hex(value: &str) -> Option<Self> {
        if value.len() != 32 {
            return None;
        }
        let mut id = [0; 16];
        let (digits, remainder) = value.as_bytes().as_chunks::<2>();
        debug_assert!(remainder.is_empty());
        for (byte, digits) in id.iter_mut().zip(digits) {
            let high = hex_digit(digits[0])?;
            let low = hex_digit(digits[1])?;
            *byte = (high << 4) | low;
        }
        Some(Self(id))
    }

    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(32);
        for byte in self.0 {
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0xf) as usize] as char);
        }
        encoded
    }
}

fn hex_digit(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

/// Per-path metadata and optional trusted identity from one index generation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileEvidence {
    pub stamps: HashMap<String, FileStamp>,
    pub content_ids: HashMap<String, ContentId>,
}

impl FileEvidence {
    pub fn from_stamps(stamps: HashMap<String, FileStamp>) -> Self {
        Self {
            stamps,
            content_ids: HashMap::new(),
        }
    }

    pub fn stamp(&self, path: &str) -> Option<&FileStamp> {
        self.stamps.get(path)
    }

    pub fn content_id(&self, path: &str) -> Option<ContentId> {
        self.content_ids.get(path).copied()
    }

    pub fn insert(&mut self, path: String, stamp: FileStamp, content_id: Option<ContentId>) {
        if let Some(content_id) = content_id {
            self.content_ids.insert(path.clone(), content_id);
        } else {
            self.content_ids.remove(&path);
        }
        self.stamps.insert(path, stamp);
    }

    pub fn remove(&mut self, path: &str) -> Option<FileStamp> {
        self.content_ids.remove(path);
        self.stamps.remove(path)
    }

    pub fn clear(&mut self) {
        self.stamps.clear();
        self.content_ids.clear();
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&str, &FileStamp) -> bool) {
        self.stamps.retain(|path, stamp| keep(path, stamp));
        self.content_ids
            .retain(|path, _| self.stamps.contains_key(path));
    }
}

/// Convert filesystem metadata into the persisted stamp used by change
/// detection.
pub fn file_stamp(metadata: &std::fs::Metadata) -> FileStamp {
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    FileStamp {
        mtime,
        size: metadata.len(),
    }
}

/// Write per-file stamps to `filestamps.json` in the index directory.
pub fn write_filestamps(stamps: &HashMap<String, FileStamp>, index_dir: &Path) -> Result<()> {
    let path = index_dir.join(FILESTAMPS_FILENAME);
    let json = serde_json::to_string(stamps)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Read per-file stamps from `filestamps.json` in the index directory.
pub fn read_filestamps(index_dir: &Path) -> Result<HashMap<String, FileStamp>> {
    let path = index_dir.join(FILESTAMPS_FILENAME);
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let json = std::fs::read_to_string(&path)?;
    let stamps: HashMap<String, FileStamp> = serde_json::from_str(&json)?;
    Ok(stamps)
}

#[derive(Deserialize)]
struct EvidenceEntry {
    mtime: u64,
    size: u64,
    #[serde(default)]
    c: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct EvidenceEntryRef<'a> {
    mtime: u64,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    c: Option<&'a str>,
}

/// Write stamps and optional content identities in the compatible filestamp
/// JSON shape. Older readers ignore the compact `c` field.
pub fn write_file_evidence(evidence: &FileEvidence, index_dir: &Path) -> Result<()> {
    let path = index_dir.join(FILESTAMPS_FILENAME);
    let encoded_ids: HashMap<&str, String> = evidence
        .content_ids
        .iter()
        .filter(|(path, _)| evidence.stamps.contains_key(path.as_str()))
        .map(|(path, id)| (path.as_str(), id.to_hex()))
        .collect();
    let entries: HashMap<&str, EvidenceEntryRef<'_>> = evidence
        .stamps
        .iter()
        .map(|(path, stamp)| {
            (
                path.as_str(),
                EvidenceEntryRef {
                    mtime: stamp.mtime,
                    size: stamp.size,
                    c: encoded_ids.get(path.as_str()).map(String::as_str),
                },
            )
        })
        .collect();
    std::fs::write(path, serde_json::to_string(&entries)?)?;
    Ok(())
}

/// Read stamps and any valid per-path identities. A malformed identity discards
/// only that identity; the path's metadata remains available for stale checks.
pub fn read_file_evidence(index_dir: &Path) -> Result<FileEvidence> {
    let path = index_dir.join(FILESTAMPS_FILENAME);
    if !path.exists() {
        return Ok(FileEvidence::default());
    }
    let json = std::fs::read_to_string(path)?;
    let entries: HashMap<String, EvidenceEntry> = serde_json::from_str(&json)?;
    let mut evidence = FileEvidence::default();
    for (path, entry) in entries {
        let content_id = entry
            .c
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .and_then(ContentId::from_hex);
        evidence.insert(
            path,
            FileStamp {
                mtime: entry.mtime,
                size: entry.size,
            },
            content_id,
        );
    }
    Ok(evidence)
}

/// Remove all persisted stamp and identity evidence for the active generation.
pub fn remove_file_evidence(index_dir: &Path) -> Result<()> {
    match std::fs::remove_file(index_dir.join(FILESTAMPS_FILENAME)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Collect file stamps (mtime + size) for a list of relative paths under `root`.
pub fn collect_filestamps(root: &Path, paths: &[String]) -> HashMap<String, FileStamp> {
    use rayon::prelude::*;

    paths
        .par_iter()
        .filter_map(|rel_path| {
            let full_path = root.join(rel_path);
            std::fs::metadata(&full_path)
                .ok()
                .map(|metadata| (rel_path.clone(), file_stamp(&metadata)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_evidence_is_legacy_compatible_and_roundtrips_ids() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILESTAMPS_FILENAME),
            r#"{"legacy.rs":{"mtime":1,"size":2}}"#,
        )
        .unwrap();
        let legacy = read_file_evidence(dir.path()).unwrap();
        assert_eq!(
            legacy.stamp("legacy.rs"),
            Some(&FileStamp { mtime: 1, size: 2 })
        );
        assert_eq!(legacy.content_id("legacy.rs"), None);

        let id = ContentId::from_indexed_bytes(b"decoded text");
        let mut evidence = FileEvidence::default();
        evidence.insert(
            "indexed.rs".to_string(),
            FileStamp { mtime: 3, size: 4 },
            Some(id),
        );
        write_file_evidence(&evidence, dir.path()).unwrap();

        assert_eq!(read_file_evidence(dir.path()).unwrap(), evidence);
        assert_eq!(
            read_filestamps(dir.path()).unwrap().get("indexed.rs"),
            Some(&FileStamp { mtime: 3, size: 4 })
        );
        let json = std::fs::read_to_string(dir.path().join(FILESTAMPS_FILENAME)).unwrap();
        assert!(json.contains(&format!(r#""c":"{}""#, id.to_hex())));
    }

    #[test]
    fn malformed_content_id_drops_only_that_paths_identity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILESTAMPS_FILENAME),
            r#"{
                "short.rs":{"mtime":1,"size":2,"c":"abcd"},
                "uppercase.rs":{"mtime":3,"size":4,"c":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"},
                "wrong-type.rs":{"mtime":5,"size":6,"c":7}
            }"#,
        )
        .unwrap();

        let evidence = read_file_evidence(dir.path()).unwrap();
        assert_eq!(evidence.stamps.len(), 3);
        assert!(evidence.content_ids.is_empty());
        assert_eq!(
            evidence.stamp("wrong-type.rs"),
            Some(&FileStamp { mtime: 5, size: 6 })
        );
    }
}
