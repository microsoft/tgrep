use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use super::{GenerationError, Repository, Result, TrackedEntry};
use crate::managed::process::{Control, PipedProcess};

pub(crate) fn command() -> Command {
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

fn controlled_output(
    command: &mut Command,
    operation: &'static str,
    control: Option<&Control>,
    limit: usize,
) -> Result<Vec<u8>> {
    let Some(control) = control else {
        return output(command, operation);
    };
    let mut process = PipedProcess::spawn(command, control.clone(), false)?;
    let bytes = process.read_output(limit)?;
    let (status, stderr) = process.finish()?;
    if !status.success() {
        return Err(GenerationError::Git {
            operation,
            code: status.code(),
            stderr,
        });
    }
    Ok(bytes)
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

pub(super) fn worktree_root(root: &Path) -> Result<PathBuf> {
    discover_directory(root, "--show-toplevel", None)
}

pub(super) fn discover_directory(
    root: &Path,
    option: &str,
    control: Option<&Control>,
) -> Result<PathBuf> {
    let mut bytes = controlled_output(
        command()
            .current_dir(root)
            .args(["rev-parse", "--path-format=absolute", option]),
        "discover common directory",
        control,
        128 * 1024,
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
    worktrees_controlled(repository, None)
}

pub(super) fn worktrees_controlled(
    repository: &Repository,
    control: Option<&Control>,
) -> Result<Vec<PathBuf>> {
    let bytes = controlled_output(
        repository_command(repository).args(["worktree", "list", "--porcelain", "-z"]),
        "list worktrees",
        control,
        1024 * 1024,
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

pub(super) fn object_format_controlled(
    repository: &Repository,
    control: Option<&Control>,
) -> Result<usize> {
    let bytes = controlled_output(
        repository_command(repository).args(["rev-parse", "--show-object-format"]),
        "read object format",
        control,
        128,
    )?;
    match bytes.as_slice() {
        b"sha1\n" => Ok(40),
        b"sha256\n" => Ok(64),
        _ => Err(GenerationError::Unsupported(
            "unsupported Git object format".into(),
        )),
    }
}

fn resolve(repository: &Repository, revision: &str, control: Option<&Control>) -> Result<String> {
    let bytes = controlled_output(
        repository_command(repository)
            .current_dir(repository.git_dir())
            .args(["rev-parse", "--verify", "--end-of-options", revision]),
        "resolve committed revision",
        control,
        128,
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
    commit_tree_controlled(repository, revision, None)
}

pub(super) fn commit_tree_controlled(
    repository: &Repository,
    revision: &str,
    control: Option<&Control>,
) -> Result<(String, String)> {
    let commit = resolve(repository, &format!("{revision}^{{commit}}"), control)?;
    let tree = resolve(repository, &format!("{commit}^{{tree}}"), control)?;
    Ok((commit, tree))
}

pub(crate) fn entries_controlled(
    repository: &Repository,
    tree: &str,
    control: Option<&Control>,
    limit: usize,
    memory: Option<&mut crate::managed::work::MemoryCharge>,
) -> Result<Vec<TrackedEntry>> {
    let mut command = repository_command(repository);
    command.args(["ls-tree", "-r", "-z", "-l", "--full-tree", tree]);
    let bytes = if let Some(control) = control {
        let mut process = PipedProcess::spawn(&mut command, control.clone(), false)?;
        let bytes = process.read_output_accounted(limit, memory, 32)?;
        let (status, stderr) = process.finish()?;
        if !status.success() {
            return Err(GenerationError::Git {
                operation: "enumerate committed tree",
                code: status.code(),
                stderr,
            });
        }
        bytes
    } else {
        output(&mut command, "enumerate committed tree")?
    };
    parse_entries(repository, &bytes, control)
}

pub(crate) fn entries_for_paths(
    repository: &Repository,
    tree: &str,
    paths: &[&str],
    control: &Control,
    limit: usize,
    max_paths: u32,
) -> Result<Vec<TrackedEntry>> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut remaining = limit;
    while start < paths.len() {
        let mut end = start;
        let mut command_bytes = 0;
        while end < paths.len() {
            let bytes = paths[end].len().saturating_mul(2).saturating_add(4);
            if bytes > 16384 {
                return Err(crate::managed::Error::pressure("adaptive-path-argument-limit").into());
            }
            if command_bytes + bytes > 16384 {
                break;
            }
            command_bytes += bytes;
            end += 1;
        }
        let bytes = controlled_output(
            repository_command(repository)
                .arg("--literal-pathspecs")
                .args(["ls-tree", "-r", "-z", "-l", "--full-tree", tree, "--"])
                .args(&paths[start..end]),
            "inspect adaptive target paths",
            Some(control),
            remaining,
        )?;
        remaining -= bytes.len();
        entries.extend(parse_entries(repository, &bytes, Some(control))?);
        if entries.len() > max_paths as usize {
            return Err(crate::managed::Error::pressure("adaptive-target-path-limit").into());
        }
        start = end;
    }
    entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    entries.dedup_by(|a, b| a.path == b.path);
    Ok(entries)
}

fn parse_entries(
    repository: &Repository,
    bytes: &[u8],
    control: Option<&Control>,
) -> Result<Vec<TrackedEntry>> {
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
        if let Some(control) = control {
            control.check()?;
        }
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
    if let Some(control) = control {
        let mut ordered = std::collections::BTreeMap::new();
        for entry in entries.drain(..) {
            control.check()?;
            if ordered.insert(entry.path.clone(), entry).is_some() {
                return Err(GenerationError::InvalidMetadata(
                    "duplicate tracked path".into(),
                ));
            }
        }
        for entry in ordered.into_values() {
            control.check()?;
            entries.push(entry);
        }
    } else {
        entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        if entries.windows(2).any(|pair| pair[0].path == pair[1].path) {
            return Err(GenerationError::InvalidMetadata(
                "duplicate tracked path".into(),
            ));
        }
    }
    Ok(entries)
}

/// A single batch-plumbing process; stderr is spooled to avoid pipe deadlock.
pub(crate) struct LegacyBlobs {
    child: Option<Child>,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    stderr: tempfile::NamedTempFile,
}

impl LegacyBlobs {
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

impl Drop for LegacyBlobs {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(crate) enum Blobs {
    Legacy(LegacyBlobs),
    Controlled(PipedProcess),
}

impl Blobs {
    pub(crate) fn new(repository: &Repository) -> Result<Self> {
        Ok(Self::Legacy(LegacyBlobs::new(repository)?))
    }

    pub(crate) fn controlled(repository: &Repository, control: Control) -> Result<Self> {
        Ok(Self::Controlled(PipedProcess::spawn(
            repository_command(repository).args(["cat-file", "--batch"]),
            control,
            true,
        )?))
    }

    pub(crate) fn read(&mut self, oid: &str, size: u64) -> Result<Vec<u8>> {
        let Self::Controlled(process) = self else {
            let Self::Legacy(process) = self else {
                unreachable!()
            };
            return process.read(oid, size);
        };
        let input = process
            .input
            .as_mut()
            .ok_or_else(|| GenerationError::InvalidMetadata("Git batch input closed".into()))?;
        writeln!(input, "{oid}")?;
        input.flush()?;
        let output = process
            .output
            .as_mut()
            .ok_or_else(|| GenerationError::InvalidMetadata("Git batch output closed".into()))?;
        let mut header = Vec::new();
        (&mut *output).take(256).read_until(b'\n', &mut header)?;
        if header != format!("{oid} blob {size}\n").as_bytes() {
            return Err(GenerationError::InvalidMetadata(
                "unexpected bounded cat-file response".into(),
            ));
        }
        let length = usize::try_from(size)
            .map_err(|_| crate::managed::Error::pressure("blob-address-range"))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| crate::managed::Error::pressure("blob-memory"))?;
        bytes.resize(length, 0);
        output.read_exact(&mut bytes)?;
        let mut newline = [0];
        output.read_exact(&mut newline)?;
        if newline != *b"\n" {
            return Err(GenerationError::InvalidMetadata(
                "unterminated cat-file blob".into(),
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn finish(self) -> Result<()> {
        match self {
            Self::Legacy(process) => process.finish(),
            Self::Controlled(process) => {
                let (status, stderr) = process.finish()?;
                if !status.success() {
                    return Err(GenerationError::Git {
                        operation: "read committed blobs",
                        code: status.code(),
                        stderr,
                    });
                }
                Ok(())
            }
        }
    }
}
