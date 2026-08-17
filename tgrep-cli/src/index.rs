/// `tgrep index` — build the trigram index.
use std::path::Path;

use anyhow::Result;
use tgrep_core::builder;

pub fn run(
    root: &Path,
    index_path: Option<&Path>,
    include_hidden: bool,
    no_ignore: bool,
    exclude_dirs: &[String],
) -> Result<()> {
    builder::build_index(root, index_path, include_hidden, no_ignore, exclude_dirs)?;
    Ok(())
}
