//! Integration tests for ripgrep-compatible CLI flags.
//!
//! Each test creates a temporary directory with known files and runs the
//! `tgrep` binary with `--no-index` (brute-force) so no index setup is needed.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

/// Create a temp directory with a few source files for testing.
fn setup_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();

    fs::write(
        dir.path().join("hello.rs"),
        "fn main() {\n    println!(\"hello world\");\n}\n",
    )
    .unwrap();

    fs::write(
        dir.path().join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();

    fs::write(
        dir.path().join("config.toml"),
        "[package]\nname = \"test\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    fs::write(
        dir.path().join("notes.txt"),
        "This is a note.\nNothing important here.\nJust some text.\n",
    )
    .unwrap();

    dir
}

fn tgrep() -> Command {
    Command::cargo_bin("tgrep").unwrap()
}

// ─── --glob / -g (multiple) ───────────────────────────────────────────

#[test]
fn glob_single_pattern() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-g",
            "*.rs",
            "fn",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello.rs"))
        .stdout(predicate::str::contains("lib.rs"))
        .stdout(predicate::str::contains("config.toml").not());
}

#[test]
fn glob_multiple_patterns() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-g",
            "*.rs",
            "-g",
            "*.toml",
            "fn|name",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello.rs"))
        .stdout(predicate::str::contains("lib.rs"))
        .stdout(predicate::str::contains("config.toml"))
        .stdout(predicate::str::contains("notes.txt").not());
}

#[test]
fn glob_no_matches() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-g",
            "*.xyz",
            "fn",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1); // no files match glob, so no matches
}

// ─── -H / --with-filename (no-op, should be accepted) ────────────────

#[test]
fn with_filename_accepted() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-H",
            "fn main",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello.rs"));
}

#[test]
fn with_filename_long_accepted() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--with-filename",
            "fn main",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello.rs"));
}

// ─── --no-filename ───────────────────────────────────────────────────

#[test]
fn no_filename_flat() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "fn main",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn main()"));
    // filename should not appear in any output line
    assert!(!stdout.contains("hello.rs"));
}

#[test]
fn no_filename_count() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-c",
            "fn",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should print counts without file prefixes
    assert!(!stdout.contains("hello.rs"));
    assert!(!stdout.contains("lib.rs"));
    // Counts should be present as bare numbers
    for line in stdout.lines() {
        assert!(
            line.trim().parse::<usize>().is_ok(),
            "expected bare count, got: {line}"
        );
    }
}

// ─── -n / --line-number (no-op, should be accepted) ──────────────────

#[test]
fn line_number_short_accepted() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-n",
            "fn main",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(":1:"));
}

#[test]
fn line_number_long_accepted() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--line-number",
            "fn main",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(":1:"));
}

// ─── -N / --no-line-number ──────────────────────────────────────────

#[test]
fn no_line_number_flat() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-N",
            "fn main",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should have file:content (no line number between them)
    for line in stdout.lines() {
        // In flat mode: "file:content" — should NOT have "file:N:content"
        let parts: Vec<&str> = line.splitn(2, ':').collect();
        assert_eq!(parts.len(), 2, "expected file:content, got: {line}");
        assert!(
            parts[1].starts_with("fn main"),
            "content should follow filename directly, got: {line}"
        );
    }
}

#[test]
fn no_line_number_long() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-line-number",
            "fn main",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(2, ':').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[1].starts_with("fn main"));
    }
}

#[test]
fn no_filename_and_no_line_number() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "fn main",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should be just the content, no file prefix, no line number
    for line in stdout.lines() {
        assert_eq!(line.trim(), "fn main() {");
    }
}

// ─── -L / --files-without-match ─────────────────────────────────────

