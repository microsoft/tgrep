use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::SystemTime;

use crate::Result;

const META_FILENAME: &str = "meta.json";

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
            version: 1,
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
