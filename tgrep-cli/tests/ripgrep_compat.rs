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

    // Create a visible subdirectory for test files so the parallel walker
    // (which respects hidden-directory filtering) always finds them, even
    // when TempDir creates a dot-prefixed path like /tmp/.tmpXXXXXX.
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();

    fs::write(
        sub.join("hello.rs"),
        "fn main() {\n    println!(\"hello world\");\n}\n",
    )
    .unwrap();

    fs::write(
        sub.join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();

    fs::write(
        sub.join("config.toml"),
        "[package]\nname = \"test\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    fs::write(
        sub.join("notes.txt"),
        "This is a note.\nNothing important here.\nJust some text.\n",
    )
    .unwrap();

    dir
}

/// Returns the path to the test files inside the fixture.
fn fixture_path(dir: &TempDir) -> String {
    dir.path().join("testdata").to_str().unwrap().to_string()
}

fn tgrep() -> Command {
    Command::cargo_bin("tgrep").unwrap()
}

// ─── Multiple and normalized path arguments ───────────────────────────

#[test]
fn accepts_multiple_path_arguments() {
    let dir = setup_fixture();
    let root = dir.path().join("testdata");
    let hello = root.join("hello.rs").to_str().unwrap().to_string();
    let lib = root.join("lib.rs").to_str().unwrap().to_string();

    tgrep()
        .args(["--no-index", "--no-heading", "fn", &hello, &lib])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello.rs"))
        .stdout(predicate::str::contains("lib.rs"));
}

#[test]
fn strips_extra_quotes_from_path_argument() {
    let dir = setup_fixture();
    let quoted = format!("\"{}\"", fixture_path(&dir));

    tgrep()
        .args(["--no-index", "--no-heading", "fn main", &quoted])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello.rs"));
}

#[test]
fn missing_path_is_treated_as_no_matches() {
    let dir = setup_fixture();
    let missing = dir
        .path()
        .join("testdata")
        .join("does-not-exist.rs")
        .to_str()
        .unwrap()
        .to_string();

    tgrep()
        .args(["--no-index", "--no-heading", "fn", &missing])
        .assert()
        .code(1)
        .stderr(predicate::str::is_empty());
}

#[test]
fn supports_negative_lookahead_fallback() {
    let dir = setup_fixture();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "hello(?! world)",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "hello(?! there)",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello world"));
}

#[test]
fn files_mode_accepts_single_file_path() {
    let dir = setup_fixture();
    let hello = dir
        .path()
        .join("testdata")
        .join("hello.rs")
        .to_str()
        .unwrap()
        .to_string();

    tgrep()
        .args(["--files", &hello])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello.rs"))
        .stdout(predicate::str::contains("lib.rs").not());
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
            &fixture_path(&dir),
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
            &fixture_path(&dir),
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
            &fixture_path(&dir),
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
            &fixture_path(&dir),
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
            &fixture_path(&dir),
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
            &fixture_path(&dir),
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
            &fixture_path(&dir),
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
            &fixture_path(&dir),
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
            &fixture_path(&dir),
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
            &fixture_path(&dir),
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
            &fixture_path(&dir),
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
            &fixture_path(&dir),
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
        .args(["--no-index", "-L", "fn", &fixture_path(&dir)])
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
            &fixture_path(&dir),
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
        .args(["--no-index", "-L", ".", &fixture_path(&dir)])
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
            &fixture_path(&dir),
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
            &fixture_path(&dir),
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
        .args(["--no-index", "-q", "fn main", &fixture_path(&dir)])
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
            &fixture_path(&dir),
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty());
}

#[test]
fn quiet_long_form() {
    let dir = setup_fixture();
    tgrep()
        .args(["--no-index", "--quiet", "fn", &fixture_path(&dir)])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn quiet_no_stderr_on_match() {
    let dir = setup_fixture();
    tgrep()
        .args(["--no-index", "-q", "fn", &fixture_path(&dir)])
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
        .args(["--no-index", "-q", "-L", "fn main", &fixture_path(&dir)])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn quiet_with_files_without_match_all_match() {
    let dir = setup_fixture();
    // Every file matches "." so -L finds nothing → exit 1
    tgrep()
        .args(["--no-index", "-q", "-L", ".", &fixture_path(&dir)])
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
            &fixture_path(&dir),
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
            &fixture_path(&dir),
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
        .args(["--no-index", "-L", "fn main", &fixture_path(&dir)])
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
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("hello.rs"));
    assert!(stdout.contains("fn main()"));
}

// ─── count-files ────────────────────────────────────────────────────

#[test]
fn count_files_reports_correct_count() {
    let dir = setup_fixture();
    let output = tgrep()
        .args(["count-files", &fixture_path(&dir)])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Fixture has 4 text files: hello.rs, lib.rs, config.toml, notes.txt
    assert_eq!(stdout.trim(), "4");
}

#[test]
fn count_files_stderr_has_details() {
    let dir = setup_fixture();
    let output = tgrep()
        .args(["count-files", &fixture_path(&dir)])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("4 text files"));
    assert!(stderr.contains("binary skipped"));
}