#[test]
fn files_without_match_short() {
    let dir = setup_fixture();
    let output = tgrep()
        .args(["--no-index", "-L", "fn", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // hello.rs and lib.rs contain "fn", so they should NOT appear
    assert!(!stdout.contains("hello.rs"));
    assert!(!stdout.contains("lib.rs"));
    // config.toml and notes.txt do NOT contain "fn", so they should appear
    assert!(stdout.contains("config.toml"));
    assert!(stdout.contains("notes.txt"));
}

#[test]
fn files_without_match_long() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--files-without-match",
            "fn",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("hello.rs"));
    assert!(!stdout.contains("lib.rs"));
    assert!(stdout.contains("config.toml"));
    assert!(stdout.contains("notes.txt"));
}

#[test]
fn files_without_match_all_match() {
    let dir = setup_fixture();
    // Every file contains a newline, so "." (any char) matches all files
    tgrep()
        .args(["--no-index", "-L", ".", dir.path().to_str().unwrap()])
        .assert()
        .code(1) // no files without matches → exit 1
        .stdout(predicate::str::is_empty());
}

#[test]
fn files_without_match_none_match() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "-L",
            "zzz_nonexistent_pattern_zzz",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // No file matches, so all files should be printed
    assert!(stdout.contains("hello.rs"));
    assert!(stdout.contains("lib.rs"));
    assert!(stdout.contains("config.toml"));
    assert!(stdout.contains("notes.txt"));
}

#[test]
fn files_without_match_with_glob() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "-L",
            "-g",
            "*.rs",
            "fn main",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Only .rs files considered; hello.rs matches "fn main", lib.rs does not
    assert!(!stdout.contains("hello.rs"));
    assert!(stdout.contains("lib.rs"));
    // Non-.rs files should not appear (filtered by glob)
    assert!(!stdout.contains("config.toml"));
    assert!(!stdout.contains("notes.txt"));
}

// ─── -q / --quiet ───────────────────────────────────────────────────

#[test]
fn quiet_match_exits_zero() {
    let dir = setup_fixture();
    tgrep()
        .args(["--no-index", "-q", "fn main", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn quiet_no_match_exits_one() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "-q",
            "zzz_nonexistent_zzz",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty());
}

#[test]
fn quiet_long_form() {
    let dir = setup_fixture();
    tgrep()
        .args(["--no-index", "--quiet", "fn", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn quiet_no_stderr_on_match() {
    let dir = setup_fixture();
    tgrep()
        .args(["--no-index", "-q", "fn", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
}

// ─── Flag combinations ──────────────────────────────────────────────

#[test]
fn quiet_with_files_without_match() {
    let dir = setup_fixture();
    // -q -L: exit 0 if any file doesn't match, no output
    tgrep()
        .args([
            "--no-index",
            "-q",
            "-L",
            "fn main",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn quiet_with_files_without_match_all_match() {
    let dir = setup_fixture();
    // Every file matches "." so -L finds nothing → exit 1
    tgrep()
        .args(["--no-index", "-q", "-L", ".", dir.path().to_str().unwrap()])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty());
}

#[test]
fn no_filename_with_no_line_number_and_context() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "-A",
            "1",
            "fn main",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should get content lines without file or line number prefixes
    assert!(stdout.contains("fn main() {"));
    assert!(stdout.contains("println!"));
    assert!(!stdout.contains("hello.rs"));
}

#[test]
fn glob_multiple_with_files_only() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "-l",
            "-g",
            "*.rs",
            "-g",
            "*.toml",
            ".",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello.rs"));
    assert!(stdout.contains("lib.rs"));
    assert!(stdout.contains("config.toml"));
    assert!(!stdout.contains("notes.txt"));
}

#[test]
fn files_without_match_exit_code_success() {
    let dir = setup_fixture();
    // "fn main" only matches hello.rs; lib.rs, config.toml, notes.txt don't
    tgrep()
        .args(["--no-index", "-L", "fn main", dir.path().to_str().unwrap()])
        .assert()
        .success(); // exit 0 because files without matches were found
}

#[test]
fn with_filename_and_no_filename_last_wins() {
    let dir = setup_fixture();
    // When both are specified, --no-filename should take effect
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-H",
            "--no-filename",
            "fn main",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("hello.rs"));
    assert!(stdout.contains("fn main()"));
}
