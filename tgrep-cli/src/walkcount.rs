/// `tgrep count-files` — count text files using the fast parallel walker.
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use tgrep_core::walker::{self, WalkOptions};

pub fn run(root: &Path, include_hidden: bool, no_ignore: bool) -> Result<()> {
    let root = std::fs::canonicalize(root)?;
    let start = Instant::now();

    let walk = walker::walk_dir(
        &root,
        &WalkOptions {
            include_hidden,
            no_ignore,
            ..Default::default()
        },
    );

    let elapsed = start.elapsed();
    let total = walk.files.len();

    println!("{total}");
    eprintln!(
        "{} text files ({} binary skipped, {} too large, {} errors) in {:.0}ms",
        total,
        walk.skipped_binary,
        walk.skipped_too_large,
        walk.skipped_error,
        elapsed.as_secs_f64() * 1000.0
    );

    Ok(())
}
