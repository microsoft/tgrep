//! The 64 MiB default size cap, which is a deliberate divergence from ripgrep.
//!
//! ripgrep has no default `--max-filesize`. tgrep does, because a file a walk
//! picks up is also a file the index carries and re-reads on every query whose
//! trigrams make it a candidate, so one outlier is paid for repeatedly rather
//! than once. On a real enlistment a single 13.41 GiB generated artifact was
//! 71% of all searchable bytes and 39x the cost of an otherwise sub-second
//! query.
//!
//! The cap can hide a real match, so these tests pin the two things that keep
//! that from being silent: `--no-max-filesize`, and the rule that a file named
//! directly on the command line is never dropped by the *inherited* default.
//! They also pin the boundary itself, since a default nobody tests is a default
//! that drifts.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn tgrep_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("tgrep")
}

const NEEDLE: &str = "NEEDLE_OVER_THE_CAP";
const FILLER: &str = "filler line that is not interesting at all\n";

/// A repo holding one small file and one file just past the 64 MiB default,
/// both containing the needle.
///
/// The oversized file is built by writing real bytes rather than `set_len`,
/// because the search path has to *read* it — a sparse hole would be searched
/// as NUL bytes and rejected as binary, which would pass the "not found" case
/// for entirely the wrong reason.
fn fixture() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("testdata");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(
        root.join("small.txt"),
        format!("{NEEDLE} in a small file\n"),
    )
    .unwrap();

    let big_path = root.join("big.txt");
    let mut big = std::io::BufWriter::new(std::fs::File::create(&big_path).unwrap());
    // The needle goes in first so that a truncated read cannot be the reason a
    // search fails to find it.
    writeln!(big, "{NEEDLE} in a big file").unwrap();
    let mut written = 0usize;
    while written <= 64 * 1024 * 1024 {
        big.write_all(FILLER.as_bytes()).unwrap();
        written += FILLER.len();
    }
    big.flush().unwrap();
    drop(big);

    assert!(
        std::fs::metadata(&big_path).unwrap().len() > 64 * 1024 * 1024,
        "fixture must land above the default cap"
    );
    (dir, big_path)
}

/// Run a search and return the paths that matched, one per line.
fn search(target: &Path, extra: &[&str]) -> Vec<String> {
    let out = Command::new(tgrep_bin())
        .arg("--no-index")
        .args(extra)
        .args(["--files-with-matches", NEEDLE])
        .arg(target)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| {
            Path::new(l.trim())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn a_walk_drops_files_past_the_default_cap() {
    let (dir, _big) = fixture();
    let root = dir.path().join("testdata");

    // The small sibling proves the walk itself worked, so the missing big file
    // is the cap and not a broken fixture.
    assert_eq!(search(&root, &[]), vec!["small.txt"]);
}

#[test]
fn no_max_filesize_restores_ripgrep_behaviour() {
    let (dir, _big) = fixture();
    let root = dir.path().join("testdata");

    let mut found = search(&root, &["--no-max-filesize"]);
    found.sort();
    assert_eq!(found, vec!["big.txt", "small.txt"]);
}

#[test]
fn a_raised_cap_also_restores_the_file() {
    let (dir, _big) = fixture();
    let root = dir.path().join("testdata");

    let mut found = search(&root, &["--max-filesize", "128M"]);
    found.sort();
    assert_eq!(found, vec!["big.txt", "small.txt"]);
}

#[test]
fn naming_a_file_defeats_the_inherited_default() {
    let (_dir, big) = fixture();

    // Pointing at a file is an unambiguous request to search it. Answering "no
    // match" because of a limit the user never asked for would be a lie, and an
    // exit status of 1 makes it an actionable one.
    assert_eq!(search(&big, &[]), vec!["big.txt"]);
}

#[test]
fn naming_a_file_still_honours_a_cap_the_user_set() {
    let (_dir, big) = fixture();

    // The exemption is for the *inherited* default only. Once the user names a
    // limit, the limit is itself the request.
    assert!(search(&big, &["--max-filesize", "1M"]).is_empty());
}

#[test]
fn the_default_is_last_flag_wins() {
    let (dir, _big) = fixture();
    let root = dir.path().join("testdata");

    // `--max-filesize` and `--no-max-filesize` override one another, so a
    // wrapper script that sets one cannot be silently defeated by argument
    // order it did not choose. The small file stays visible under the 1M cap,
    // which is what distinguishes "the later flag won" from "the search broke".
    assert_eq!(
        search(&root, &["--no-max-filesize", "--max-filesize", "1M"]),
        vec!["small.txt"]
    );
    let mut found = search(&root, &["--max-filesize", "1M", "--no-max-filesize"]);
    found.sort();
    assert_eq!(found, vec!["big.txt", "small.txt"]);
}
