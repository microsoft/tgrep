//! Explicit repository daemon. Never reads/writes legacy `serve.json` or merges
//! a worktree overlay into a generation. Disk storage and its ancestors are trusted.
mod managed;
mod protocol;
mod protocol_v2;
mod server;

pub use protocol::{Client, Lifecycle, MARKER, PROTOCOL, run_lifecycle};
pub use server::{Options, run};

pub fn run_managed(
    root: &std::path::Path,
    options: Options<'_>,
    policy: &std::path::Path,
) -> anyhow::Result<()> {
    managed::run(root, options, protocol_v2::read_policy(policy)?)
}

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

/// Tolerates transient `accept` failures, such as a peer that reset before it
/// was accepted or a brief descriptor shortage. Either daemon holds every
/// attached lease, so one such failure must not end it; a failure that
/// persists for [`AcceptRetry::LIMIT`] consecutive attempts is still fatal.
struct AcceptRetry {
    consecutive: u32,
    backoff: std::time::Duration,
}

impl AcceptRetry {
    const LIMIT: u32 = 100;

    fn new() -> Self {
        Self {
            consecutive: 0,
            backoff: std::time::Duration::from_millis(100),
        }
    }

    fn accepted(&mut self) {
        self.consecutive = 0;
    }

    /// Log and back off after a failed accept, or return the error once the
    /// failures have persisted.
    fn failed(&mut self, error: std::io::Error) -> std::io::Result<()> {
        self.consecutive += 1;
        if self.consecutive >= Self::LIMIT {
            return Err(error);
        }
        eprintln!("shared daemon accept failed; retrying: {error}");
        std::thread::sleep(self.backoff);
        Ok(())
    }
}

#[cfg(test)]
mod accept_tests {
    use super::AcceptRetry;

    #[test]
    fn transient_accept_failures_are_retried_until_they_persist() {
        let failure = || std::io::Error::from(std::io::ErrorKind::ConnectionAborted);
        let mut retry = AcceptRetry {
            consecutive: 0,
            backoff: std::time::Duration::ZERO,
        };
        for _ in 1..AcceptRetry::LIMIT {
            retry.failed(failure()).unwrap();
        }
        retry.accepted();
        for _ in 1..AcceptRetry::LIMIT {
            retry.failed(failure()).unwrap();
        }
        let error = retry.failed(failure()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionAborted);
    }
}

pub fn status(root: &Path, index_path: Option<&Path>) -> Result<()> {
    anyhow::ensure!(index_path.is_none(), "--shared does not use --index-path");
    let (client, view) = Client::registered(root)?;
    let result = client.view_request("status", &view, serde_json::json!({}))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
