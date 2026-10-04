use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use super::{GenerationError, Repository, Result, TrackedEntry};

pub(super) fn command() -> Command {
    let mut command = Command::new("git");
    // Ambient Git state must not redirect a request to another repository.
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    command
        .arg("--no-replace-objects")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    command
}

fn output(command: &mut Command, operation: &'static str) -> Result<Vec<u8>> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(GenerationError::Git {
            operation,
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output.stdout)
}

fn path(bytes: Vec<u8>) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(bytes).into())
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes)
            .map(|value| OsString::from(value).into())
            .map_err(|_| {
                GenerationError::Unsupported("Git returned a non-UTF-8 native path".into())
            })
    }
}

pub(super) fn common_dir(root: &Path) -> Result<PathBuf> {
    discover_directory(root, "--git-common-dir")
}

pub(super) fn worktree_git_dir(root: &Path) -> Result<PathBuf> {
    discover_directory(root, "--absolute-git-dir")
}

fn discover_directory(root: &Path, option: &str) -> Result<PathBuf> {
    let mut bytes = output(
        command()
            .current_dir(root)
            .args(["rev-parse", "--path-format=absolute", option]),
        "discover common directory",
    )?;
    if bytes.pop() != Some(b'\n') {
        return Err(GenerationError::Unsupported("unterminated Git path".into()));
    }
    #[cfg(windows)]
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    let directory = path(bytes)?;
    if !directory.is_absolute() {
        return Err(GenerationError::Unsupported(
            "Git requires --path-format=absolute support".into(),
        ));
    }
    Ok(std::fs::canonicalize(directory)?)
}

pub(super) fn repository_command(repository: &Repository) -> Command {
    let mut command = command();
    // Git for Windows does not consistently accept verbatim paths in --git-dir.
    // Set the native process cwd instead; keep the canonical identity lossless.
    command
        .current_dir(repository.common_dir())
        .args(["--git-dir", "."]);
    command
}

pub(super) fn worktrees(repository: &Repository) -> Result<Vec<PathBuf>> {
    let bytes = output(
        repository_command(repository).args(["worktree", "list", "--porcelain", "-z"]),
        "list worktrees",
    )?;
    bytes
        .split(|&byte| byte == 0)
        .filter_map(|field| field.strip_prefix(b"worktree "))
        .map(|bytes| path(bytes.to_vec()))
        .collect()
}

