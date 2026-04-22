use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use crate::Result;

const META_FILENAME: &str = "meta.json";
const FILESTAMPS_FILENAME: &str = "filestamps.json";

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

/// Collect file stamps (mtime + size) for a list of relative paths under `root`.
pub fn collect_filestamps(root: &Path, paths: &[String]) -> HashMap<String, FileStamp> {
    use rayon::prelude::*;

    paths
        .par_iter()
        .filter_map(|rel_path| {
            let full_path = root.join(rel_path);
            std::fs::metadata(&full_path).ok().map(|metadata| {
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                (
                    rel_path.clone(),
                    FileStamp {
                        mtime,
                        size: metadata.len(),
                    },
                )
            })
        })
        .collect()
}
