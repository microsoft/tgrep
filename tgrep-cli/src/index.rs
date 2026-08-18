/// `tgrep index` — build the trigram index.
use std::path::Path;

use anyhow::Result;
use tgrep_core::builder::{self, BuildOptions, IndexStrategy};

pub fn run(
    root: &Path,
    index_path: Option<&Path>,
    include_hidden: bool,
    no_ignore: bool,
    exclude_dirs: &[String],
    strategy: IndexStrategy,
    buffer_bytes: Option<u64>,
) -> Result<()> {
    let buffer_bytes = buffer_bytes
        .and_then(|bytes| usize::try_from(bytes).ok())
        .unwrap_or(builder::DEFAULT_INDEX_BUFFER_BYTES);

    if strategy == IndexStrategy::External {
        eprintln!(
            "Using external merge sort (arena {} MB before spilling)",
            buffer_bytes / (1024 * 1024)
        );
    }

    builder::build_index_with_options(
        root,
        index_path,
        &BuildOptions {
            include_hidden,
            no_ignore,
            exclude_dirs: exclude_dirs.to_vec(),
            strategy,
            buffer_bytes,
        },
    )?;
    Ok(())
}
