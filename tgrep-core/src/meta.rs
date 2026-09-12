use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use crate::Result;

const META_FILENAME: &str = "meta.json";
const FILESTAMPS_FILENAME: &str = "filestamps.json";
pub const INDEX_FORMAT_VERSION: u32 = crate::ondisk::INDEX_FORMAT_VERSION;
pub type FileTableId = [u8; 32];
const CONTENT_ID_DOMAIN: &[u8] =
    b"tgrep/content-id/v1\0decode_for_index-output\0binary-and-posting-semantics-v2";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMeta<V = crate::visibility::PathVisibility> {
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
    /// Proven coverage of otherwise eligible hidden files. Missing on legacy
    /// indexes; neither `complete` alone nor a partial build proves coverage.
    #[serde(default)]
    pub hidden_complete: bool,
    #[serde(default)]
    pub visibility: V,
    /// Binds visibility to the exact path table across multi-file publication.
    #[serde(default)]
    pub file_table_id: Option<FileTableId>,
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
            version: INDEX_FORMAT_VERSION,
            num_files,
            num_trigrams,
            created_at: now,
            updated_at: now,
            root_path: root_path.to_string(),
            complete: true,
            hidden_complete: false,
            visibility: Default::default(),
            file_table_id: None,
        }
    }

    pub fn save(&self, index_dir: &Path) -> Result<()> {
        let path = index_dir.join(META_FILENAME);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load(index_dir: &Path) -> Result<Self> {
        load_metadata(index_dir)
    }

    /// Locate the indexed root without materializing the visibility map. This
    /// validates the same metadata shape as `load`, but does not establish
    /// coverage or file-table identity for an indexed query.
    pub fn load_root(index_dir: &Path) -> Result<String> {
        let meta: IndexMeta<SkippedVisibility> = load_metadata(index_dir)?;
        Ok(meta.root_path)
    }
}

fn load_metadata<T: serde::de::DeserializeOwned>(index_dir: &Path) -> Result<T> {
    let path = index_dir.join(META_FILENAME);
    if !path.exists() {
        return Err(crate::Error::IndexNotFound(index_dir.display().to_string()));
    }
    let data = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&data)?)
}

#[derive(Default, Deserialize)]
struct SkippedVisibility {
    #[serde(rename = "hidden")]
    _hidden: SkippedHiddenEntries,
}

#[derive(Default)]
struct SkippedHiddenEntries;

impl<'de> Deserialize<'de> for SkippedHiddenEntries {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct HiddenVisitor;
        impl<'de> serde::de::Visitor<'de> for HiddenVisitor {
            type Value = SkippedHiddenEntries;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a map")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                while map.next_entry::<SkippedPath, bool>()?.is_some() {}
                Ok(SkippedHiddenEntries)
            }
        }
        deserializer.deserialize_map(HiddenVisitor)
    }
}

struct SkippedPath;

impl<'de> Deserialize<'de> for SkippedPath {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct PathVisitor;
        impl serde::de::Visitor<'_> for PathVisitor {
            type Value = SkippedPath;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_str<E: serde::de::Error>(
                self,
                _: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(SkippedPath)
            }
        }
        // Unlike IgnoredAny, this still rejects malformed Unicode escapes.
        // Unescaped keys are borrowed; escaped keys reuse serde_json's scratch.
        deserializer.deserialize_str(PathVisitor)
    }
}

pub fn file_table_id(data: &[u8]) -> FileTableId {
    *blake3::hash(data).as_bytes()
}

pub fn read_file_table_id(index_dir: &Path) -> Result<FileTableId> {
    use std::io::Read;

    let mut file = std::fs::File::open(index_dir.join("files.bin"))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(*hasher.finalize().as_bytes());
        }
        hasher.update(&buffer[..read]);
    }
}

/// Per-file stamp for change detection (mtime + size).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileStamp {
    pub mtime: u64,
    pub size: u64,
}

/// Full-resolution metadata identifying the file version used by an indexed
/// read. A scan may compare this evidence, but must not create read evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVersion {
    stamp: FileStamp,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change_seconds: i64,
    #[cfg(unix)]
    change_nanos: i64,
}

impl FileVersion {
    pub fn stamp(&self) -> &FileStamp {
        &self.stamp
    }

