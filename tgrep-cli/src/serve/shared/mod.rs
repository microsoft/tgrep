//! Explicit repository daemon. Never reads/writes legacy `serve.json` or merges
//! a worktree overlay into a generation. Disk storage and its ancestors are trusted.
mod protocol;
mod server;

pub use protocol::{Client, Lifecycle, MARKER, PROTOCOL, run_lifecycle};
pub use server::{Options, run};

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use tgrep_core::generations::Repository;

pub fn worktree_root(path: &Path) -> Result<PathBuf> {
    let path = std::fs::canonicalize(path)?;
    let start = if path.is_file() {
        path.parent().context("file has no parent")?
    } else {
        &path
    };
    // Do not search through a nested repository, submodule or linked worktree.
    for parent in start.ancestors() {
        if parent.join(".git").try_exists()? {
            Repository::discover(parent)?;
            return Ok(parent.to_path_buf());
        }
    }
    bail!("shared mode requires a Git worktree")
}

pub fn status(root: &Path, index_path: Option<&Path>) -> Result<()> {
    anyhow::ensure!(index_path.is_none(), "--shared does not use --index-path");
    let (client, view) = Client::registered(root)?;
    let result = client.view_request("status", &view, serde_json::json!({}))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