pub(super) fn valid_oid(oid: &str, length: usize) -> bool {
    oid.len() == length
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn object_format(repository: &Repository) -> Result<usize> {
    let bytes = output(
        repository_command(repository).args(["rev-parse", "--show-object-format"]),
        "read object format",
    )?;
    match bytes.as_slice() {
        b"sha1\n" => Ok(40),
        b"sha256\n" => Ok(64),
        _ => Err(GenerationError::Unsupported(
            "unsupported Git object format".into(),
        )),
    }
}

fn resolve(repository: &Repository, revision: &str) -> Result<String> {
    let bytes = output(
        repository_command(repository)
            .current_dir(repository.git_dir())
            .args(["rev-parse", "--verify", "--end-of-options", revision]),
        "resolve committed revision",
    )?;
    let oid = String::from_utf8(bytes)
        .map_err(|_| GenerationError::InvalidMetadata("non-ASCII object ID".into()))?;
    let oid = oid.strip_suffix('\n').unwrap_or(&oid);
    if !valid_oid(oid, repository.oid_length) {
        return Err(GenerationError::InvalidMetadata(
            "invalid resolved object ID".into(),
        ));
    }
    Ok(oid.into())
}

pub(super) fn commit_tree(repository: &Repository, revision: &str) -> Result<(String, String)> {
    let commit = resolve(repository, &format!("{revision}^{{commit}}"))?;
    let tree = resolve(repository, &format!("{commit}^{{tree}}"))?;
    Ok((commit, tree))
}

pub(super) fn entries(repository: &Repository, tree: &str) -> Result<Vec<TrackedEntry>> {
    let bytes = output(
        repository_command(repository).args(["ls-tree", "-r", "-z", "-l", "--full-tree", tree]),
        "enumerate committed tree",
    )?;
    let mut entries = Vec::new();
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return Err(GenerationError::InvalidMetadata(
            "unterminated ls-tree output".into(),
        ));
    }
    for record in bytes
        .split(|&byte| byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|&byte| byte == b'\t')
            .ok_or_else(|| GenerationError::InvalidMetadata("malformed ls-tree record".into()))?;
        let (header, path) = (&record[..tab], &record[tab + 1..]);
        let header = std::str::from_utf8(header)
            .map_err(|_| GenerationError::InvalidMetadata("malformed ls-tree header".into()))?;
        let parts: Vec<_> = header.split_ascii_whitespace().collect();
        if parts.len() != 4 || !valid_oid(parts[2], repository.oid_length) {
            return Err(GenerationError::InvalidMetadata(
                "malformed ls-tree fields".into(),
            ));
        }
        let mode = match (parts[0], parts[1]) {
            ("100644", "blob") => super::EntryMode::File,
            ("100755", "blob") => super::EntryMode::Executable,
            ("120000", "blob") => super::EntryMode::Symlink,
            ("160000", "commit") => super::EntryMode::Gitlink,
            _ => {
                return Err(GenerationError::Unsupported(format!(
                    "unsupported Git mode: {}",
                    parts[0]
                )));
            }
        };
        let size = if mode == super::EntryMode::Gitlink {
            if parts[3] != "-" {
                return Err(GenerationError::InvalidMetadata(
                    "gitlink has a blob size".into(),
                ));
            }
            None
        } else {
            Some(
                parts[3]
                    .parse::<u64>()
                    .map_err(|_| GenerationError::InvalidMetadata("invalid blob size".into()))?,
            )
        };
        let path = std::str::from_utf8(path).map_err(|_| {
            GenerationError::Unsupported(
                "non-UTF-8 tracked paths are not supported by the index format".into(),
            )
        })?;
        super::validate_tracked_path(path)?;
        entries.push(TrackedEntry {
            path: path.into(),
            oid: parts[2].into(),
            mode,
            size,
            content: super::EntryContent::NotRegular,
        });
    }
    entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    if entries.windows(2).any(|pair| pair[0].path == pair[1].path) {
        return Err(GenerationError::InvalidMetadata(
            "duplicate tracked path".into(),
        ));
    }
    Ok(entries)
}

/// A single batch-plumbing process; stderr is spooled to avoid pipe deadlock.
pub(super) struct Blobs {
    child: Option<Child>,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    stderr: tempfile::NamedTempFile,
}

impl Blobs {
    pub(super) fn new(repository: &Repository) -> Result<Self> {
        let stderr = tempfile::NamedTempFile::new()?;
        let mut child = repository_command(repository)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr.reopen()?))
            .spawn()?;
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(Self {
            child: Some(child),
            input,
            output,
            stderr,
        })
    }

    pub(super) fn read(&mut self, oid: &str, size: u64) -> Result<Vec<u8>> {
        let input = self.input.as_mut().expect("active batch input");
        writeln!(input, "{oid}")?;
        input.flush()?;
        let mut header = String::new();
        self.output.read_line(&mut header)?;
        if header != format!("{oid} blob {size}\n") {
            return Err(GenerationError::InvalidMetadata(format!(
                "unexpected cat-file response: {header:?}"
            )));
        }
        let length = usize::try_from(size)
            .map_err(|_| GenerationError::Unsupported("blob exceeds addressable memory".into()))?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(|error| {
            GenerationError::Unsupported(format!("cannot allocate blob: {error}"))
        })?;
        bytes.resize(length, 0);
        self.output.read_exact(&mut bytes)?;
        let mut newline = [0];
        self.output.read_exact(&mut newline)?;
        if newline != *b"\n" {
            return Err(GenerationError::InvalidMetadata(
                "unterminated cat-file blob".into(),
            ));
        }
        Ok(bytes)
    }

    pub(super) fn finish(mut self) -> Result<()> {
        self.input.take();
        let status = self.child.as_mut().expect("active batch process").wait()?;
        self.child.take();
        if !status.success() {
            let mut stderr = String::new();
            let file: &mut File = self.stderr.as_file_mut();
            file.seek(SeekFrom::Start(0))?;
            file.read_to_string(&mut stderr)?;
            return Err(GenerationError::Git {
                operation: "read committed blobs",
                code: status.code(),
                stderr,
            });
        }
        Ok(())
    }
}

impl Drop for Blobs {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