    /// Whether this platform supplied enough metadata for precise comparisons.
    pub fn is_trusted(&self) -> bool {
        if self.modified.is_none() {
            return false;
        }
        #[cfg(unix)]
        {
            (0..1_000_000_000).contains(&self.change_nanos)
        }
        #[cfg(windows)]
        {
            self.created.is_some()
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }

    fn persisted(&self) -> Option<PersistedVersion> {
        if !self.is_trusted() {
            return None;
        }
        Some(PersistedVersion {
            schema: 1,
            platform: std::env::consts::OS.to_string(),
            size: self.stamp.size,
            modified: VersionTime::from(self.modified?),
            created: self.created.map(VersionTime::from),
            #[cfg(unix)]
            unix: Some(UnixVersion {
                device: self.device,
                inode: self.inode,
                change_seconds: self.change_seconds,
                change_nanos: self.change_nanos,
            }),
            #[cfg(not(unix))]
            unix: None,
        })
    }
}

/// Derive precise metadata from an existing stat, without another filesystem
/// query. Trust as indexed evidence only after validating the associated read.
pub fn file_version(metadata: &std::fs::Metadata) -> FileVersion {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    FileVersion {
        stamp: file_stamp(metadata),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        change_seconds: metadata.ctime(),
        #[cfg(unix)]
        change_nanos: metadata.ctime_nsec(),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionTime {
    before_epoch: bool,
    seconds: u64,
    nanos: u32,
}

impl From<SystemTime> for VersionTime {
    fn from(time: SystemTime) -> Self {
        let (before_epoch, duration) = match time.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(duration) => (false, duration),
            Err(error) => (true, error.duration()),
        };
        Self {
            before_epoch,
            seconds: duration.as_secs(),
            nanos: duration.subsec_nanos(),
        }
    }
}

impl VersionTime {
    fn decode(self) -> Option<SystemTime> {
        if self.nanos >= 1_000_000_000 {
            return None;
        }
        let duration = std::time::Duration::new(self.seconds, self.nanos);
        if self.before_epoch {
            SystemTime::UNIX_EPOCH.checked_sub(duration)
        } else {
            SystemTime::UNIX_EPOCH.checked_add(duration)
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnixVersion {
    device: u64,
    inode: u64,
    change_seconds: i64,
    change_nanos: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedVersion {
    schema: u32,
    platform: String,
    size: u64,
    modified: VersionTime,
    // An explicit null represents unavailable birth time; omission is not
    // complete evidence for this schema.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    created: Option<VersionTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unix: Option<UnixVersion>,
}

impl PersistedVersion {
    fn decode(self, stamp: FileStamp) -> Option<FileVersion> {
        if self.schema != 1 || self.platform != std::env::consts::OS || self.size != stamp.size {
            return None;
        }
        let modified = self.modified.decode()?;
        if modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs())
            != stamp.mtime
        {
            return None;
        }
        let created = match self.created {
            Some(time) => Some(time.decode()?),
            None => None,
        };
        #[cfg(unix)]
        let unix = self.unix?;
        #[cfg(not(unix))]
        if self.unix.is_some() {
            return None;
        }
        let version = FileVersion {
            stamp,
            modified: Some(modified),
            created,
            #[cfg(unix)]
            device: unix.device,
            #[cfg(unix)]
            inode: unix.inode,
            #[cfg(unix)]
            change_seconds: unix.change_seconds,
            #[cfg(unix)]
            change_nanos: unix.change_nanos,
        };
        version.is_trusted().then_some(version)
    }
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

/// Per-path metadata and optional trusted content/version evidence from one
/// index generation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileEvidence {
    pub stamps: HashMap<String, FileStamp>,
    pub content_ids: HashMap<String, ContentId>,
    pub versions: HashMap<String, FileVersion>,
}

impl FileEvidence {
    pub fn from_stamps(stamps: HashMap<String, FileStamp>) -> Self {
        Self {
            stamps,
            content_ids: HashMap::new(),
            versions: HashMap::new(),
        }
    }

    pub fn stamp(&self, path: &str) -> Option<&FileStamp> {
        self.stamps.get(path)
    }

    pub fn content_id(&self, path: &str) -> Option<ContentId> {
        self.content_ids.get(path).copied()
    }

    pub fn version(&self, path: &str) -> Option<&FileVersion> {
        self.versions
            .get(path)
            .filter(|version| version.is_trusted() && self.stamp(path) == Some(version.stamp()))
    }

    pub fn insert(&mut self, path: String, stamp: FileStamp, content_id: Option<ContentId>) {
        self.insert_verified(path, stamp, content_id, None);
    }

    /// Insert a version validated around the read that produced this path's
    /// postings (or binary classification), not metadata collected afterwards.
    /// Omitted or incompatible evidence clears the previous version.
    pub fn insert_verified(
        &mut self,
        path: String,
        stamp: FileStamp,
        content_id: Option<ContentId>,
        version: Option<FileVersion>,
    ) {
        if let Some(version) =
            version.filter(|version| version.is_trusted() && version.stamp() == &stamp)
        {
            self.versions.insert(path.clone(), version);
        } else {
            self.versions.remove(&path);
        }
        if let Some(content_id) = content_id {
            self.content_ids.insert(path.clone(), content_id);
        } else {
            self.content_ids.remove(&path);
        }
        self.stamps.insert(path, stamp);
    }

    pub fn remove(&mut self, path: &str) -> Option<FileStamp> {
        self.versions.remove(path);
        self.content_ids.remove(path);
        self.stamps.remove(path)
    }

    pub fn clear(&mut self) {
        self.stamps.clear();
        self.content_ids.clear();
        self.versions.clear();
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&str, &FileStamp) -> bool) {
        self.stamps.retain(|path, stamp| keep(path, stamp));
        self.content_ids
            .retain(|path, _| self.stamps.contains_key(path));
        self.versions
            .retain(|path, version| self.stamps.get(path) == Some(version.stamp()));
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
    #[serde(default)]
    v: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct EvidenceEntryRef<'a> {
    mtime: u64,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    c: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    v: Option<PersistedVersion>,
}

/// Write stamps and optional content/version evidence in the compatible
/// filestamp JSON shape. Older readers ignore the compact `c` and `v` fields.
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
                    v: evidence.version(path).and_then(FileVersion::persisted),
                },
            )
        })
        .collect();
    std::fs::write(path, serde_json::to_string(&entries)?)?;
    Ok(())
}