#[test]
fn count_files_skips_binary() {
    let dir = setup_fixture();
    // Add a binary file (by extension)
    fs::write(
        dir.path().join("testdata").join("image.png"),
        b"\x89PNG\r\n",
    )
    .unwrap();
    let output = tgrep()
        .args(["count-files", &fixture_path(&dir)])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Still 4 text files — png is skipped
    assert_eq!(stdout.trim(), "4");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("1 binary skipped"));
}

// ─── --exclude (index) ─────────────────────────────────────────────

/// Create a fixture with subdirectories to test --exclude during indexing.
fn setup_exclude_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("testdata");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("vendor")).unwrap();
    fs::create_dir_all(root.join("third_party")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main() { hello(); }").unwrap();
    fs::write(root.join("vendor/dep.rs"), "fn hello() { dep(); }").unwrap();
    fs::write(root.join("third_party/lib.rs"), "fn hello() { lib(); }").unwrap();
    fs::write(root.join("README.md"), "# hello project").unwrap();
    dir
}

#[test]
fn index_exclude_single_dir() {
    let dir = setup_exclude_fixture();
    let root = dir.path().join("testdata");
    let index_dir = dir.path().join("idx");

    // Build index excluding vendor
    tgrep()
        .args([
            "index",
            root.to_str().unwrap(),
            "--index-path",
            index_dir.to_str().unwrap(),
            "--exclude",
            "vendor",
        ])
        .assert()
        .success();

    // Search the index for "hello" — should find src/main.rs and third_party/lib.rs
    // but not vendor/dep.rs
    let output = tgrep()
        .args([
            "hello",
            root.to_str().unwrap(),
            "--index-path",
            index_dir.to_str().unwrap(),
            "-l",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("src/main.rs") || stdout.contains("src\\main.rs"));
    assert!(stdout.contains("third_party/lib.rs") || stdout.contains("third_party\\lib.rs"));
    assert!(!stdout.contains("vendor"));
}

#[test]
fn index_exclude_multiple_dirs() {
    let dir = setup_exclude_fixture();
    let root = dir.path().join("testdata");
    let index_dir = dir.path().join("idx");

    // Build index excluding both vendor and third_party
    tgrep()
        .args([
            "index",
            root.to_str().unwrap(),
            "--index-path",
            index_dir.to_str().unwrap(),
            "--exclude",
            "vendor",
            "--exclude",
            "third_party",
        ])
        .assert()
        .success();

    // Search the index for "hello" — should only find src/main.rs and README.md
    let output = tgrep()
        .args([
            "hello",
            root.to_str().unwrap(),
            "--index-path",
            index_dir.to_str().unwrap(),
            "-l",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("src/main.rs") || stdout.contains("src\\main.rs"));
    assert!(!stdout.contains("vendor"));
    assert!(!stdout.contains("third_party"));
}

// ─── serve lock ─────────────────────────────────────────────────────

#[test]
fn serve_rejects_second_server_on_same_index() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("testdata");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("hello.txt"), "hello world").unwrap();

    let index_dir = dir.path().join("idx");

    // Start first server in background using std::process::Command
    let tgrep_bin = assert_cmd::cargo::cargo_bin("tgrep");
    let mut server1 = std::process::Command::new(&tgrep_bin)
        .args([
            "serve",
            root.to_str().unwrap(),
            "--index-path",
            index_dir.to_str().unwrap(),
            "--no-watch",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Wait for server1 to be ready (serve.json written)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !index_dir.join("serve.json").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "server1 did not start in time"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Try to start second server on the same index — should fail
    tgrep()
        .args([
            "serve",
            root.to_str().unwrap(),
            "--index-path",
            index_dir.to_str().unwrap(),
            "--no-watch",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "another tgrep server is already running",
        ));

    // Clean up: kill server1
    server1.kill().ok();
    server1.wait().ok();
}
