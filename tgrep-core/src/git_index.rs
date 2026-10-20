//! Reading the set of files git tracks, and whether it compares paths without
//! regard to case.
//!
//! Both answers come from the repository itself — `.git/index` and
//! `.git/config` — rather than from running `git`, so this works with no git
//! installation and costs no subprocess.
//!
//! # Why a walker needs this
//!
//! An ignore rule and a tracked file can both match the same path, and git has
//! a rule for that: **a tracked file is never ignored**. Ignore rules only
//! decide what to do with files git does not already know about.
//!
//! That distinction does not matter while ignore matching is case-sensitive,
//! because a rule that matches a tracked file exactly would have stopped it
//! being added in the first place. It matters as soon as matching ignores case,
//! which is what git does when `core.ignorecase` is set — the state git chooses
//! automatically when it clones onto a case-insensitive filesystem. Then rules
//! start matching files that are already tracked, and without the exemption
//! they would disappear from the search.
//!
//! Measured on a 299,126-file Windows enlistment: matching ignore rules without
//! regard to case removes 274 files, and 273 of them are tracked — `.JPG`,
//! `.PNG` and `.RLL` files caught by rules written in lower case. The 274th is
//! a 13.4 GiB untracked build artifact in a directory the root `.gitignore`
//! spells `QLogs` and the filesystem spells `qlogs`. Applying the exemption
//! leaves exactly that one file excluded, and every tracked file in place.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// The paths git has in its index, lowercased for case-insensitive lookup.
///
/// Only ever consulted to *rescue* a path an ignore rule matched, so a path
/// missing from here can only cost an exclusion that git would also make.
pub struct TrackedFiles {
    /// Repo-relative, `/`-separated, lowercased.
    paths: HashSet<Box<str>>,
    ordered: Option<BTreeSet<Box<str>>>,
    _memory: Option<crate::managed::work::MemoryCharge>,
}

impl TrackedFiles {
    fn unordered(paths: HashSet<Box<str>>) -> Self {
        Self {
            paths,
            ordered: None,
            _memory: None,
        }
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Whether git tracks `rel_path`, which must be repo-relative.
    ///
    /// Compared without regard to case: this is only used where the ignore
    /// matching is itself case-insensitive, and there the on-disk spelling of a
    /// path need not match the spelling git recorded.
    pub fn contains(&self, rel_path: &str) -> bool {
        let normalised = normalise(rel_path);
        self.paths.contains(normalised.as_str())
    }

    /// Whether git tracks anything beneath `rel_dir`.
    ///
    /// A directory cannot be skipped just because a rule matches it — git still
    /// reports the tracked files inside. `qlogs/` is prunable because nothing
    /// under it is tracked; a directory holding even one tracked file is not.
    pub fn contains_any_under(&self, rel_dir: &str) -> bool {
        let mut prefix = normalise(rel_dir);
        if prefix.is_empty() {
            return !self.paths.is_empty();
        }
        prefix.push('/');
        if let Some(paths) = &self.ordered {
            return paths
                .range::<str, _>((
                    std::ops::Bound::Included(prefix.as_str()),
                    std::ops::Bound::Unbounded,
                ))
                .next()
                .is_some_and(|path| path.starts_with(prefix.as_str()));
        }
        // Linear, but only ever reached for a directory an ignore rule already
        // matched — a handful per walk, not one per entry.
        self.paths.iter().any(|p| p.starts_with(prefix.as_str()))
    }

    pub(crate) fn fingerprint_matching(
        &self,
        mut include: impl FnMut(&str) -> bool,
    ) -> (usize, u64, u64) {
        let mut count = 0;
        let mut xor = 0;
        let mut sum = 0u64;
        for path in &self.paths {
            if !include(path) {
                continue;
            }
            let hash = membership_hash(path);
            count += 1;
            xor ^= hash;
            sum = sum.wrapping_add(hash);
        }
        (count, xor, sum)
    }

    pub(crate) fn fingerprint_controlled(
        &self,
        permit: &std::sync::Arc<crate::managed::WorkPermit>,
    ) -> crate::managed::Result<(usize, u64, u64)> {
        let mut xor = 0;
        let mut sum = 0_u64;
        for path in &self.paths {
            permit.check()?;
            let hash = membership_hash(path);
            xor ^= hash;
            sum = sum.wrapping_add(hash);
        }
        Ok((self.paths.len(), xor, sum))
    }
}

fn normalise(path: &str) -> String {
    path.replace('\\', "/")
        .trim_matches('/')
        .to_ascii_lowercase()
}

fn membership_hash(path: &str) -> u64 {
    path.as_bytes()
        .iter()
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

/// Locate the directory holding a repository's `.git` metadata.
///
/// `.git` is usually a directory, but is a file holding `gitdir: <path>` in a
/// linked worktree or a submodule.
/// Best-effort metadata-directory hint for cheap discovery probes. Callers that
/// establish repository identity or authorize a scope must validate with Git.
pub fn git_dir(repo_root: &Path) -> Option<PathBuf> {
    read_git_dir(repo_root).ok()
}

fn metadata_path(mut bytes: &[u8]) -> std::io::Result<&Path> {
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    if bytes.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty Git metadata path",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(Path::new(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes)
            .map(Path::new)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }
}

fn read_git_dir(repo_root: &Path) -> std::io::Result<PathBuf> {
    read_git_dir_using(repo_root, |path| std::fs::read(path))
}

fn read_git_dir_using(
    repo_root: &Path,
    read: impl FnOnce(&Path) -> std::io::Result<Vec<u8>>,
) -> std::io::Result<PathBuf> {
    let dot_git = repo_root.join(".git");
    let meta = std::fs::metadata(&dot_git)?;
    if meta.is_dir() {
        return Ok(dot_git);
    }
    let contents = read(&dot_git)?;
    let target = contents.strip_prefix(b"gitdir: ").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid worktree gitfile")
    })?;
    Ok(repo_root.join(metadata_path(target)?))
}