/// Read stamps and valid per-path evidence. Malformed, unsupported, or
/// platform-incompatible evidence is discarded, preserving the legacy stamp.
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
        let stamp = FileStamp {
            mtime: entry.mtime,
            size: entry.size,
        };
        let version = entry
            .v
            .and_then(|value| serde_json::from_value::<PersistedVersion>(value).ok())
            .and_then(|version| version.decode(stamp.clone()));
        evidence.insert_verified(path, stamp, content_id, version);
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
    #[test]
    fn root_projection_preserves_metadata_validation() {
        let dir = tempfile::tempdir().unwrap();
        let mut original = serde_json::to_value(super::IndexMeta::new("root", 1, 2)).unwrap();
        original["visibility"] = serde_json::json!({
            "hidden": {".github": true, "attribute-hidden.txt": false, "escaped\\name": true}
        });
        let assert_projection = |value: &serde_json::Value| {
            std::fs::write(
                dir.path().join(super::META_FILENAME),
                serde_json::to_vec(value).unwrap(),
            )
            .unwrap();
            let full = super::IndexMeta::load(dir.path());
            let root = super::IndexMeta::load_root(dir.path());
            assert_eq!(full.is_ok(), root.is_ok(), "{value}");
            if let Ok(full) = full {
                assert_eq!(root.unwrap(), full.root_path);
            }
        };
        assert_projection(&original);
        for key in original.as_object().unwrap().keys() {
            let mut missing = original.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert_projection(&missing);
            for invalid in [serde_json::Value::Null, serde_json::json!({"wrong": []})] {
                let mut changed = original.clone();
                changed[key] = invalid;
                assert_projection(&changed);
            }
        }
        for visibility in [
            serde_json::json!({}),
            serde_json::json!({"hidden": []}),
            serde_json::json!({"hidden": {".secret": 1}}),
            serde_json::json!({"hidden": {".secret": null}}),
            serde_json::json!({"hidden": {}, "unknown": [null]}),
        ] {
            let mut changed = original.clone();
            changed["visibility"] = visibility;
            assert_projection(&changed);
        }
        for malformed in [
            "{".to_string(),
            r#"{"root_path":"root","visibility":{"hidden":{"#.to_string(),
            "{} false".to_string(),
            serde_json::to_string(&original)
                .unwrap()
                .replace(r#""escaped\\name""#, r#""\uD800""#),
            serde_json::to_string(&original)
                .unwrap()
                .replace(r#""escaped\\name""#, r#""\x01""#),
        ] {
            std::fs::write(dir.path().join(super::META_FILENAME), malformed).unwrap();
            assert!(super::IndexMeta::load(dir.path()).is_err());
            assert!(super::IndexMeta::load_root(dir.path()).is_err());
        }
        std::fs::remove_file(dir.path().join(super::META_FILENAME)).unwrap();
        assert!(matches!(
            super::IndexMeta::load_root(dir.path()),
            Err(crate::Error::IndexNotFound(_))
        ));
    }

    #[test]
    fn legacy_metadata_does_not_prove_hidden_coverage() {
        let mut value = serde_json::to_value(super::IndexMeta::new("root", 1, 2)).unwrap();
        value.as_object_mut().unwrap().remove("hidden_complete");
        value.as_object_mut().unwrap().remove("visibility");
        let meta: super::IndexMeta = serde_json::from_value(value).unwrap();
        assert!(meta.complete);
        assert!(!meta.hidden_complete);
    }

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
        assert!(legacy.versions.is_empty());

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

    fn test_version() -> FileVersion {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("indexed.rs");
        std::fs::write(&path, b"indexed text").unwrap();
        file_version(&std::fs::metadata(path).unwrap())
    }

    #[test]
    fn file_version_distinguishes_subsecond_modified_times() {
        let mut first = test_version();
        let base = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        first.stamp.mtime = 1;
        first.modified = Some(base + std::time::Duration::from_nanos(100));
        let second = FileVersion {
            modified: Some(base + std::time::Duration::from_nanos(200)),
            ..first.clone()
        };

        assert_ne!(first, second);
        assert_eq!(first.stamp, second.stamp);
    }

    #[test]
    fn precise_versions_roundtrip_without_changing_legacy_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let version = test_version();
        let mut evidence = FileEvidence::default();
        evidence.insert_verified(
            "indexed.rs".into(),
            version.stamp().clone(),
            Some(ContentId::from_indexed_bytes(b"indexed text")),
            Some(version),
        );
        write_file_evidence(&evidence, dir.path()).unwrap();
        assert_eq!(read_file_evidence(dir.path()).unwrap(), evidence);
        assert_eq!(read_filestamps(dir.path()).unwrap(), evidence.stamps);

        write_filestamps(&evidence.stamps, dir.path()).unwrap();
        let legacy = read_file_evidence(dir.path()).unwrap();
        assert_eq!(legacy.stamps, evidence.stamps);
        assert!(legacy.versions.is_empty());
    }

    #[test]
    fn malformed_or_incompatible_versions_fail_open_per_path() {
        let dir = tempfile::tempdir().unwrap();
        let version = test_version();
        let id = ContentId::from_indexed_bytes(b"indexed text");
        let valid = serde_json::to_value(version.persisted().unwrap()).unwrap();
        let mut bad_platform = valid.clone();
        bad_platform["platform"] = "unsupported-platform".into();
        let mut future_schema = valid.clone();
        future_schema["schema"] = 2.into();
        let mut bad_nanos = valid.clone();
        bad_nanos["modified"]["nanos"] = 1_000_000_000u64.into();
        let mut overflow_time = valid.clone();
        overflow_time["modified"]["seconds"] = u64::MAX.into();
        let mut missing_modified = valid.clone();
        missing_modified.as_object_mut().unwrap().remove("modified");
        let mut missing_created = valid.clone();
        missing_created.as_object_mut().unwrap().remove("created");
        let mut bad_created = valid.clone();
        bad_created["created"] = serde_json::json!({
            "before_epoch": false, "seconds": 0, "nanos": 1_000_000_000
        });
        let mut mismatched_stamp = valid.clone();
        mismatched_stamp["modified"]["seconds"] = (version.stamp().mtime + 1).into();
        let mut mismatched_size = valid.clone();
        mismatched_size["size"] = (version.stamp().size + 1).into();
        let mut bad_unix = valid.clone();
        #[cfg(unix)]
        {
            bad_unix["unix"] = serde_json::Value::Null;
        }
        #[cfg(not(unix))]
        {
            bad_unix["unix"] = serde_json::json!({
                "device": 1, "inode": 2, "change_seconds": 3, "change_nanos": 4
            });
        }
        let invalid = [
            serde_json::Value::Null,
            serde_json::json!("not a version"),
            serde_json::json!({}),
            bad_platform,
            future_schema,
            bad_nanos,
            overflow_time,
            missing_modified,
            missing_created,
            bad_created,
            mismatched_stamp,
            mismatched_size,
            bad_unix,
        ];
        let mut entries = serde_json::Map::new();
        for (i, value) in invalid.into_iter().enumerate() {
            entries.insert(
                format!("{i}.rs"),
                serde_json::json!({
                    "mtime": version.stamp().mtime,
                    "size": version.stamp().size,
                    "c": id.to_hex(),
                    "v": value,
                }),
            );
        }
        entries.insert(
            "valid.rs".into(),
            serde_json::json!({
                "mtime": version.stamp().mtime,
                "size": version.stamp().size,
                "c": id.to_hex(),
                "v": valid,
            }),
        );
        std::fs::write(
            dir.path().join(FILESTAMPS_FILENAME),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        let evidence = read_file_evidence(dir.path()).unwrap();
        assert_eq!(evidence.stamps.len(), entries.len());
        assert_eq!(evidence.content_ids.len(), entries.len());
        assert_eq!(evidence.versions.len(), 1);
        assert_eq!(evidence.version("valid.rs"), Some(&version));
    }

    #[test]
    fn legacy_insert_and_evidence_lifecycle_clear_versions() {
        let version = test_version();
        let mut evidence = FileEvidence::default();
        for path in ["legacy.rs", "remove.rs", "retain.rs", "clear.rs"] {
            evidence.insert_verified(
                path.into(),
                version.stamp().clone(),
                None,
                Some(version.clone()),
            );
        }
        evidence.insert("legacy.rs".into(), version.stamp().clone(), None);
        assert!(evidence.version("legacy.rs").is_none());
        assert!(!evidence.versions.contains_key("legacy.rs"));
        evidence.remove("remove.rs");
        assert!(!evidence.versions.contains_key("remove.rs"));
        evidence.retain(|path, _| path != "retain.rs");
        assert!(!evidence.versions.contains_key("retain.rs"));
        assert_eq!(evidence.versions.len(), 1);
        evidence.clear();
        assert!(evidence.versions.is_empty());
    }

    #[test]
    fn untrusted_or_mismatched_versions_are_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let version = test_version();
        let mut untrusted = version.clone();
        untrusted.modified = None;
        let mut evidence = FileEvidence::default();
        evidence.insert_verified(
            "untrusted.rs".into(),
            version.stamp().clone(),
            None,
            Some(untrusted.clone()),
        );
        evidence.insert_verified(
            "mismatched.rs".into(),
            FileStamp { mtime: 0, size: 0 },
            None,
            Some(version.clone()),
        );
        assert!(evidence.versions.is_empty());

        // Public maps may be edited directly; neither lookup nor serialization
        // may turn incompatible entries into trusted evidence.
        evidence.versions.insert("untrusted.rs".into(), untrusted);
        evidence.versions.insert("mismatched.rs".into(), version);
        assert!(evidence.version("untrusted.rs").is_none());
        assert!(evidence.version("mismatched.rs").is_none());
        write_file_evidence(&evidence, dir.path()).unwrap();
        assert!(read_file_evidence(dir.path()).unwrap().versions.is_empty());
    }

    #[test]
    fn version_timestamps_roundtrip_before_the_epoch() {
        let mut version = test_version();
        version.stamp.mtime = 0;
        version.modified = Some(SystemTime::UNIX_EPOCH - std::time::Duration::from_nanos(1));
        let persisted = version.persisted().unwrap();
        assert_eq!(persisted.decode(version.stamp.clone()), Some(version));
    }

    #[cfg(unix)]
    #[test]
    fn invalid_unix_change_nanoseconds_are_not_trusted() {
        let mut version = test_version();
        for nanos in [-1, 1_000_000_000] {
            version.change_nanos = nanos;
            assert!(!version.is_trusted());
            assert!(version.persisted().is_none());
        }
    }
}
