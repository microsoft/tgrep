use std::path::{Path, PathBuf};

use anyhow::Result;

const MICROSOFT_REMOTE_MARKERS: &[&str] = &[
    "dev.azure.com/microsoft",
    "microsoft.visualstudio.com",
    "ssh.dev.azure.com:v3/microsoft",
    "github.com/microsoft",
];

pub(crate) fn ensure_can_recursively_walk(root: &Path, operation: &str) -> Result<()> {
    if root.is_file() || !is_windows_os_repo_root(root) {
        return Ok(());
    }

    anyhow::bail!(
        "refusing to recursively enumerate the Windows OS repo at `{}` for `{operation}`. \
         Pass a narrower subdirectory or file path instead.",
        root.display()
    );
}

fn is_windows_os_repo_root(root: &Path) -> bool {
    let Some(config_path) = git_config_path(root) else {
        return false;
    };

    let Ok(config) = std::fs::read_to_string(config_path) else {
        return false;
    };

    config.lines().any(|line| {
        line.trim_start()
            .strip_prefix("url")
            .and_then(|rest| rest.trim_start().strip_prefix('='))
            .is_some_and(|url| is_windows_os_remote(url.trim()))
    })
}

fn git_config_path(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git.join("config"));
    }

    let git_file = std::fs::read_to_string(&dot_git).ok()?;
    let git_dir = git_file
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))?
        .trim();
    let git_dir = PathBuf::from(git_dir);
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        root.join(git_dir)
    };
    Some(git_dir.join("config"))
}

fn is_windows_os_remote(url: &str) -> bool {
    let normalized = url
        .trim_end_matches(".git")
        .replace('\\', "/")
        .to_ascii_lowercase();

    if !MICROSOFT_REMOTE_MARKERS
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        return false;
    }

    normalized.contains("/os/_git/os")
        || normalized.contains("ssh.dev.azure.com:v3/microsoft/os/os")
        || normalized.contains("github.com/microsoft/windows")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_git_config(root: &Path, remote_url: &str) {
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(
            root.join(".git").join("config"),
            format!("[remote \"origin\"]\n    url = {remote_url}\n"),
        )
        .unwrap();
    }

    #[test]
    fn detects_windows_os_azure_devops_remote() {
        let dir = TempDir::new().unwrap();
        write_git_config(
            dir.path(),
            "https://microsoft.visualstudio.com/DefaultCollection/OS/_git/OS",
        );

        assert!(is_windows_os_repo_root(dir.path()));
    }

    #[test]
    fn ignores_non_windows_os_remote() {
        let dir = TempDir::new().unwrap();
        write_git_config(dir.path(), "https://github.com/microsoft/tgrep");

        assert!(!is_windows_os_repo_root(dir.path()));
    }

    #[test]
    fn ignores_non_microsoft_os_remote() {
        let dir = TempDir::new().unwrap();
        write_git_config(dir.path(), "https://dev.azure.com/example/OS/_git/OS");

        assert!(!is_windows_os_repo_root(dir.path()));
    }

    #[test]
    fn guard_allows_files_inside_windows_os_repo() {
        let dir = TempDir::new().unwrap();
        write_git_config(dir.path(), "https://dev.azure.com/microsoft/OS/_git/OS");
        let file = dir.path().join("readme.txt");
        fs::write(&file, "hello").unwrap();

        ensure_can_recursively_walk(&file, "search").unwrap();
    }

    #[test]
    fn guard_rejects_windows_os_repo_root() {
        let dir = TempDir::new().unwrap();
        write_git_config(dir.path(), "https://dev.azure.com/microsoft/OS/_git/OS");

        let err = ensure_can_recursively_walk(dir.path(), "count-files").unwrap_err();
        assert!(err.to_string().contains("Windows OS repo"));
    }
}