fn read_common_dir(git_dir: &Path) -> std::io::Result<PathBuf> {
    read_common_dir_using(git_dir, |path| std::fs::read(path))
}

fn read_common_dir_using(
    git_dir: &Path,
    read: impl FnOnce(&Path) -> std::io::Result<Vec<u8>>,
) -> std::io::Result<PathBuf> {
    match read(&git_dir.join("commondir")) {
        Ok(target) => Ok(git_dir.join(metadata_path(&target)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(git_dir.to_path_buf()),
        Err(error) => Err(error),
    }
}

/// Read native worktree/common-directory paths without spawning Git.
///
/// These are not canonical identities. Authority checks must canonicalize and
/// compare them against a repository previously validated with Git.
pub fn read_repository_dirs(repo_root: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    let git_dir = read_git_dir(repo_root)?;
    let common_dir = read_common_dir(&git_dir)?;
    Ok((git_dir, common_dir))
}

/// Bound the small gitfile/commondir inputs used while registering watchers.
pub fn read_repository_dirs_bounded(repo_root: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    use std::io::Read;
    let read = |path: &Path| {
        const LIMIT: u64 = 128 * 1024;
        if !std::fs::symlink_metadata(path)?.file_type().is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "nonregular Git pointer file",
            ));
        }
        let file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take(LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LIMIT {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Git pointer file exceeds watcher input bound",
            ));
        }
        Ok(bytes)
    };
    let git_dir = read_git_dir_using(repo_root, read)?;
    let common_dir = read_common_dir_using(&git_dir, read)?;
    Ok((git_dir, common_dir))
}

pub(crate) fn read_repository_dirs_controlled(
    root: &Path,
    control: &dyn ignore::gitignore::FileReadControl,
) -> std::io::Result<(PathBuf, PathBuf)> {
    let read = |path: &Path| control.read_file(path).map(|bytes| bytes.to_vec());
    let git_dir = read_git_dir_using(root, read)?;
    let common = read_common_dir_using(&git_dir, read)?;
    Ok((git_dir, common))
}

/// The repository metadata directory shared by all linked worktrees.
pub(crate) fn common_git_dir(git_dir: &Path) -> PathBuf {
    read_common_dir(git_dir).unwrap_or_else(|_| git_dir.to_path_buf())
}

fn config_bool(config: &str, section: &str, key: &str) -> Option<bool> {
    let mut in_section = false;
    let mut result = None;
    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(header) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            in_section = header.trim().eq_ignore_ascii_case(section);
            continue;
        }
        if !in_section {
            continue;
        }
        let (candidate, value) = line
            .split_once('=')
            .map_or((line, None), |(candidate, value)| (candidate, Some(value)));
        if !candidate.trim().eq_ignore_ascii_case(key) {
            continue;
        }
        result = match value.map(str::trim) {
            None => Some(true),
            Some(value)
                if matches!(
                    value.to_ascii_lowercase().as_str(),
                    "true" | "yes" | "on" | "1"
                ) =>
            {
                Some(true)
            }
            Some(value)
                if matches!(
                    value.to_ascii_lowercase().as_str(),
                    "false" | "no" | "off" | "0" | ""
                ) =>
            {
                Some(false)
            }
            Some(_) => None,
        };
    }
    result
}

/// Whether the repository compares paths without regard to case.
///
/// A repository may opt into per-worktree config. In that layout Git reads the
/// common config first, then lets `$GIT_DIR/config.worktree` override it.
pub fn ignores_case(repo_root: &Path) -> bool {
    let Some(git_dir) = git_dir(repo_root) else {
        return false;
    };
    let common = common_git_dir(&git_dir);
    let Ok(config) = std::fs::read_to_string(common.join("config")) else {
        return false;
    };
    let common_ignorecase = config_bool(&config, "core", "ignorecase").unwrap_or(false);
    if !config_bool(&config, "extensions", "worktreeConfig").unwrap_or(false) {
        return common_ignorecase;
    }
    std::fs::read_to_string(git_dir.join("config.worktree"))
        .ok()
        .and_then(|config| config_bool(&config, "core", "ignorecase"))
        .unwrap_or(common_ignorecase)
}

