/// `tgrep index` — build the trigram index.
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use tgrep_core::builder::{self, BuildOptions, IndexStrategy};

use crate::mem;

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

    let started = Instant::now();
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
    let elapsed = started.elapsed();

    let strategy_label = match strategy {
        IndexStrategy::InMemory => "memory",
        IndexStrategy::External => "external",
    };
    // Peak is an OS high-water mark for the whole process. `tgrep index` does
    // nothing substantial before the build, so it reads as the build's peak.
    match mem::peak_rss_bytes() {
        Some(peak) => eprintln!(
            "Indexed in {:.1}s using {} strategy (peak memory {})",
            elapsed.as_secs_f64(),
            strategy_label,
            mem::format_bytes(peak)
        ),
        None => eprintln!(
            "Indexed in {:.1}s using {} strategy",
            elapsed.as_secs_f64(),
            strategy_label
        ),
    }
    Ok(())
}
