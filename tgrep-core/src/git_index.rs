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

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The paths git has in its index, lowercased for case-insensitive lookup.
///
/// Only ever consulted to *rescue* a path an ignore rule matched, so a path
/// missing from here can only cost an exclusion that git would also make.
pub struct TrackedFiles {
    /// Repo-relative, `/`-separated, lowercased.
    paths: HashSet<Box<str>>,
}

impl TrackedFiles {
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
        // Linear, but only ever reached for a directory an ignore rule already
        // matched — a handful per walk, not one per entry.
        self.paths.iter().any(|p| p.starts_with(prefix.as_str()))
    }
}

fn normalise(path: &str) -> String {
    path.replace('\\', "/")
        .trim_matches('/')
        .to_ascii_lowercase()
}

/// Locate the directory holding a repository's `.git` metadata.
///
/// `.git` is usually a directory, but is a file holding `gitdir: <path>` in a
/// linked worktree or a submodule.
pub(crate) fn git_dir(repo_root: &Path) -> Option<PathBuf> {
    let dot_git = repo_root.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let target = contents.strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let path = Path::new(target);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    })
}

/// The repository metadata directory shared by all linked worktrees.
pub(crate) fn common_git_dir(git_dir: &Path) -> PathBuf {
    let Ok(target) = std::fs::read_to_string(git_dir.join("commondir")) else {
        return git_dir.to_path_buf();
    };
    let target = target.trim();
    if target.is_empty() {
        return git_dir.to_path_buf();
    }
    let path = Path::new(target);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        git_dir.join(path)
    }
}

/// Whether the repository compares paths without regard to case.
///
/// git writes `core.ignorecase = true` into the repository's own config when it
/// detects a case-insensitive filesystem, so the per-repository config is the
/// authoritative place to read it and needs no config-precedence handling.
pub fn ignores_case(repo_root: &Path) -> bool {
    let Some(git_dir) = git_dir(repo_root) else {
        return false;
    };
    let Ok(config) = std::fs::read_to_string(common_git_dir(&git_dir).join("config")) else {
        return false;
    };
    let mut in_core = false;
    for line in config.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_core = section.trim().eq_ignore_ascii_case("core");
            continue;
        }
        if !in_core {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("ignorecase") {
            // git spells boolean true several ways.
            let value = value.trim();
            return matches!(
                value.to_ascii_lowercase().as_str(),
                "true" | "yes" | "on" | "1"
            );
        }
    }
    false
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
    let bytes = std::fs::read(index_path(repo_root)?).ok()?;
    parse_index(&bytes)
}

/// Parse the git index format.
///
/// Versions 2 and 3 store each path in full, NUL-terminated and padded so the
/// next entry starts on an 8-byte boundary. Version 4 drops the padding and
/// stores each path as "strip N bytes off the end of the previous path, then
/// append this suffix", so entries can only be read in order.
fn parse_index(bytes: &[u8]) -> Option<TrackedFiles> {
    /// 62 bytes of stat data, hash and flags precede the path.
    const ENTRY_HEADER: usize = 62;

    if bytes.len() < 12 || &bytes[..4] != b"DIRC" {
        return None;
    }
    let version = u32::from_be_bytes(bytes[4..8].try_into().ok()?);
    if !(2..=4).contains(&version) {
        return None;
    }
    let count = u32::from_be_bytes(bytes[8..12].try_into().ok()?) as usize;

    // The header is attacker-controlled — reserving `count` outright lets a
    // 12-byte file claim 4 billion entries and abort the process in the
    // allocator, which is not catchable. No entry can be shorter than its
    // 62-byte header plus a NUL and padding, so the file length is a hard
    // ceiling on how many there really are.
    let mut paths = HashSet::with_capacity(count.min((bytes.len() - 12) / (ENTRY_HEADER + 1)));
    let mut pos = 12;
    let mut previous: Vec<u8> = Vec::new();
    for _ in 0..count {
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

        let path = if version >= 4 {
            let (strip, next) = read_varint(bytes, pos)?;
            pos = next;
            let keep = previous.len().checked_sub(strip)?;
            let end = memchr::memchr(0, bytes.get(pos..)?)? + pos;
            let mut path = Vec::with_capacity(keep + (end - pos));
            path.extend_from_slice(&previous[..keep]);
            path.extend_from_slice(&bytes[pos..end]);
            pos = end + 1;
            path
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
            let path = bytes.get(pos..end)?.to_vec();
            // Pad the whole entry to a multiple of 8, measured from its start.
            let unpadded = end + 1 - entry_start;
            pos = entry_start + unpadded.div_ceil(8) * 8;
            path
        };

        if version >= 4 {
            previous = path.clone();
        }
        if let Ok(text) = String::from_utf8(path) {
            paths.insert(normalise(&text).into_boxed_str());
        }
    }
    Some(TrackedFiles { paths })
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
        std::fs::write(common.join("config"), "[core]\n\tignorecase = true\n").expect("write");
        std::fs::write(worktree_git.join("commondir"), "../..\n").expect("write");
        std::fs::write(worktree_git.join("index"), v2_index(&["worktree/only.rs"])).expect("write");

        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).expect("mkdir");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git.display()),
        )
        .expect("write");

        assert!(ignores_case(&worktree));
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