pub(crate) fn ignores_case_controlled(
    repository: &crate::generations::Repository,
    control: &dyn ignore::gitignore::FileReadControl,
) -> std::io::Result<bool> {
    let read = |path: PathBuf| -> std::io::Result<Option<String>> {
        match control.read_file(&path) {
            Ok(bytes) => std::str::from_utf8(&bytes)
                .map(|text| Some(text.to_string()))
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    };
    let Some(config) = read(repository.common_dir().join("config"))? else {
        return Ok(false);
    };
    let common = config_bool(&config, "core", "ignorecase").unwrap_or(false);
    if !config_bool(&config, "extensions", "worktreeConfig").unwrap_or(false) {
        return Ok(common);
    }
    Ok(read(repository.git_dir().join("config.worktree"))?
        .and_then(|config| config_bool(&config, "core", "ignorecase"))
        .unwrap_or(common))
}

/// Read the tracked paths from `.git/index`.
///
/// Returns `None` when there is no readable index, which the caller must treat
/// as "exempt nothing" only where that is the safe direction.
/// The index file a repository's tracked-file set is read from.
pub(crate) fn index_path(repo_root: &Path) -> Option<PathBuf> {
    Some(git_dir(repo_root)?.join("index"))
}

pub fn load_tracked(repo_root: &Path) -> Option<TrackedFiles> {
    let path = index_path(repo_root)?;
    let bytes = std::fs::read(&path).ok()?;
    // A split index names its shared half relative to the directory holding
    // it, which for a linked worktree is that worktree's private Git dir.
    let dir = path.parent()?;
    parse_index_with(&bytes, |hash| {
        std::fs::read(dir.join(format!("sharedindex.{hash}"))).ok()
    })
}

pub(crate) fn load_tracked_controlled(
    root: &Path,
    permit: &std::sync::Arc<crate::managed::WorkPermit>,
) -> crate::managed::Result<TrackedFiles> {
    use crate::managed::process::{Control, PipedProcess};
    let mut command = crate::generations::git::command();
    command
        .current_dir(root)
        .args(["ls-files", "--cached", "--deduplicate", "-z"]);
    let mut memory = permit.memory(0)?;
    let mut process = PipedProcess::spawn(&mut command, Control::work(permit), false)?;
    let limit = usize::try_from(permit.private_limit() / 64)
        .map_err(|_| crate::managed::Error::pressure("tracked-path-address-range"))?;
    let bytes = process.read_output_accounted(limit, Some(&mut memory), 32)?;
    let (status, stderr) = process.finish()?;
    if !status.success() {
        return Err(crate::generations::GenerationError::Git {
            operation: "enumerate worktree tracked paths",
            code: status.code(),
            stderr,
        }
        .into());
    }
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return Err(crate::managed::Error::corrupt(
            "unterminated tracked-path output",
        ));
    }
    let mut paths = HashSet::new();
    let mut ordered = BTreeSet::new();
    for path in bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        permit.check()?;
        let path = std::str::from_utf8(path)
            .map_err(|_| crate::managed::Error::incompatible("tracked path is not UTF-8"))?;
        crate::generations::validate_tracked_path(path)?;
        let path = normalise(path).into_boxed_str();
        ordered.insert(path.clone());
        paths.insert(path);
    }
    Ok(TrackedFiles {
        paths,
        ordered: Some(ordered),
        _memory: Some(memory),
    })
}

/// 62 bytes of stat data, object id and flags precede each entry's path.
const ENTRY_HEADER: usize = 62;
/// Every index ends with a checksum at least this long (SHA-1; SHA-256 is 32).
const MIN_TRAILER: usize = 20;

/// Parse an index that is known not to be split.
#[cfg(test)]
fn parse_index(bytes: &[u8]) -> Option<TrackedFiles> {
    parse_index_with(bytes, |_| None)
}

/// Parse the git index format, reading the shared half of a split index
/// through `shared`, which maps a hex object id to the bytes of
/// `sharedindex.<id>`.
///
/// Returns `None` for anything it cannot read completely: a partial set would
/// silently drop the exemption for every file it missed.
fn parse_index_with(
    bytes: &[u8],
    shared: impl Fn(&str) -> Option<Vec<u8>>,
) -> Option<TrackedFiles> {
    let (_, count) = index_header(bytes)?;
    let mut paths = HashSet::with_capacity(entry_capacity(bytes, count));
    let end = walk_entries(bytes, |_, path| insert_path(&mut paths, path))?;
    let Some(link) = extension(bytes, end, b"link") else {
        return Some(TrackedFiles::unordered(paths));
    };
    // Split index: these entries are only the changes git layered on a shared
    // index. Without the shared half, most tracked files would look untracked.
    for hash_len in [20, 32] {
        let Some(link) = SplitLink::parse(link, hash_len) else {
            continue;
        };
        if link.base.iter().all(|byte| *byte == 0) {
            // A null base means this index needs no shared file.
            return Some(TrackedFiles::unordered(paths));
        }
        let Some(base) = shared(&hex(link.base)) else {
            continue;
        };
        drop(paths);
        return merge_split(bytes, &base, &link);
    }
    None
}

