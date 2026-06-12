/// `tgrep index` — build the trigram index.
use std::path::Path;

use anyhow::Result;
use tgrep_core::builder;

pub fn run(
    root: &Path,
    index_path: Option<&Path>,
    include_hidden: bool,
    exclude_dirs: &[String],
) -> Result<()> {
    let root = std::fs::canonicalize(root)?;
    crate::repo_guard::ensure_can_recursively_walk(&root, "index")?;
    builder::build_index(&root, index_path, include_hidden, exclude_dirs)?;
    Ok(())
}
