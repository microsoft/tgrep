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
    builder::build_index(root, index_path, include_hidden, exclude_dirs)?;
    Ok(())
}