/// Apply a split index's `link` to its shared index, as git's
/// `merge_base_index` does.
///
/// The shared index keeps every entry not in the delete bitmap. The first `n`
/// entries of the split index, where `n` is the number of bits in the replace
/// bitmap, update shared entries in place without renaming them. The rest are
/// additions.
fn merge_split(split: &[u8], base: &[u8], link: &SplitLink) -> Option<TrackedFiles> {
    // The shared file is named by its own trailing checksum.
    if base.len() < link.base.len() || &base[base.len() - link.base.len()..] != link.base {
        return None;
    }
    let (_, base_count) = index_header(base)?;
    let base_count = base_count.min(entry_capacity(base, base_count));
    let mut deleted = vec![false; base_count];
    read_ewah(link.delete, base_count, |position| {
        deleted[position] = true;
        true
    })?;
    let mut replaced = 0usize;
    read_ewah(link.replace, base_count, |position| {
        replaced += 1;
        !deleted[position]
    })?;

    let (_, split_count) = index_header(split)?;
    if replaced > split_count {
        return None;
    }
    let added = entry_capacity(split, split_count).saturating_sub(replaced);
    let mut paths = HashSet::with_capacity(base_count + added);
    let base_end = walk_entries(base, |entry, path| {
        // No more entries can parse than `entry_capacity` allows, so every
        // visited position is inside the bitmap.
        if deleted.get(entry) == Some(&false) {
            insert_path(&mut paths, path);
        }
    })?;
    // A shared index is never itself split.
    if extension(base, base_end, b"link").is_some() {
        return None;
    }
    walk_entries(split, |entry, path| {
        if entry >= replaced {
            insert_path(&mut paths, path);
        }
    })?;
    Some(TrackedFiles::unordered(paths))
}

/// The payload of a split index's `link` extension.
struct SplitLink<'a> {
    /// Object id of the shared index.
    base: &'a [u8],
    delete: &'a [u8],
    replace: &'a [u8],
}

impl<'a> SplitLink<'a> {
    /// Split the payload assuming an object id of `hash_len` bytes. Both
    /// bitmaps are optional, but when present they must fill the rest exactly,
    /// which is what tells a SHA-1 link from a SHA-256 one.
    fn parse(data: &'a [u8], hash_len: usize) -> Option<Self> {
        let base = data.get(..hash_len)?;
        let rest = &data[hash_len..];
        if rest.is_empty() {
            return Some(Self {
                base,
                delete: &[],
                replace: &[],
            });
        }
        let delete_len = ewah_len(rest)?;
        let replace_len = ewah_len(&rest[delete_len..])?;
        if delete_len + replace_len != rest.len() {
            return None;
        }
        Some(Self {
            base,
            delete: &rest[..delete_len],
            replace: &rest[delete_len..],
        })
    }
}

/// The serialized length of the EWAH bitmap at the start of `bytes`: bit
/// count, word count, the words, and the position of the last marker word.
fn ewah_len(bytes: &[u8]) -> Option<usize> {
    let words = u32::from_be_bytes(bytes.get(4..8)?.try_into().ok()?) as usize;
    let len = words.checked_mul(8)?.checked_add(12)?;
    (len <= bytes.len()).then_some(len)
}

