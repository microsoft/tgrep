/// `tgrep index` — build the trigram index.
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use tgrep_core::builder::{self, BuildOptions, IndexStrategy};

use crate::mem;

/// Convert `--index-buffer <MB>` to bytes.
///
/// Rejects values this platform cannot represent instead of quietly falling
/// back to the default: silently indexing with a 64 MB arena after the user
/// asked for something else would misreport what actually ran.
fn buffer_bytes_from_mb(mb: u64) -> Result<usize> {
    let max_mb = (usize::MAX / (1024 * 1024)) as u64;
    mb.checked_mul(1024 * 1024)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .with_context(|| {
            format!("--index-buffer {mb} is too large for this platform (maximum {max_mb} MB)")
        })
}

/// Everything `tgrep index` needs to build an index.
///
/// A struct rather than a long parameter list: the arguments are all plain
/// scalars, so positional calls are easy to transpose without the compiler
/// noticing.
pub struct RunOptions<'a> {
    pub root: &'a Path,
    pub index_path: Option<&'a Path>,
    pub include_hidden: bool,
    pub no_ignore: bool,
    pub exclude_dirs: &'a [String],
    pub strategy: IndexStrategy,
    pub index_buffer_mb: Option<u64>,
    pub max_file_size: Option<u64>,
}

pub fn run(opts: RunOptions<'_>) -> Result<()> {
    let RunOptions {
        root,
        index_path,
        include_hidden,
        no_ignore,
        exclude_dirs,
        strategy,
        index_buffer_mb,
        max_file_size,
    } = opts;
    let explicit_buffer = index_buffer_mb.is_some();
    let buffer_bytes = index_buffer_mb
        .map(buffer_bytes_from_mb)
        .transpose()?
        .unwrap_or(builder::DEFAULT_INDEX_BUFFER_BYTES);

    // External is the default, so announcing it on every run would be noise.
    // Confirm the arena only when the user explicitly tuned it.
    if strategy == IndexStrategy::External && explicit_buffer {
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
            max_file_size,
            ..Default::default()
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
