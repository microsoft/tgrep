//! Integration test: searching files large enough to be memory-mapped.
//!
//! Reading a file with `fs::read` costs its full size in heap, so large files
//! are mapped instead and searched straight out of the mapping. That path is
//! only taken above a size threshold, which means the rest of the suite — whose
//! fixtures are all comfortably small — never exercises it. These tests are the
//! only coverage of the mapped path, so they deliberately build files past the
//! threshold and check that results are indistinguishable from the read path.
//!
//! The mapping is only valid when the mapped bytes are exactly the bytes to
//! search, so the cases that must *decline* to map matter as much as the one
//! that maps: invalid UTF-8 needs lossy repair, and a BOM needs transcoding.
//! Both must fall back and still return correct results.

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

fn tgrep_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("tgrep")
}

/// Comfortably above the 1 MiB mapping threshold in `search.rs`.
const LARGE_ENOUGH_TO_MAP: usize = 2 * 1024 * 1024;

const FILLER: &str = "the quick brown fox jumps over the lazy dog\n";
const NEEDLE_LINE: &str = "NEEDLE_ALPHA marker\n";

/// Build text past the mapping threshold, returning it with the 1-based line
/// numbers that hold the needle.
fn large_text_with_needles() -> (String, Vec<usize>) {
    let mut text = String::with_capacity(LARGE_ENOUGH_TO_MAP + FILLER.len());
    let mut needle_lines = Vec::new();
    let mut lineno = 0usize;
    while text.len() < LARGE_ENOUGH_TO_MAP {
        lineno += 1;
        // Spread the needles across the file so a bug that only reads the head
        // or tail of the mapping cannot pass.
        if lineno % 20_000 == 7 {
            text.push_str(NEEDLE_LINE);
            needle_lines.push(lineno);
        } else {
            text.push_str(FILLER);
        }
    }
    assert!(
        needle_lines.len() >= 3,
        "fixture should scatter several needles, got {}",
        needle_lines.len()
    );
    (text, needle_lines)
}

fn search(path: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new(tgrep_bin())
        .arg("--no-index")
        .args(args)
        .arg("NEEDLE_ALPHA")
        .arg(path)
        .output()
        .expect("tgrep should run");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_mapped_file_reports_the_same_lines_as_a_read_one() {
    let dir = TempDir::new().unwrap();
    let (text, needle_lines) = large_text_with_needles();
    let path = dir.path().join("huge.txt");
    std::fs::write(&path, &text).unwrap();

    assert!(
        std::fs::metadata(&path).unwrap().len() as usize > 1024 * 1024,
        "fixture must exceed the mapping threshold or this tests nothing"
    );

    let count = search(&path, &["-c"]);
    assert_eq!(
        count.trim(),
        needle_lines.len().to_string(),
        "mapped file should count every needle"
    );

    // Line numbers are the part most likely to break if the mapped bytes and
    // the searched bytes ever disagree.
    let numbered = search(&path, &["-n"]);
    let reported: Vec<usize> = numbered
        .lines()
        .filter_map(|l| l.split(':').next()?.trim().parse().ok())
        .collect();
    assert_eq!(
        reported, needle_lines,
        "mapped file should report exact lines"
    );
}

#[test]
fn a_large_file_with_invalid_utf8_still_searches() {
    // Invalid UTF-8 cannot be mapped and searched directly: it needs lossy
    // repair first, so this must fall back to the read path and still match.
    let dir = TempDir::new().unwrap();
    let (text, needle_lines) = large_text_with_needles();
    let mut bytes = text.into_bytes();
    bytes.extend_from_slice(&[0xFF, 0xFE, 0xFF]);
    bytes.extend_from_slice(NEEDLE_LINE.as_bytes());

    let path = dir.path().join("invalid.txt");
    std::fs::write(&path, &bytes).unwrap();

    let count = search(&path, &["-c"]);
    assert_eq!(
        count.trim(),
        (needle_lines.len() + 1).to_string(),
        "invalid UTF-8 must fall back to the read path, not lose matches"
    );
}

#[test]
fn a_large_utf16_file_still_searches() {
    // A BOM means the bytes on disk are not the bytes to search, so the
    // mapping must be declined in favour of transcoding.
    let dir = TempDir::new().unwrap();
    let (text, needle_lines) = large_text_with_needles();

    let mut bytes = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }

    let path = dir.path().join("utf16.txt");
    std::fs::write(&path, &bytes).unwrap();

    let count = search(&path, &["-c"]);
    assert_eq!(
        count.trim(),
        needle_lines.len().to_string(),
        "UTF-16 must be transcoded rather than searched as mapped bytes"
    );
}
