//! Explicit repository daemon. Never reads/writes legacy `serve.json` or merges
//! a worktree overlay into a generation. Disk storage and its ancestors are trusted.
mod protocol;
mod server;

pub use protocol::{Client, Lifecycle, MARKER, PROTOCOL, run_lifecycle};
pub use server::{Options, run};

use anyhow::{Context, Result, bail, ensure};
use std::path::{Path, PathBuf};
use tgrep_core::generations::Repository;

fn canonical_root(path: &Path) -> Result<PathBuf> {
    let root = std::fs::canonicalize(path)?;
    ensure!(
        root.to_str().is_some(),
        "shared protocol requires a UTF-8 root"
    );
    Ok(root)
}

pub fn worktree_root(path: &Path) -> Result<PathBuf> {
    let path = canonical_root(path)?;
    let start = if path.is_file() {
        path.parent().context("file has no parent")?
    } else {
        &path
    };
    // Do not search through a nested repository, submodule or linked worktree.
    for parent in start.ancestors() {
        if parent.join(".git").try_exists()? {
            return Ok(parent.to_path_buf());
        }
    }
    bail!("shared mode requires a Git worktree")
}

/// Recheck filesystem identity against the repository validated at attachment,
/// without running Git on the daemon's query workers.
fn validate_repository(root: &Path, repository: &Repository) -> Result<()> {
    ensure!(
        worktree_root(root)? == root,
        "root is no longer a worktree root"
    );
    let (git_dir, common_dir) = tgrep_core::git_index::read_repository_dirs(root)
        .context("reading worktree Git directories")?;
    let git_dir = std::fs::canonicalize(git_dir)?;
    ensure!(
        git_dir == repository.git_dir(),
        "worktree Git directory changed; reattach"
    );
    ensure!(
        std::fs::canonicalize(common_dir)? == repository.common_dir(),
        "worktree repository changed; reattach"
    );
    Ok(())
}

pub fn status(root: &Path, index_path: Option<&Path>) -> Result<()> {
    anyhow::ensure!(index_path.is_none(), "--shared does not use --index-path");
    let (client, view) = Client::registered(root)?;
    let result = client.view_request("status", &view, serde_json::json!({}))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