/// Visit every set bit of a serialized EWAH bitmap, as git's `ewah_each_bit`.
///
/// The words alternate between a marker and the literal words it announces.
/// A marker's low bit says whether its run is of ones or zeros, the next 32
/// bits how many 64-bit words the run spans, and the top 31 how many literal
/// words follow. Every set bit must be below `limit`, so a forged run of ones
/// cannot drive an unbounded loop. An empty input has no bits.
fn read_ewah(bytes: &[u8], limit: usize, mut visit: impl FnMut(usize) -> bool) -> Option<()> {
    if bytes.is_empty() {
        return Some(());
    }
    let words = (ewah_len(bytes)? - 12) / 8;
    let word = |index: usize| {
        let at = 8 + index * 8;
        Some(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
    };
    let mut position = 0usize;
    let mut index = 0;
    while index < words {
        let marker = word(index)?;
        index += 1;
        let run = usize::try_from((marker >> 1) & 0xFFFF_FFFF)
            .ok()?
            .checked_mul(64)?;
        let end = position.checked_add(run)?;
        if marker & 1 != 0 {
            if end > limit {
                return None;
            }
            for bit in position..end {
                if !visit(bit) {
                    return None;
                }
            }
        }
        position = end;
        for _ in 0..marker >> 33 {
            if index >= words {
                return None;
            }
            let mut literal = word(index)?;
            index += 1;
            while literal != 0 {
                let bit = position.checked_add(literal.trailing_zeros() as usize)?;
                if bit >= limit || !visit(bit) {
                    return None;
                }
                literal &= literal - 1;
            }
            position = position.checked_add(64)?;
        }
    }
    Some(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The payload of the first extension named `signature`, scanning from
/// `pos`, where the entries end.
///
/// Each extension is a 4-byte signature, a 4-byte big-endian length and that
/// much data. The scan stops at the first header that would not leave room for
/// the trailing checksum, which is how it finds the end without knowing the
/// repository's hash.
fn extension<'a>(bytes: &'a [u8], mut pos: usize, signature: &[u8; 4]) -> Option<&'a [u8]> {
    while let Some(header) = bytes.get(pos..pos.checked_add(8)?) {
        let size = u32::from_be_bytes(header[4..8].try_into().ok()?) as usize;
        let start = pos + 8;
        let end = start.checked_add(size)?;
        if end.checked_add(MIN_TRAILER)? > bytes.len() {
            return None;
        }
        if &header[..4] == signature {
            return Some(&bytes[start..end]);
        }
        pos = end;
    }
    None
}

/// Validate the 12-byte header, returning the version and declared count.
fn index_header(bytes: &[u8]) -> Option<(u32, usize)> {
    if bytes.len() < 12 || &bytes[..4] != b"DIRC" {
        return None;
    }
    let version = u32::from_be_bytes(bytes[4..8].try_into().ok()?);
    if !(2..=4).contains(&version) {
        return None;
    }
    let count = u32::from_be_bytes(bytes[8..12].try_into().ok()?) as usize;
    Some((version, count))
}

/// How many entries are worth reserving for.
///
/// The header is attacker-controlled — reserving `count` outright lets a
/// 12-byte file claim 4 billion entries and abort the process in the
/// allocator, which is not catchable. No entry can be shorter than its
/// 62-byte header plus a NUL, so the file length is a hard ceiling on how
/// many there really are.
fn entry_capacity(bytes: &[u8], count: usize) -> usize {
    count.min(bytes.len().saturating_sub(12) / (ENTRY_HEADER + 1))
}

fn insert_path(paths: &mut HashSet<Box<str>>, path: &[u8]) {
    if let Ok(text) = std::str::from_utf8(path) {
        paths.insert(normalise(text).into_boxed_str());
    }
}

/// Visit each entry's position and path in on-disk order, returning the
/// offset where the extensions begin.
///
/// Versions 2 and 3 store each path in full, NUL-terminated and padded so the
/// next entry starts on an 8-byte boundary. Version 4 drops the padding and
/// stores each path as "strip N bytes off the end of the previous path, then
/// append this suffix", so entries can only be read in order.
fn walk_entries(bytes: &[u8], mut visit: impl FnMut(usize, &[u8])) -> Option<usize> {
    let (version, count) = index_header(bytes)?;
    let mut pos = 12;
    let mut previous: Vec<u8> = Vec::new();
    for entry in 0..count {
        if pos + ENTRY_HEADER > bytes.len() {
            return None;
        }
        let entry_start = pos;
        let flags = u16::from_be_bytes(bytes[pos + 60..pos + 62].try_into().ok()?);
        pos += ENTRY_HEADER;
        // Version 3 introduced a second flag word, present only when the
        // extended bit is set. Version 4 keeps that rule; version 2 has no
        // such word at all.
        if version >= 3 && flags & 0x4000 != 0 {
            pos += 2;
            if pos > bytes.len() {
                return None;
            }
        }

        let path: &[u8] = if version >= 4 {
            let (strip, next) = read_varint(bytes, pos)?;
            pos = next;
            let keep = previous.len().checked_sub(strip)?;
            let end = memchr::memchr(0, bytes.get(pos..)?)? + pos;
            previous.truncate(keep);
            previous.extend_from_slice(&bytes[pos..end]);
            pos = end + 1;
            &previous
        } else {
            // The low 12 bits hold the length, but saturate at 0xFFF; a longer
            // path is delimited only by its NUL.
            let named = (flags & 0x0FFF) as usize;
            let end = if named < 0x0FFF {
                let end = pos.checked_add(named)?;
                if bytes.get(end) != Some(&0) {
                    return None;
                }
                end
            } else {
                memchr::memchr(0, bytes.get(pos..)?)? + pos
            };
            let path = bytes.get(pos..end)?;
            // Pad the whole entry to a multiple of 8, measured from its start.
            let unpadded = end + 1 - entry_start;
            pos = entry_start + unpadded.div_ceil(8) * 8;
            path
        };
        visit(entry, path);
    }
    Some(pos)
}

/// git's offset-encoded varint, as used by index version 4.
///
/// Big-endian, 7 bits per byte, and each continuation adds one to the value so
/// that no integer has two encodings.
fn read_varint(bytes: &[u8], mut pos: usize) -> Option<(usize, usize)> {
    let mut byte = *bytes.get(pos)?;
    pos += 1;
    let mut value = (byte & 0x7F) as usize;
    while byte & 0x80 != 0 {
        value = value.checked_add(1)?.checked_shl(7)?;
        byte = *bytes.get(pos)?;
        pos += 1;
        value = value.checked_add((byte & 0x7F) as usize)?;
    }
    Some((value, pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_paths_strip_only_git_line_endings() {
        for bytes in [b" metadata ".as_slice(), b" metadata \r\n"] {
            assert_eq!(metadata_path(bytes).unwrap(), Path::new(" metadata "));
        }
        assert!(metadata_path(b"\r\n").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn metadata_paths_preserve_native_bytes() {
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            metadata_path(b"../repo-\xff/.git\r\n")
                .unwrap()
                .as_os_str()
                .as_bytes(),
            b"../repo-\xff/.git"
        );
    }

    /// Build a version 2 index the way git writes one, so the parser is tested
    /// against the layout rather than against itself.
    fn v2_index(paths: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"DIRC");
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&(paths.len() as u32).to_be_bytes());
        for path in paths {
            let start = out.len();
            out.extend_from_slice(&[0u8; 60]);
            let named = path.len().min(0x0FFF) as u16;
            out.extend_from_slice(&named.to_be_bytes());
            out.extend_from_slice(path.as_bytes());
            out.push(0);
            while (out.len() - start) % 8 != 0 {
                out.push(0);
            }
        }
        out
    }

    #[test]
    fn reads_a_version_2_index() {
        let index = v2_index(&["src/main.rs", "a.txt", "deeply/nested/path/file.cs"]);
        let tracked = parse_index(&index).expect("parses");
        assert_eq!(tracked.len(), 3);
        assert!(tracked.contains("src/main.rs"));
        assert!(tracked.contains("a.txt"));
        assert!(tracked.contains("deeply/nested/path/file.cs"));
        assert!(!tracked.contains("src/other.rs"));
    }

    #[test]
    fn matches_paths_without_regard_to_case_or_separator() {
        let index = v2_index(&["Assets/Logo.PNG"]);
        let tracked = parse_index(&index).expect("parses");
        // The point of the exemption: the rule that matched said `*.png`, and
        // the walker hands back whatever the filesystem spelled.
        assert!(tracked.contains("assets/logo.png"));
        assert!(tracked.contains("Assets\\Logo.PNG"));
        assert!(tracked.contains("/Assets/Logo.PNG"));
    }

    #[test]
    fn finds_tracked_files_under_a_directory() {
        let index = v2_index(&["keep/a.txt", "qlogs_like/but_tracked.txt"]);
        let tracked = parse_index(&index).expect("parses");
        assert!(tracked.contains_any_under("keep"));
        assert!(tracked.contains_any_under("Keep"));
        assert!(!tracked.contains_any_under("qlogs"));
        // A prefix must stop at a path separator, or `qlogs` would claim
        // `qlogs_like/`.
        assert!(tracked.contains_any_under("qlogs_like"));
    }

    #[test]
    fn handles_a_path_at_and_beyond_the_length_field_limit() {
        // The 12-bit length field saturates, after which the NUL is the only
        // delimiter. Straddle that boundary.
        for len in [0x0FFE, 0x0FFF, 0x1000, 0x1100] {
            let long = "d/".to_string() + &"x".repeat(len - 2);
            let index = v2_index(&[&long, "after.txt"]);
            let tracked = parse_index(&index).unwrap_or_else(|| panic!("parses at {len}"));
            assert_eq!(tracked.len(), 2, "at {len}");
            assert!(tracked.contains(&long), "at {len}");
            assert!(tracked.contains("after.txt"), "at {len}");
        }
    }

    #[test]
    fn rejects_input_that_is_not_an_index() {
        assert!(parse_index(b"").is_none());
        assert!(parse_index(b"NOPE\0\0\0\x02\0\0\0\x01").is_none());
        // A version it does not understand is refused rather than guessed at.
        let mut wrong_version = v2_index(&["a.txt"]);
        wrong_version[7] = 9;
        assert!(parse_index(&wrong_version).is_none());
    }

    #[test]
    fn refuses_a_truncated_index_instead_of_inventing_entries() {
        let index = v2_index(&["src/main.rs", "b.txt"]);
        // Cuts that land in the header, inside the first entry's path, and
        // inside the second entry's path.
        for cut in [13, 20, 40, 79, 157] {
            assert!(
                parse_index(&index[..cut]).is_none(),
                "accepted an index truncated to {cut}"
            );
        }
    }

    #[test]
    fn tolerates_the_trailing_data_a_real_index_carries() {
        // Entries are not the end of the file: git appends extensions and a
        // trailing checksum. Requiring the last entry to end the file would
        // reject every index git actually writes.
        let mut index = v2_index(&["src/main.rs", "b.txt"]);
        index.extend_from_slice(b"TREE");
        index.extend_from_slice(&[0u8; 24]);
        let tracked = parse_index(&index).expect("parses despite trailing data");
        assert_eq!(tracked.len(), 2);
        assert!(tracked.contains("src/main.rs"));
    }

    #[test]
    fn a_count_larger_than_the_data_is_refused() {
        let mut index = v2_index(&["a.txt"]);
        index[11] = 40;
        assert!(parse_index(&index).is_none());
    }

    #[test]
    fn a_header_claiming_four_billion_entries_does_not_reserve_for_them() {
        // A bare header claiming u32::MAX entries. Reserving that many would ask
        // the allocator for roughly 146GB and abort the process before any of the
        // per-entry length checks below ever run, so the clamp has to happen at
        // the reservation itself. Reaching the `None` at all proves it did.
        let mut index = Vec::from(*b"DIRC");
        index.extend_from_slice(&2u32.to_be_bytes());
        index.extend_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(index.len(), 12);
        assert!(parse_index(&index).is_none());
    }

    #[test]
    fn reads_a_version_4_index_with_prefix_compressed_paths() {
        // Written by hand because v4 entries are only decodable in order, so a
        // round-trip through our own writer would not prove much.
        let mut out = Vec::new();
        out.extend_from_slice(b"DIRC");
        out.extend_from_slice(&4u32.to_be_bytes());
        out.extend_from_slice(&3u32.to_be_bytes());
        // Each entry: 62 bytes, then varint strip count, then suffix + NUL.
        for (strip, suffix) in [(0usize, "src/alpha.rs"), (8, "beta.rs"), (7, "gamma.rs")] {
            out.extend_from_slice(&[0u8; 60]);
            out.extend_from_slice(&(suffix.len() as u16).to_be_bytes());
            out.push(strip as u8);
            out.extend_from_slice(suffix.as_bytes());
            out.push(0);
        }
        let tracked = parse_index(&out).expect("parses");
        assert_eq!(tracked.len(), 3);
        assert!(tracked.contains("src/alpha.rs"));
        assert!(tracked.contains("src/beta.rs"));
        assert!(tracked.contains("src/gamma.rs"));
    }

    /// Serialize `bits` as git's EWAH: one marker announcing literal words.
    fn ewah(bits: &[usize]) -> Vec<u8> {
        let literals = bits.iter().max().map_or(0, |max| max / 64 + 1);
        let mut words = vec![0u64; literals];
        for bit in bits {
            words[bit / 64] |= 1 << (bit % 64);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&(literals as u32 * 64).to_be_bytes());
        out.extend_from_slice(&(literals as u32 + 1).to_be_bytes());
        out.extend_from_slice(&((literals as u64) << 33).to_be_bytes());
        for word in words {
            out.extend_from_slice(&word.to_be_bytes());
        }
        out.extend_from_slice(&0u32.to_be_bytes());
        out
    }

    /// `index` with an extension and a checksum appended.
    fn with_extension(mut index: Vec<u8>, signature: &[u8; 4], data: &[u8]) -> Vec<u8> {
        index.extend_from_slice(signature);
        index.extend_from_slice(&(data.len() as u32).to_be_bytes());
        index.extend_from_slice(data);
        index.extend_from_slice(&[0x5a; 20]);
        index
    }

    fn link(base: &[u8; 20], deleted: &[usize], replaced: &[usize]) -> Vec<u8> {
        let mut data = base.to_vec();
        data.extend(ewah(deleted));
        data.extend(ewah(replaced));
        data
    }

    #[test]
    fn reads_a_split_index_through_its_shared_half() {
        let base_id = [0xab; 20];
        let mut shared = v2_index(&["a.txt", "b.txt", "c.txt", "d.txt"]);
        shared.extend_from_slice(&base_id);
        // `b.txt` deleted; `c.txt` replaced by the nameless first entry;
        // `e.txt` added.
        let split = with_extension(
            v2_index(&["", "e.txt"]),
            b"link",
            &link(&base_id, &[1], &[2]),
        );
        let tracked = parse_index_with(&split, |hash| {
            (hash == "ab".repeat(20)).then(|| shared.clone())
        })
        .expect("parses");
        assert_eq!(tracked.len(), 4);
        for path in ["a.txt", "c.txt", "d.txt", "e.txt"] {
            assert!(tracked.contains(path), "{path}");
        }
        assert!(!tracked.contains("b.txt"));
        assert!(!tracked.contains(""));

        // Without the bitmaps nothing is deleted or replaced.
        let split = with_extension(v2_index(&["e.txt"]), b"link", &base_id);
        let tracked = parse_index_with(&split, |_| Some(shared.clone())).expect("parses");
        assert_eq!(tracked.len(), 5);
    }

    #[test]
    fn a_split_index_without_a_matching_shared_half_is_refused() {
        let base_id = [0xab; 20];
        let split = with_extension(v2_index(&["e.txt"]), b"link", &link(&base_id, &[], &[]));
        // Reading only the split half would claim one tracked file.
        assert!(parse_index(&split).is_none());

        let mut stale = v2_index(&["a.txt"]);
        stale.extend_from_slice(&[0xcd; 20]);
        assert!(parse_index_with(&split, |_| Some(stale.clone())).is_none());

        // Bits beyond the shared index, or more replacements than the split
        // index has entries, are corrupt.
        let mut shared = v2_index(&["a.txt"]);
        shared.extend_from_slice(&base_id);
        for data in [link(&base_id, &[1], &[]), link(&base_id, &[], &[0])] {
            let split = with_extension(v2_index(&[]), b"link", &data);
            assert!(parse_index_with(&split, |_| Some(shared.clone())).is_none());
        }

        // A null base needs no shared half.
        let split = with_extension(v2_index(&["e.txt"]), b"link", &[0; 20]);
        assert!(parse_index(&split).expect("parses").contains("e.txt"));
    }

    #[test]
    fn ewah_runs_and_literals_are_bounded_by_the_shared_index() {
        // A run of ones over one word, then one literal with bit 3 set.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&128u32.to_be_bytes());
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&(1u64 | (1 << 1) | (1 << 33)).to_be_bytes());
        bytes.extend_from_slice(&(1u64 << 3).to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        let mut bits = Vec::new();
        read_ewah(&bytes, 100, |bit| {
            bits.push(bit);
            true
        })
        .expect("reads");
        assert_eq!(bits, (0..64).chain([67]).collect::<Vec<_>>());
        assert!(read_ewah(&bytes, 67, |_| true).is_none());
        assert!(read_ewah(&bytes, 60, |_| true).is_none());
        // A marker announcing more literal words than the bitmap holds.
        bytes[8] = 0x01;
        assert!(read_ewah(&bytes, 1 << 20, |_| true).is_none());
    }

    #[test]
    fn reads_the_split_indexes_git_writes() {
        let run = |root: &Path, args: &[&str]| {
            let mut command = std::process::Command::new("git");
            for (key, _) in std::env::vars_os() {
                if key
                    .to_string_lossy()
                    .to_ascii_uppercase()
                    .starts_with("GIT_")
                {
                    command.env_remove(key);
                }
            }
            let empty = root.join("..").join("empty-config");
            std::fs::write(&empty, "").expect("write config");
            let output = command
                .current_dir(root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", &empty)
                .args([
                    "-c",
                    "user.name=Split Index",
                    "-c",
                    "user.email=split@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.splitIndex=true",
                    "-c",
                    "splitIndex.maxPercentChange=100",
                    "-c",
                    "index.skipHash=false",
                ])
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        for version in ["2", "4"] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path().join("repo");
            std::fs::create_dir_all(root.join("dir")).expect("mkdir");
            run(&root, &["init", "-q"]);
            for file in 0..40 {
                std::fs::write(root.join(format!("dir/f{file:02}.txt")), "base").expect("write");
            }
            run(&root, &["add", "."]);
            run(&root, &["commit", "-q", "-m", "base"]);
            run(&root, &["update-index", "--index-version", version]);
            run(&root, &["update-index", "--split-index"]);
            run(&root, &["rm", "-q", "dir/f03.txt"]);
            std::fs::write(root.join("dir/f05.txt"), "changed").expect("write");
            std::fs::create_dir_all(root.join("new")).expect("mkdir");
            std::fs::write(root.join("new/added.txt"), "added").expect("write");
            run(&root, &["add", "dir/f05.txt", "new/added.txt"]);

            let shared = std::fs::read_dir(root.join(".git"))
                .expect("read .git")
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("sharedindex.")
                })
                .count();
            assert!(shared > 0, "v{version}: git did not split the index");
            let index = std::fs::read(root.join(".git").join("index")).expect("read index");
            assert!(
                index_header(&index).expect("header").1 > 0,
                "v{version}: the changes were not layered on the shared index"
            );
            assert!(
                parse_index(&index).is_none(),
                "v{version}: the split half alone is incomplete"
            );

            let listed = run(&root, &["ls-files", "-z"]);
            let expected: Vec<&str> = listed
                .split(|byte| *byte == 0)
                .filter(|path| !path.is_empty())
                .map(|path| std::str::from_utf8(path).expect("utf-8"))
                .collect();
            let tracked = load_tracked(&root).expect("loads a split index");
            assert_eq!(tracked.len(), expected.len(), "v{version}");
            for path in &expected {
                assert!(tracked.contains(path), "v{version}: {path}");
            }
            assert!(tracked.contains("new/added.txt"), "v{version}");
            assert!(tracked.contains("dir/f05.txt"), "v{version}");
            assert!(!tracked.contains("dir/f03.txt"), "v{version}");
        }
    }

    #[test]
    fn reads_ignorecase_from_a_repository_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = tmp.path().join(".git");
        std::fs::create_dir_all(&git).expect("mkdir");

        let cases = [
            ("[core]\n\tignorecase = true\n", true),
            ("[core]\n\tignorecase = false\n", false),
            ("[core]\n\tignorecase = True\n", true),
            ("[CORE]\n\tIgnoreCase = yes\n", true),
            // Set, but in another section: not ours to read.
            (
                "[core]\n\tbare = false\n[other]\n\tignorecase = true\n",
                false,
            ),
            ("[core]\n\tbare = false\n", false),
            ("", false),
        ];
        for (config, want) in cases {
            std::fs::write(git.join("config"), config).expect("write");
            assert_eq!(ignores_case(tmp.path()), want, "for config {config:?}");
        }
    }

    #[test]
    fn follows_a_gitdir_pointer_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real-git-dir");
        std::fs::create_dir_all(&real).expect("mkdir");
        std::fs::write(real.join("config"), "[core]\n\tignorecase = true\n").expect("write");
        std::fs::write(real.join("index"), v2_index(&["worktree/file.rs"])).expect("write");

        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).expect("mkdir");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", real.display()),
        )
        .expect("write");

        assert!(ignores_case(&worktree));
        let tracked = load_tracked(&worktree).expect("loads through the pointer");
        assert!(tracked.contains("worktree/file.rs"));
    }

    #[test]
    fn linked_worktree_uses_the_common_config_and_its_own_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let common = tmp.path().join("repo.git");
        let worktree_git = common.join("worktrees").join("topic");
        std::fs::create_dir_all(&worktree_git).expect("mkdir");
        std::fs::write(worktree_git.join("commondir"), "../..\n").expect("write");
        std::fs::write(worktree_git.join("index"), v2_index(&["worktree/only.rs"])).expect("write");

        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).expect("mkdir");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git.display()),
        )
        .expect("write");

        std::fs::write(
            common.join("config"),
            "[core]\n\tignorecase = true\n[extensions]\n\tworktreeConfig\n",
        )
        .expect("write");
        std::fs::write(
            worktree_git.join("config.worktree"),
            "[core]\n\tignorecase = false\n",
        )
        .expect("write");
        assert!(
            !ignores_case(&worktree),
            "worktree false must override common true"
        );

        std::fs::write(
            common.join("config"),
            "[core]\n\tignorecase = false\n[extensions]\n\tworktreeConfig = true\n",
        )
        .expect("write");
        std::fs::write(
            worktree_git.join("config.worktree"),
            "[core]\n\tignorecase = true\n",
        )
        .expect("write");
        assert!(ignores_case(&worktree));

        std::fs::write(common.join("config"), "[core]\n\tignorecase = false\n").expect("write");
        assert!(
            !ignores_case(&worktree),
            "config.worktree is inactive until the extension is enabled"
        );
        assert_eq!(index_path(&worktree), Some(worktree_git.join("index")));
        assert!(
            load_tracked(&worktree)
                .expect("loads worktree index")
                .contains("worktree/only.rs")
        );
    }

    #[test]
    fn a_directory_without_git_reports_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!ignores_case(tmp.path()));
        assert!(load_tracked(tmp.path()).is_none());
    }
}
