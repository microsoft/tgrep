//! Integration tests for ripgrep-compatible CLI flags.
//!
//! Most tests use `--no-index` (brute-force) so no index setup is needed.
//! The `indexed_*` tests at the bottom build a trigram index first and verify
//! that the same search flags produce correct results through the index path.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
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

fn send_rpc_request(port: u16, request: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    writeln!(stream, "{request}")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(response)
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
fn missing_path_reports_error_and_exits_2() {
    let dir = setup_fixture();
    let missing = dir
        .path()
        .join("testdata")
        .join("does-not-exist.rs")
        .to_str()
        .unwrap()
        .to_string();

    // ripgrep reports unreadable paths on stderr and exits 2 rather than
    // silently pretending there were no matches.
    tgrep()
        .args(["--no-index", "--no-heading", "fn", &missing])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("does-not-exist.rs"));
}

#[test]
fn missing_path_stays_quiet_with_no_messages() {
    let dir = setup_fixture();
    let missing = dir
        .path()
        .join("testdata")
        .join("does-not-exist.rs")
        .to_str()
        .unwrap()
        .to_string();

    tgrep()
        .args([
            "--no-index",
            "--no-messages",
            "--no-heading",
            "fn",
            &missing,
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
}

#[test]
fn missing_path_does_not_stop_remaining_paths() {
    let dir = setup_fixture();
    let missing = dir.path().join("nope").to_str().unwrap().to_string();

    // A bad path must not swallow results from the good ones.
    tgrep()
        .args([
            "--no-index",
            "--no-messages",
            "--no-heading",
            "fn main",
            &missing,
            &fixture_path(&dir),
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("hello.rs"));
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

#[test]
fn files_mode_preserves_single_file_relative_path_for_globs() {
    let dir = setup_fixture();

    tgrep()
        .current_dir(dir.path())
        .args(["--files", "-g", "testdata/*", "testdata/hello.rs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("testdata/hello.rs"))
        .stdout(predicate::str::contains("lib.rs").not());
}

#[test]
fn explicit_file_search_bypasses_hidden_walk_filter() {
    let dir = setup_fixture();
    let hidden = dir.path().join("testdata").join(".hidden.rs");
    fs::write(&hidden, "fn hidden_entry() {}\n").unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-H",
            "hidden_entry",
            hidden.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(".hidden.rs"));
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
    // Should have file:content (no line number between them). The fixture path
    // is absolute, so split on the file suffix rather than the first colon.
    for line in stdout.lines() {
        let content = line
            .rsplit_once(".rs:")
            .map(|(_, c)| c)
            .unwrap_or_else(|| panic!("expected file:content, got: {line}"));
        assert!(
            content.starts_with("fn main"),
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
        let content = line.rsplit_once(".rs:").map(|(_, c)| c).unwrap();
        assert!(content.starts_with("fn main"));
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
        .args([
            "--no-index",
            "--files-without-match",
            "fn",
            &fixture_path(&dir),
        ])
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
        .args([
            "--no-index",
            "--files-without-match",
            ".",
            &fixture_path(&dir),
        ])
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
            "--files-without-match",
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
            "--files-without-match",
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
        .args([
            "--no-index",
            "-q",
            "--files-without-match",
            "fn main",
            &fixture_path(&dir),
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
        .args([
            "--no-index",
            "-q",
            "--files-without-match",
            ".",
            &fixture_path(&dir),
        ])
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
        .args([
            "--no-index",
            "--files-without-match",
            "fn main",
            &fixture_path(&dir),
        ])
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

#[test]
fn serve_rebuilds_when_existing_index_is_corrupted() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("testdata");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("hello.txt"), "hello world").unwrap();

    let index_dir = dir.path().join("idx");
    fs::create_dir_all(&index_dir).unwrap();
    fs::write(index_dir.join("lookup.bin"), vec![0u8; 15]).unwrap();
    fs::write(index_dir.join("index.bin"), vec![0u8; 6]).unwrap();
    fs::write(index_dir.join("files.bin"), b"").unwrap();

    let tgrep_bin = assert_cmd::cargo::cargo_bin("tgrep");
    let mut server = std::process::Command::new(&tgrep_bin)
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

    let serve_json = index_dir.join("serve.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    let port = loop {
        if let Some(status) = server.try_wait().unwrap() {
            panic!("server exited before recovering corrupted index: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "server did not start after corrupted index recovery"
        );
        if let Ok(data) = fs::read_to_string(&serve_json)
            && let Ok(info) = serde_json::from_str::<serde_json::Value>(&data)
            && let Some(port) = info.get("port").and_then(|v| v.as_u64())
            && TcpStream::connect(format!("127.0.0.1:{port}")).is_ok()
        {
            break port as u16;
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "server did not finish rebuilding corrupted index"
        );
        let response = send_rpc_request(port, r#"{"jsonrpc":"2.0","method":"status","id":0}"#)
            .expect("status request failed");
        let status: serde_json::Value =
            serde_json::from_str(&response).expect("invalid status JSON");
        let indexing = status
            .pointer("/result/indexing")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if !indexing {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let search_request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "search",
        "id": 1,
        "params": { "pattern": "hello" }
    })
    .to_string();
    let response = send_rpc_request(port, &search_request).expect("search request failed");
    let search: serde_json::Value = serde_json::from_str(&response).expect("invalid search JSON");
    assert!(
        search.get("error").is_none(),
        "search returned error: {search}"
    );
    assert_eq!(
        search
            .pointer("/result/num_matches")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert!(
        search
            .pointer("/result/matches")
            .and_then(|v| v.as_array())
            .is_some_and(|matches| matches.iter().any(|m| m
                .get("file")
                .and_then(|v| v.as_str())
                .is_some_and(|path| path == "hello.txt"))),
        "expected rebuilt index to find hello.txt, got: {search}"
    );

    server.kill().ok();
    let output = server.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("existing index failed to load")
            && stderr.contains("rebuilding in background"),
        "expected corrupted-index recovery trace, got: {stderr}"
    );
}

// ─── Case-insensitive matching (-i / --ignore-case) ─────────────────

#[test]
fn case_insensitive_short_flag() {
    let dir = setup_fixture();
    // "FN" should not match without -i; should match with -i
    tgrep()
        .args(["--no-index", "--no-heading", "FN", &fixture_path(&dir)])
        .assert()
        .code(1);

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-i",
            "FN",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("fn main"))
        .stdout(predicate::str::contains("fn add"));
}

#[test]
fn case_insensitive_long_flag() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--ignore-case",
            "HELLO",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello world"));
}

// ─── Smart case (-S / --smart-case) ─────────────────────────────────

#[test]
fn smart_case_all_lowercase_is_insensitive() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    // Add a file with uppercase "FN" so we can verify smart-case actually
    // enables case-insensitive matching (lowercase pattern matches uppercase text).
    fs::write(sub.join("upper.rs"), "FN UPPER() {}\n").unwrap();

    // All-lowercase pattern → smart-case triggers case-insensitive mode,
    // so "fn" should match both lowercase "fn" and uppercase "FN".
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-S",
            "fn",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn main"));
    assert!(stdout.contains("fn add"));
    assert!(
        stdout.contains("FN UPPER"),
        "smart-case should match uppercase FN with lowercase pattern, got: {stdout}"
    );
}

#[test]
fn smart_case_with_uppercase_is_sensitive() {
    let dir = setup_fixture();
    // Pattern has uppercase → case-sensitive; "FN" won't match "fn"
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-S",
            "FN",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);
}

#[test]
fn smart_case_long_flag() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--smart-case",
            "fn",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("fn main"));
}

// ─── Fixed strings (-F / --fixed-strings) ───────────────────────────

#[test]
fn fixed_strings_short_flag() {
    let dir = setup_fixture();
    // "i32" is also valid regex, but let's test that regex metacharacters
    // are treated literally with -F.
    // "(a" is invalid regex but valid literal
    let sub = dir.path().join("testdata");
    fs::write(sub.join("parens.txt"), "call(a, b)\nno match\n").unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-F",
            "(a",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("call(a, b)"));
}

#[test]
fn fixed_strings_long_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(sub.join("special.txt"), "price is $10.00\nother line\n").unwrap();

    // "$10.00" contains regex metacharacters ($ and .)
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--fixed-strings",
            "$10.00",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("price is $10.00"));
}

#[test]
fn fixed_strings_dot_not_wildcard() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(sub.join("dots.txt"), "a.b\nacb\naXb\n").unwrap();

    // Without -F, "a.b" matches "acb" and "aXb" too (. is wildcard)
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "a.b",
            sub.join("dots.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("a.b"));
    assert!(stdout.contains("acb"));

    // With -F, only literal "a.b" matches
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-F",
            "a.b",
            sub.join("dots.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("a.b"));
    assert!(!stdout.contains("acb"));
    assert!(!stdout.contains("aXb"));
}

// ─── Word boundary (-w / --word-regexp) ─────────────────────────────

#[test]
fn word_regexp_short_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(
        sub.join("words.txt"),
        "add the numbers\nadditional info\nadd\n",
    )
    .unwrap();

    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-w",
            "add",
            sub.join("words.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("add the numbers"));
    assert!(!stdout.contains("additional"));
}

#[test]
fn word_regexp_long_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(sub.join("words2.txt"), "main function\nremainly\nmain\n").unwrap();

    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--word-regexp",
            "main",
            sub.join("words2.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("main function"));
    assert!(stdout.contains("main"));
    assert!(!stdout.contains("remainly"));
}

// ─── Invert match (-v / --invert-match) ─────────────────────────────

#[test]
fn invert_match_short_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-v",
            "fn",
            sub.join("hello.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Lines NOT containing "fn" should be printed
    assert!(stdout.contains("println!"));
    assert!(stdout.contains("}"));
    assert!(!stdout.contains("fn main"));
}

#[test]
fn invert_match_long_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--invert-match",
            "fn",
            sub.join("lib.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("a + b"));
    assert!(!stdout.contains("fn add"));
}

// ─── Only matching (-o / --only-matching) ───────────────────────────

#[test]
fn only_matching_short_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "-o",
            "fn [a-z]+",
            sub.join("hello.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn main"));
    // Should NOT print the full line including "() {"
    assert!(!stdout.contains("() {"));
}

#[test]
fn only_matching_long_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "--only-matching",
            r"\d+\.\d+\.\d+",
            sub.join("config.toml").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "0.1.0");
}

// ─── Max count (-m / --max-count) ───────────────────────────────────

#[test]
fn max_count_limits_matches_per_file() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(
        sub.join("many.txt"),
        "line1 match\nline2 match\nline3 match\nline4 match\n",
    )
    .unwrap();

    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "-m",
            "2",
            "match",
            sub.join("many.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let match_lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(match_lines.len(), 2, "expected 2 matches, got: {stdout}");
}

#[test]
fn max_count_long_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(sub.join("many2.txt"), "a\nb\nc\nd\ne\n").unwrap();

    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "--max-count",
            "1",
            ".",
            sub.join("many2.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.lines().count(), 1);
}

// ─── Count (-c / --count) ──────────────────────────────────────────

#[test]
fn count_short_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-c",
            "fn",
            sub.join("hello.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // hello.rs has 1 line with "fn"
    assert!(stdout.contains("1"), "expected count of 1, got: {stdout}");
}

#[test]
fn count_long_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--count",
            ".",
            sub.join("notes.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // notes.txt has 3 non-empty lines
    assert!(stdout.contains("3"), "expected count of 3, got: {stdout}");
}

// ─── Files with matches (-l / --files-with-matches) ─────────────────

#[test]
fn files_with_matches_short_flag() {
    let dir = setup_fixture();
    let output = tgrep()
        .args(["--no-index", "-l", "fn", &fixture_path(&dir)])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello.rs"));
    assert!(stdout.contains("lib.rs"));
    assert!(!stdout.contains("config.toml"));
    assert!(!stdout.contains("notes.txt"));
}

#[test]
fn files_with_matches_long_flag() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--files-with-matches",
            "fn main",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello.rs"));
    assert!(!stdout.contains("lib.rs"));
}

// ─── Multiple patterns (-e / --regexp) ──────────────────────────────

#[test]
fn multiple_patterns_with_e_flag() {
    let dir = setup_fixture();
    // With -e in play, every positional is a path, so both patterns need -e.
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-e",
            "fn add",
            "-e",
            "fn main",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn main"));
    assert!(stdout.contains("fn add"));
}

#[test]
fn multiple_patterns_long_flag() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--regexp",
            "version",
            "--regexp",
            "hello",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello world"));
    assert!(stdout.contains("version"));
}

// ─── Pattern file (-f / --file) ─────────────────────────────────────

#[test]
fn pattern_file_short_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let pattern_file = sub.join("patterns.txt");
    // -f also makes every positional a path, so both patterns come from here.
    fs::write(&pattern_file, "version\nfn main\n").unwrap();

    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-f",
            pattern_file.to_str().unwrap(),
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn main"));
    assert!(stdout.contains("version"));
}

// ─── Context lines (-A, -B, -C) ────────────────────────────────────

#[test]
fn after_context_short_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "-A",
            "1",
            "fn main",
            sub.join("hello.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn main() {"));
    assert!(stdout.contains("println!"));
}

#[test]
fn before_context_short_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "-B",
            "1",
            "println",
            sub.join("hello.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn main() {"));
    assert!(stdout.contains("println!"));
}

#[test]
fn context_short_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "-C",
            "1",
            "println",
            sub.join("hello.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should have before and after context
    assert!(stdout.contains("fn main() {"));
    assert!(stdout.contains("println!"));
    assert!(stdout.contains("}"));
}

#[test]
fn after_context_long_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "--after-context",
            "2",
            "fn main",
            sub.join("hello.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn main() {"));
    assert!(stdout.contains("println!"));
    assert!(stdout.contains("}"));
}

#[test]
fn context_separator_between_disjoint_matches() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(
        sub.join("separated.txt"),
        "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\n",
    )
    .unwrap();

    // Use -C 1 so context mode is triggered; matches at lines 1 and 6 are
    // far enough apart that a "--" separator should appear between groups.
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "-C",
            "1",
            "alpha|foxtrot",
            sub.join("separated.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // There should be a "--" separator between the two disjoint match groups
    assert!(
        stdout.contains("--"),
        "expected context separator, got: {stdout}"
    );
    assert!(stdout.contains("alpha"));
    assert!(stdout.contains("foxtrot"));
}

// ─── JSON output (--json) ──────────────────────────────────────────

#[test]
fn json_output_flag() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--json",
            "fn main",
            sub.join("hello.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // ripgrep emits a begin/match.../end envelope per file, then a summary.
    let msgs: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|_| panic!("invalid JSON line: {line}"))
        })
        .collect();
    let kinds: Vec<&str> = msgs.iter().map(|m| m["type"].as_str().unwrap()).collect();
    assert_eq!(kinds.first(), Some(&"begin"), "stream: {kinds:?}");
    assert_eq!(kinds.last(), Some(&"summary"), "stream: {kinds:?}");
    assert!(kinds.contains(&"match"), "stream: {kinds:?}");
    assert!(kinds.contains(&"end"), "stream: {kinds:?}");

    for m in msgs.iter().filter(|m| m["type"] == "match") {
        assert!(m["data"]["line_number"].is_number());
        assert!(m["data"]["lines"]["text"].is_string());
        assert!(m["data"]["absolute_offset"].is_number());
        assert!(m["data"]["submatches"].is_array());
    }
}

#[test]
fn json_output_includes_file_and_line() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--json",
            "fn main",
            sub.join("hello.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_match: serde_json::Value = stdout
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|m| m["type"] == "match")
        .expect("expected a match message");

    assert_eq!(first_match["data"]["line_number"], 1);
    let text = first_match["data"]["lines"]["text"].as_str().unwrap();
    assert!(text.contains("fn main"));
    // ripgrep includes the trailing newline in `lines.text`.
    assert!(text.ends_with('\n'), "expected trailing newline: {text:?}");

    let file = first_match["data"]["path"]["text"].as_str().unwrap();
    assert!(
        file.contains("hello.rs"),
        "expected hello.rs in path, got: {file}"
    );

    let submatches = first_match["data"]["submatches"].as_array().unwrap();
    assert_eq!(submatches.len(), 1);
    assert_eq!(submatches[0]["match"]["text"], "fn main");
    assert!(submatches[0]["start"].is_number());
    assert!(submatches[0]["end"].is_number());
}

#[test]
fn json_context_lines_have_context_type() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--json",
            "-A",
            "1",
            "fn main",
            sub.join("hello.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let kinds: Vec<String> = stdout
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(kinds[0], "begin", "stream: {kinds:?}");
    assert_eq!(kinds[1], "match", "stream: {kinds:?}");
    assert_eq!(kinds[2], "context", "stream: {kinds:?}");
}

// ─── Vimgrep output (--vimgrep) ────────────────────────────────────

#[test]
fn vimgrep_output_format() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    // Use current_dir so output paths are relative (avoids Windows C: colon issues)
    let output = tgrep()
        .current_dir(&sub)
        .args(["--no-index", "--vimgrep", "fn main", "hello.rs"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Vimgrep format: file:line:col:content
    for line in stdout.lines() {
        assert!(
            line.contains("hello.rs"),
            "expected filename in vimgrep output, got: {line}"
        );
        // Extract line:col:content after the filename
        let after_file = line
            .strip_prefix("hello.rs:")
            .expect("expected hello.rs: prefix");
        let parts: Vec<&str> = after_file.splitn(3, ':').collect();
        assert_eq!(
            parts.len(),
            3,
            "expected line:col:content after filename, got: {after_file}"
        );
        assert!(
            parts[0].parse::<usize>().is_ok(),
            "expected line number, got: {}",
            parts[0]
        );
        assert!(
            parts[1].parse::<usize>().is_ok(),
            "expected column number, got: {}",
            parts[1]
        );
    }
}

#[test]
fn vimgrep_column_is_one_based() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(sub.join("col.txt"), "hello world\n").unwrap();

    // Use current_dir so output paths are relative
    let output = tgrep()
        .current_dir(&sub)
        .args(["--no-index", "--vimgrep", "world", "col.txt"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap();
    let after_file = line
        .strip_prefix("col.txt:")
        .expect("expected col.txt: prefix");
    let parts: Vec<&str> = after_file.splitn(3, ':').collect();
    // "world" starts at column 7 (1-based)
    assert_eq!(parts[1], "7", "expected column 7, got: {}", parts[1]);
}

// ─── Trim (--trim) ────────────────────────────────────────────────

#[test]
fn trim_strips_whitespace() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(
        sub.join("indented.txt"),
        "    indented line\n  another indented\n",
    )
    .unwrap();

    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "--trim",
            "indented",
            sub.join("indented.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        assert!(
            !line.starts_with(' '),
            "expected trimmed line, got: '{line}'"
        );
    }
    assert!(stdout.contains("indented line"));
    assert!(stdout.contains("another indented"));
}

// ─── Null separator (-0 / --null) ──────────────────────────────────

#[test]
fn null_separator_in_files_mode() {
    let dir = setup_fixture();
    let output = tgrep()
        .args(["--no-index", "-l", "-0", "fn", &fixture_path(&dir)])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Files should be separated by NUL bytes
    assert!(
        stdout.contains('\0'),
        "expected NUL separator in output, got: {stdout}"
    );
    // And should NOT have newlines as separators between filenames
    let filenames: Vec<&str> = stdout.split('\0').filter(|s| !s.is_empty()).collect();
    assert!(
        filenames.len() >= 2,
        "expected at least 2 files, got: {filenames:?}"
    );
}

#[test]
fn null_separator_long_flag() {
    let dir = setup_fixture();
    let output = tgrep()
        .args(["--no-index", "-l", "--null", "fn", &fixture_path(&dir)])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains('\0'));
}

// ─── Heading (--heading / --no-heading) ─────────────────────────────

#[test]
fn no_heading_outputs_flat_format() {
    let dir = setup_fixture();
    // Run from inside the fixture so printed paths are relative. An absolute
    // path would carry a drive-letter colon on Windows and silently supply the
    // extra field this test is looking for.
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .current_dir(&sub)
        .args(["--no-index", "--no-heading", "fn", "hello.rs", "lib.rs"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty(), "expected matches");
    // Flat format prefixes every match with its filename rather than printing
    // the name once as a heading. ripgrep omits line numbers when stdout is not
    // a terminal and `-n` was not asked for, so the shape here is file:content.
    for line in stdout.lines() {
        let (file, content) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("expected file:content in flat mode, got: {line}"));
        assert!(
            file == "hello.rs" || file == "lib.rs",
            "expected a filename prefix on every line, got: {line}"
        );
        assert!(
            content.contains("fn"),
            "expected the matching content after the filename, got: {line}"
        );
    }
}

#[test]
fn no_heading_with_line_numbers_is_file_line_content() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    // Two file arguments: naming exactly one file would suppress the filename,
    // leaving line:content.
    let output = tgrep()
        .current_dir(&sub)
        .args([
            "--no-index",
            "--no-heading",
            "-n",
            "fn",
            "hello.rs",
            "lib.rs",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty(), "expected matches");
    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        assert_eq!(
            parts.len(),
            3,
            "expected file:line:content once -n is given, got: {line}"
        );
        assert!(
            parts[0] == "hello.rs" || parts[0] == "lib.rs",
            "expected a filename prefix, got: {line}"
        );
        assert!(
            parts[1].parse::<usize>().is_ok(),
            "expected a line number, got: {}",
            parts[1]
        );
    }
}

// ─── Color (--color never) ─────────────────────────────────────────

#[test]
fn color_never_has_no_ansi_codes() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--color",
            "never",
            "fn",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("\x1b["),
        "expected no ANSI codes with --color never"
    );
}

// ─── Hidden files (--hidden) ───────────────────────────────────────

#[test]
fn hidden_flag_includes_dotfiles() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(sub.join(".hidden_file.txt"), "secret hidden content\n").unwrap();

    // Without --hidden, dot-files should be skipped
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "secret hidden",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);

    // With --hidden, dot-files should be found
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--hidden",
            "secret hidden",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("secret hidden content"));
}

// ─── No-ignore (--no-ignore) ───────────────────────────────────────

#[test]
fn no_ignore_includes_gitignored_files() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");

    // Initialize a git repo so .gitignore is respected
    let git_status = std::process::Command::new("git")
        .args(["init"])
        .current_dir(&sub)
        .output();
    let git_ok = git_status
        .as_ref()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !git_ok {
        eprintln!("skipping no_ignore test: git init failed or git not available");
        return;
    }

    fs::write(sub.join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(sub.join("ignored.txt"), "this is ignored content\n").unwrap();

    // Without --no-ignore, the gitignored file should be skipped
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "ignored content",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);

    // With --no-ignore, the gitignored file should be found
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-ignore",
            "ignored content",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("this is ignored content"));
}

// ─── Unrestricted (-u) ─────────────────────────────────────────────

#[test]
fn unrestricted_single_u_is_no_ignore() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");

    let git_status = std::process::Command::new("git")
        .args(["init"])
        .current_dir(&sub)
        .output();
    let git_ok = git_status
        .as_ref()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !git_ok {
        eprintln!("skipping unrestricted_single_u test: git init failed or git not available");
        return;
    }

    fs::write(sub.join(".gitignore"), "secret.txt\n").unwrap();
    fs::write(sub.join("secret.txt"), "unrestricted secret\n").unwrap();

    // -u = --no-ignore
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-u",
            "unrestricted secret",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("unrestricted secret"));
}

#[test]
fn unrestricted_double_u_includes_hidden() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(sub.join(".very_hidden.txt"), "double u content\n").unwrap();

    // -uu = --no-ignore + --hidden
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-uu",
            "double u content",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("double u content"));
}

// ─── Glob negation (!pattern) ──────────────────────────────────────

#[test]
fn glob_negation_excludes_files() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-g",
            "!*.toml",
            ".",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("config.toml"));
    // Other files should still appear
    assert!(
        stdout.contains("hello.rs") || stdout.contains("lib.rs") || stdout.contains("notes.txt")
    );
}

// ─── Combined flag interactions ────────────────────────────────────

#[test]
fn case_insensitive_with_fixed_strings() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(
        sub.join("mixed.txt"),
        "Hello World\nhello world\nHELLO WORLD\n",
    )
    .unwrap();

    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "-i",
            "-F",
            "hello world",
            sub.join("mixed.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.lines().count(),
        3,
        "expected 3 case-insensitive matches, got: {stdout}"
    );
}

#[test]
fn word_regexp_with_case_insensitive() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(
        sub.join("wordcase.txt"),
        "Add numbers\nadditional\nADD\nadd\n",
    )
    .unwrap();

    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "-w",
            "-i",
            "add",
            sub.join("wordcase.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Add numbers"));
    assert!(stdout.contains("ADD"));
    assert!(stdout.contains("add"));
    assert!(!stdout.contains("additional"));
}

#[test]
fn invert_match_with_count() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    // hello.rs has 3 lines, 1 with "fn"
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-v",
            "-c",
            "fn",
            sub.join("hello.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // 3 total lines - 1 with fn = 2 non-matching
    assert!(
        stdout.contains("2"),
        "expected 2 inverted matches, got: {stdout}"
    );
}

#[test]
fn only_matching_with_count() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    // -o -c: count should still reflect number of matching lines
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-o",
            "-c",
            "fn",
            sub.join("hello.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1"));
}

#[test]
fn max_count_with_files_only() {
    let dir = setup_fixture();
    // -m 1 -l: should still list all files that have at least 1 match
    let output = tgrep()
        .args(["--no-index", "-m", "1", "-l", "fn", &fixture_path(&dir)])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello.rs"));
    assert!(stdout.contains("lib.rs"));
}

#[test]
fn json_output_with_context() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .args([
            "--no-index",
            "--json",
            "-A",
            "1",
            "fn main",
            sub.join("hello.rs").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut has_match = false;
    let mut has_context = false;
    for line in stdout.lines() {
        let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
        match parsed["type"].as_str().unwrap() {
            "match" => has_match = true,
            "context" => has_context = true,
            "begin" | "end" | "summary" => {}
            other => panic!("unexpected type: {other}"),
        }
    }
    assert!(has_match, "expected at least one match");
    assert!(has_context, "expected at least one context line");
}

#[test]
fn glob_with_invert_match_and_count() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-g",
            "*.rs",
            "-v",
            "-c",
            "fn",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should only count in .rs files
    assert!(!stdout.contains("config.toml"));
    assert!(!stdout.contains("notes.txt"));
}

// ─── Exit codes ────────────────────────────────────────────────────

#[test]
fn exit_code_zero_on_match() {
    let dir = setup_fixture();
    tgrep()
        .args(["--no-index", "fn main", &fixture_path(&dir)])
        .assert()
        .success();
}

#[test]
fn exit_code_one_on_no_match() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "zzz_definitely_no_match_zzz",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);
}

#[test]
fn invert_match_exit_code() {
    let dir = setup_fixture();
    // Every file has at least one line not matching "fn main"
    tgrep()
        .args(["--no-index", "-v", "fn main", &fixture_path(&dir)])
        .assert()
        .success();
}

// ─── Edge cases ────────────────────────────────────────────────────

#[test]
fn empty_file_no_matches() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(sub.join("empty.txt"), "").unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "anything",
            sub.join("empty.txt").to_str().unwrap(),
        ])
        .assert()
        .code(1);
}

#[test]
fn single_line_file() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(sub.join("single.txt"), "only one line").unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "only one",
            sub.join("single.txt").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("only one line"));
}

#[test]
fn regex_alternation() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-l",
            "hello|version",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello.rs"));
    assert!(stdout.contains("config.toml"));
}

#[test]
fn multiple_patterns_with_fixed_strings() {
    let dir = setup_fixture();
    let sub = dir.path().join("testdata");
    fs::write(sub.join("special2.txt"), "a+b\nc+d\na*b\n").unwrap();

    // -F with both patterns via -e, since -e makes positionals paths.
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "-F",
            "-e",
            "c+d",
            "-e",
            "a+b",
            sub.join("special2.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("a+b"));
    assert!(stdout.contains("c+d"));
    assert!(!stdout.contains("a*b"));
}

#[test]
fn files_without_match_with_invert_match() {
    let dir = setup_fixture();
    // -v -L: files where ALL lines match the pattern (no non-matching lines)
    // Using "." which matches every non-empty line; files without match on -v
    // means files where every line matches "."
    let output = tgrep()
        .args([
            "--no-index",
            "-v",
            "--files-without-match",
            ".",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    // All our test files have content on every line, so -v "." finds no
    // non-matching lines, meaning no file "matches" inverted, so -L
    // lists them all.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello.rs"));
    assert!(stdout.contains("lib.rs"));
}

// ═══════════════════════════════════════════════════════════════════════
// Indexed search tests
//
// These tests build a trigram index first and then run searches through
// the index path (no --no-index) to ensure that the same ripgrep-compatible
// flags work correctly when the index is used for file discovery.
// ═══════════════════════════════════════════════════════════════════════

/// Create a fixture with richer content for indexed tests (trigrams need
/// at least 3-char sequences to be useful).
fn setup_indexed_fixture() -> (TempDir, String) {
    let dir = TempDir::new().unwrap();
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

    let index_dir = dir.path().join("idx");
    let root = sub.to_str().unwrap().to_string();

    // Build the index
    tgrep()
        .args(["index", &root, "--index-path", index_dir.to_str().unwrap()])
        .assert()
        .success();

    (dir, index_dir.to_str().unwrap().to_string())
}

/// Helper: get path to testdata inside the indexed fixture.
fn indexed_fixture_path(dir: &TempDir) -> String {
    dir.path().join("testdata").to_str().unwrap().to_string()
}

#[test]
fn indexed_basic_search() {
    let (dir, idx) = setup_indexed_fixture();
    tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "fn main",
            &indexed_fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello.rs"));
}

#[test]
fn indexed_case_insensitive() {
    let (dir, idx) = setup_indexed_fixture();
    // "FN" should not match without -i
    tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "FN MAIN",
            &indexed_fixture_path(&dir),
        ])
        .assert()
        .code(1);

    // With -i it should match
    tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "-i",
            "FN MAIN",
            &indexed_fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("fn main"));
}

#[test]
fn indexed_fixed_strings() {
    let (dir, idx) = setup_indexed_fixture();
    let sub = dir.path().join("testdata");
    fs::write(sub.join("special.txt"), "price is $10.00\nother line\n").unwrap();

    // Rebuild index with the new file
    tgrep()
        .args(["index", &indexed_fixture_path(&dir), "--index-path", &idx])
        .assert()
        .success();

    tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "-F",
            "$10.00",
            &indexed_fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("price is $10.00"));
}

#[test]
fn indexed_word_boundary() {
    let (dir, idx) = setup_indexed_fixture();
    let sub = dir.path().join("testdata");
    fs::write(
        sub.join("words.txt"),
        "add the numbers\nadditional info\nadd\n",
    )
    .unwrap();

    tgrep()
        .args(["index", &indexed_fixture_path(&dir), "--index-path", &idx])
        .assert()
        .success();

    let output = tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "-w",
            "add",
            &indexed_fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("add the numbers"));
    assert!(!stdout.contains("additional"));
}

#[test]
fn indexed_invert_match() {
    let (dir, idx) = setup_indexed_fixture();
    // Search the directory but filter to hello.rs via glob
    let output = tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "-v",
            "-g",
            "hello.rs",
            "fn",
            &indexed_fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("println!"));
    assert!(!stdout.contains("fn main"));
}

#[test]
fn indexed_files_with_matches() {
    let (dir, idx) = setup_indexed_fixture();
    let output = tgrep()
        .args([
            "--index-path",
            &idx,
            "-l",
            "fn",
            &indexed_fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello.rs"));
    assert!(stdout.contains("lib.rs"));
    assert!(!stdout.contains("config.toml"));
    assert!(!stdout.contains("notes.txt"));
}

#[test]
fn indexed_files_without_match() {
    let (dir, idx) = setup_indexed_fixture();
    let output = tgrep()
        .args([
            "--index-path",
            &idx,
            "--files-without-match",
            "fn",
            &indexed_fixture_path(&dir),
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
fn indexed_count() {
    let (dir, idx) = setup_indexed_fixture();
    let output = tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "-c",
            "fn main",
            &indexed_fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Only hello.rs has "fn main" (1 match)
    assert!(
        stdout.contains(":1"),
        "expected count of 1 for fn main, got: {stdout}"
    );
}

#[test]
fn indexed_quiet_exit_codes() {
    let (dir, idx) = setup_indexed_fixture();
    // Match → exit 0
    tgrep()
        .args([
            "--index-path",
            &idx,
            "-q",
            "fn main",
            &indexed_fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    // No match → exit 1
    tgrep()
        .args([
            "--index-path",
            &idx,
            "-q",
            "zzz_nonexistent_zzz",
            &indexed_fixture_path(&dir),
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty());
}

#[test]
fn indexed_glob_filter() {
    let (dir, idx) = setup_indexed_fixture();
    let output = tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "-g",
            "*.rs",
            "fn",
            &indexed_fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello.rs"));
    assert!(stdout.contains("lib.rs"));
    assert!(!stdout.contains("config.toml"));
}

#[test]
fn indexed_only_matching() {
    let (dir, idx) = setup_indexed_fixture();
    let output = tgrep()
        .args([
            "--no-heading",
            "--no-filename",
            "-N",
            "--index-path",
            &idx,
            "-o",
            "-g",
            "hello.rs",
            "fn [a-z]+",
            &indexed_fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn main"));
    assert!(!stdout.contains("() {"));
}

#[test]
fn indexed_max_count() {
    let (dir, idx) = setup_indexed_fixture();
    let sub = dir.path().join("testdata");
    fs::write(
        sub.join("many.txt"),
        "line1 match\nline2 match\nline3 match\nline4 match\n",
    )
    .unwrap();

    tgrep()
        .args(["index", &indexed_fixture_path(&dir), "--index-path", &idx])
        .assert()
        .success();

    let output = tgrep()
        .args([
            "--no-heading",
            "--no-filename",
            "-N",
            "--index-path",
            &idx,
            "-m",
            "2",
            "-g",
            "many.txt",
            "match",
            &indexed_fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.lines().count(),
        2,
        "expected 2 matches, got: {stdout}"
    );
}

#[test]
fn indexed_context_lines() {
    let (dir, idx) = setup_indexed_fixture();
    let output = tgrep()
        .args([
            "--no-heading",
            "--no-filename",
            "-N",
            "--index-path",
            &idx,
            "-A",
            "1",
            "-g",
            "hello.rs",
            "fn main",
            &indexed_fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn main() {"));
    assert!(stdout.contains("println!"));
}

#[test]
fn indexed_json_output() {
    let (dir, idx) = setup_indexed_fixture();
    let output = tgrep()
        .args([
            "--index-path",
            &idx,
            "--json",
            "-g",
            "hello.rs",
            "fn main",
            &indexed_fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut saw_match = false;
    for line in stdout.lines() {
        let parsed: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|_| panic!("invalid JSON: {line}"));
        if parsed["type"] == "match" {
            saw_match = true;
            assert!(parsed["data"]["line_number"].is_number());
            assert!(
                parsed["data"]["lines"]["text"]
                    .as_str()
                    .unwrap()
                    .contains("fn main")
            );
        }
    }
    assert!(saw_match, "expected a match message in: {stdout}");
}

#[test]
fn indexed_smart_case() {
    let (dir, idx) = setup_indexed_fixture();
    // All-lowercase pattern → case-insensitive (matches "fn main")
    tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "-S",
            "fn main",
            &indexed_fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("fn main"));

    // Pattern with uppercase → case-sensitive (no match)
    tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "-S",
            "FN MAIN",
            &indexed_fixture_path(&dir),
        ])
        .assert()
        .code(1);
}

#[test]
fn indexed_no_filename_no_line_number() {
    let (dir, idx) = setup_indexed_fixture();
    let output = tgrep()
        .args([
            "--no-heading",
            "--no-filename",
            "-N",
            "--index-path",
            &idx,
            "-g",
            "hello.rs",
            "fn main",
            &indexed_fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        assert_eq!(line.trim(), "fn main() {");
    }
}

// ─── Large files, binary files and encodings ───────────────────────
//
// These cover cases where tgrep used to silently return no results, which is
// far worse than an error: a search that finds nothing looks like a clean bill
// of health.

/// A file above the old hard-coded 1 MiB walker cap.
fn setup_large_file_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();

    let mut big = String::with_capacity(1_200_000);
    while big.len() < 1_200_000 {
        big.push_str("filler line that is not interesting\n");
    }
    big.push_str("needle_in_a_big_file\n");
    fs::write(sub.join("big.txt"), big).unwrap();
    dir
}

#[test]
fn searches_files_larger_than_one_mib() {
    let dir = setup_large_file_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "needle_in_a_big_file",
            &fixture_path(&dir),
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("needle_in_a_big_file"));
}

#[test]
fn max_filesize_excludes_large_files() {
    let dir = setup_large_file_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--max-filesize",
            "1K",
            "needle_in_a_big_file",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);
}

#[test]
fn max_filesize_accepts_suffixes() {
    let dir = setup_large_file_fixture();
    // 2M is above the 1.2 MB fixture, so the match must still be found.
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--max-filesize",
            "2M",
            "needle_in_a_big_file",
            &fixture_path(&dir),
        ])
        .assert()
        .code(0);
}

#[test]
fn invalid_max_filesize_is_an_error_not_a_silent_fallback() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--max-filesize",
            "notanumber",
            "fn main",
            &fixture_path(&dir),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("max-filesize"));
}

#[test]
fn text_flag_searches_binary_files() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("blob.bin"), b"prefix\x00binary_needle\x00suffix").unwrap();

    // Without -a the file is skipped as binary.
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "binary_needle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);

    // With -a it is searched, and reported without dumping raw bytes.
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-a",
            "binary_needle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("binary_needle"));
}

#[test]
fn unrestricted_uuu_enables_binary_search() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("blob.bin"), b"prefix\x00binary_needle\x00suffix").unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-uuu",
            "binary_needle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(0);
}

#[test]
fn searches_files_that_are_not_valid_utf8() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    // Latin-1 encoded "café" followed by an ASCII marker.
    fs::write(sub.join("latin1.txt"), b"caf\xe9 latin_needle\n").unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "latin_needle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("latin_needle"));
}

// ─── Multiline (-U / --multiline-dotall) ───────────────────────────

fn setup_multiline_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("span.txt"), "alpha\nstart\nmiddle\nend\nomega\n").unwrap();
    dir
}

#[test]
fn multiline_matches_across_lines() {
    let dir = setup_multiline_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-U",
            r"start\nmiddle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("start"))
        .stdout(predicate::str::contains("middle"));
}

#[test]
fn multiline_alone_does_not_make_dot_match_newline() {
    let dir = setup_multiline_fixture();
    // ripgrep requires --multiline-dotall for this; -U alone must not match.
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-U",
            "start.middle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);
}

#[test]
fn multiline_dotall_makes_dot_match_newline() {
    let dir = setup_multiline_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--multiline-dotall",
            "start.middle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("start"));
}

#[test]
fn non_multiline_anchors_per_line() {
    let dir = setup_multiline_fixture();
    // `^middle` must match line 3 even though it is not the start of the file.
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "^middle$",
            &fixture_path(&dir),
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("middle"));
}

// ─── Per-match output (--vimgrep, -o, --color) ─────────────────────

fn setup_repeats_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("repeats.txt"), "foo bar foo baz foo\n").unwrap();
    dir
}

#[test]
fn vimgrep_emits_one_row_per_match() {
    let dir = setup_repeats_fixture();
    let sub = dir.path().join("testdata");
    let output = tgrep()
        .current_dir(&sub)
        .args(["--no-index", "--vimgrep", "foo", "repeats.txt"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let rows: Vec<&str> = stdout.lines().collect();
    assert_eq!(rows.len(), 3, "expected one row per match, got: {stdout}");

    // Columns are 1-based byte offsets of each match: "foo bar foo baz foo".
    let cols: Vec<&str> = rows.iter().map(|r| r.split(':').nth(2).unwrap()).collect();
    assert_eq!(cols, vec!["1", "9", "17"], "rows: {stdout}");
}

#[test]
fn only_matching_prints_one_line_per_match() {
    let dir = setup_repeats_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-o",
            "foo",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let rows: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(rows.len(), 3, "expected 3 output lines, got: {stdout}");
    for row in rows {
        assert!(row.ends_with("foo"), "unexpected row: {row}");
    }
}

#[test]
fn color_always_highlights_the_match() {
    let dir = setup_repeats_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--color",
            "always",
            "bar",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\x1b[1;31mbar\x1b[0m"),
        "expected the match itself to be colored, got: {stdout:?}"
    );
}

#[test]
fn color_never_leaves_output_plain() {
    let dir = setup_repeats_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--color",
            "never",
            "bar",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains('\x1b'), "unexpected escapes: {stdout:?}");
}

#[test]
fn json_submatches_cover_every_match_on_a_line() {
    let dir = setup_repeats_fixture();
    let output = tgrep()
        .args(["--no-index", "--json", "foo", &fixture_path(&dir)])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let m: serde_json::Value = stdout
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|m| m["type"] == "match")
        .expect("expected a match message");
    let subs = m["data"]["submatches"].as_array().unwrap();
    assert_eq!(subs.len(), 3, "expected 3 submatches, got: {subs:?}");
    assert_eq!(subs[0]["start"], 0);
    assert_eq!(subs[1]["start"], 8);
    assert_eq!(subs[2]["start"], 16);
}

// ─── Glob case sensitivity ─────────────────────────────────────────

fn setup_case_glob_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("lower.txt"), "needle\n").unwrap();
    fs::write(sub.join("UPPER.TXT"), "needle\n").unwrap();
    dir
}

#[test]
fn globs_are_case_sensitive_by_default() {
    let dir = setup_case_glob_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-g",
            "*.txt",
            "needle",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("lower.txt"), "got: {stdout}");
    assert!(!stdout.contains("UPPER.TXT"), "got: {stdout}");
}

#[test]
fn iglob_matches_case_insensitively() {
    let dir = setup_case_glob_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--iglob",
            "*.txt",
            "needle",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("lower.txt"), "got: {stdout}");
    assert!(stdout.contains("UPPER.TXT"), "got: {stdout}");
}

#[test]
fn glob_case_insensitive_flag_applies_to_globs() {
    let dir = setup_case_glob_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--glob-case-insensitive",
            "-g",
            "*.txt",
            "needle",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("UPPER.TXT"), "got: {stdout}");
}

// ─── Newly added ripgrep short flags ───────────────────────────────

#[test]
fn case_sensitive_flag_overrides_smart_case() {
    let dir = setup_fixture();
    // notes.txt contains "Nothing important here." — smart case makes an
    // all-lowercase pattern case-insensitive, so it matches.
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-S",
            "nothing",
            &fixture_path(&dir),
        ])
        .assert()
        .code(0);

    // -s forces case sensitivity back on, so the same pattern no longer matches.
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-S",
            "-s",
            "nothing",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);
}

#[test]
fn ignore_case_still_wins_over_case_sensitive() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-s",
            "-i",
            "HELLO",
            &fixture_path(&dir),
        ])
        .assert()
        .code(0);
}

#[test]
fn capital_i_suppresses_filenames() {
    let dir = setup_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-I",
            "fn main",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("hello.rs"), "got: {stdout}");
    assert!(stdout.contains("fn main"), "got: {stdout}");
}

#[test]
fn dot_flag_is_an_alias_for_hidden() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join(".hidden.txt"), "hidden_needle\n").unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "hidden_needle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-.",
            "hidden_needle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(0);
}

#[test]
fn follow_flag_is_accepted_as_dash_l() {
    let dir = setup_fixture();
    // -L is --follow in ripgrep; it must no longer mean --files-without-match.
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-L",
            "fn main",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("fn main"),
        "-L should follow links, not list non-matching files: {stdout}"
    );
}

// ─── Exit codes ────────────────────────────────────────────────────

#[test]
fn quiet_with_match_exits_zero_even_after_an_error() {
    let dir = setup_fixture();
    let missing = dir.path().join("nope").to_str().unwrap().to_string();

    // ripgrep: `matched && (quiet || !errored)` → 0.
    tgrep()
        .args([
            "--no-index",
            "--no-messages",
            "-q",
            "fn main",
            &fixture_path(&dir),
            &missing,
        ])
        .assert()
        .code(0);
}

// ═══════════════════════════════════════════════════════════════════════
// Regressions found in review
//
// Each of these was a real defect: the first two produced wrong output on
// the local path, the rest made results depend on whether a server or index
// happened to be in play.
// ═══════════════════════════════════════════════════════════════════════

/// Two matching lines with a gap between them, and no context requested.
fn setup_gap_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(
        sub.join("gap.txt"),
        "alpha\nbeta\ngamma\ndelta\nalpha again\n",
    )
    .unwrap();
    dir
}

#[test]
fn no_context_separator_without_context_flags() {
    let dir = setup_gap_fixture();
    let output = tgrep()
        .args(["--no-index", "--no-heading", "alpha", &fixture_path(&dir)])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.lines().any(|l| l.trim() == "--"),
        "`--` must only appear when context is requested, got: {stdout}"
    );
}

#[test]
fn no_context_separator_in_vimgrep_output() {
    let dir = setup_gap_fixture();
    let output = tgrep()
        .args(["--no-index", "--vimgrep", "alpha", &fixture_path(&dir)])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // A bare `--` row breaks quickfix parsing.
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        assert!(
            line.contains(".txt:"),
            "unparseable vimgrep row {line:?} in: {stdout}"
        );
    }
}

#[test]
fn context_separator_still_appears_with_context() {
    let dir = setup_gap_fixture();
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-C",
            "1",
            "alpha",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|l| l.trim() == "--"),
        "expected a `--` separator, got: {stdout}"
    );
}

#[test]
fn max_count_keeps_multiline_matches_whole() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("m.txt"), "start\nmiddle\nend\ntail\n").unwrap();

    let full = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-U",
            r"start[\s\S]*end",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let full = String::from_utf8_lossy(&full.stdout).to_string();

    // --max-count limits matches, not lines: one match must still print whole.
    let capped = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-U",
            "-m",
            "1",
            r"start[\s\S]*end",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let capped = String::from_utf8_lossy(&capped.stdout).to_string();

    assert_eq!(full.lines().count(), 3, "baseline changed: {full}");
    assert_eq!(
        capped, full,
        "-m 1 truncated a single multiline match mid-way"
    );
}

#[test]
fn indexed_max_filesize_is_honored() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    let mut big = String::new();
    while big.len() < 4000 {
        big.push_str("padding padding padding padding\n");
    }
    big.push_str("indexed_size_needle\n");
    fs::write(sub.join("big.txt"), big).unwrap();

    let index_dir = dir.path().join("idx");
    let idx = index_dir.to_str().unwrap().to_string();
    tgrep()
        .args(["index", &fixture_path(&dir), "--index-path", &idx])
        .assert()
        .success();

    // Sanity: found through the index without a limit.
    tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "indexed_size_needle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(0);

    // The flag must apply on the indexed path too, not just --no-index.
    tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "--max-filesize",
            "1K",
            "indexed_size_needle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);
}

#[test]
fn indexed_pattern_file_is_honored() {
    let (dir, idx) = setup_indexed_fixture();
    let pats = dir.path().join("pats.txt");
    fs::write(&pats, "hello world\nzzz_no_such_pattern\n").unwrap();

    // -f patterns must survive the trip through the index/server path.
    tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "-f",
            pats.to_str().unwrap(),
            &indexed_fixture_path(&dir),
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("hello world"));
}

#[test]
fn indexed_extra_patterns_reach_the_query_plan() {
    let (dir, idx) = setup_indexed_fixture();
    // The trigram plan must union every pattern. Narrowing candidates with
    // only the primary pattern hides files that just the -e pattern matches.
    tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "-e",
            "hello world",
            "-e",
            "zzz_no_such_pattern",
            &indexed_fixture_path(&dir),
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("hello world"));
}

#[test]
fn indexed_multiple_patterns_match_the_same_as_brute_force() {
    let (dir, idx) = setup_indexed_fixture();
    let path = indexed_fixture_path(&dir);

    let lines = |extra: &[&str]| -> Vec<String> {
        let mut args: Vec<&str> = vec!["--no-heading"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&[
            "-e",
            "hello world",
            "-e",
            "version",
            "-e",
            "pub fn add",
            &path,
        ]);
        let out = tgrep().args(&args).output().unwrap();
        let mut v: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        v.sort();
        v
    };

    let brute = lines(&["--no-index"]);
    let indexed = lines(&["--index-path", &idx]);

    assert_eq!(brute.len(), 3, "expected 3 matches, got: {brute:?}");
    assert_eq!(brute, indexed, "indexed path missed -e patterns");
}

#[test]
fn indexed_fixed_string_multiple_patterns() {
    let (dir, idx) = setup_indexed_fixture();
    tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "-F",
            "-e",
            "println!(\"hello world\")",
            "-e",
            "a + b",
            &indexed_fixture_path(&dir),
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("hello world"))
        .stdout(predicate::str::contains("a + b"));
}
#[test]
fn indexed_count_ignores_context_lines() {
    let (dir, idx) = setup_indexed_fixture();

    let local = tgrep()
        .args([
            "--no-index",
            "-c",
            "-A",
            "2",
            "fn",
            &indexed_fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let indexed = tgrep()
        .args([
            "--index-path",
            &idx,
            "-c",
            "-A",
            "2",
            "fn",
            &indexed_fixture_path(&dir),
        ])
        .output()
        .unwrap();

    let mut a: Vec<String> = String::from_utf8_lossy(&local.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let mut b: Vec<String> = String::from_utf8_lossy(&indexed.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    a.sort();
    b.sort();
    assert!(!a.is_empty(), "expected some counts");
    assert_eq!(a, b, "-c must count matching lines, not context lines");
}

#[test]
fn indexed_search_reads_non_utf8_files() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("latin1.txt"), b"caf\xe9 indexed_latin_needle\n").unwrap();

    let index_dir = dir.path().join("idx");
    let idx = index_dir.to_str().unwrap().to_string();
    tgrep()
        .args(["index", &fixture_path(&dir), "--index-path", &idx])
        .assert()
        .success();

    tgrep()
        .args([
            "--no-heading",
            "--index-path",
            &idx,
            "indexed_latin_needle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("indexed_latin_needle"));
}

// ─── Second review round: -v candidate soundness and -c on binary files ───

/// `-v` inverts at the *line* level, so a file matches when some line does not
/// match. Filtering candidates by the trigram plan selects exactly the wrong
/// files, so the plan has to be bypassed. Regression: the indexed path used to
/// return only files that *contained* the pattern.
#[test]
fn indexed_invert_match_returns_files_without_the_pattern() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    // Only `has.txt` contains the pattern, so its trigrams are the only ones in
    // the index. `none.txt` must still be reported by `-v`.
    fs::write(sub.join("has.txt"), "zebraqux marker\n").unwrap();
    fs::write(sub.join("none.txt"), "totally unrelated text\n").unwrap();

    let idx = dir.path().join("idx").to_str().unwrap().to_string();
    tgrep()
        .args(["index", &fixture_path(&dir), "--index-path", &idx])
        .assert()
        .success();

    let local = tgrep()
        .args(["--no-index", "-v", "-l", "zebraqux", &fixture_path(&dir)])
        .output()
        .unwrap();
    let indexed = tgrep()
        .args([
            "--index-path",
            &idx,
            "-v",
            "-l",
            "zebraqux",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();

    let mut a: Vec<String> = String::from_utf8_lossy(&local.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let mut b: Vec<String> = String::from_utf8_lossy(&indexed.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    a.sort();
    b.sort();
    assert!(
        a.iter().any(|l| l.contains("none.txt")),
        "brute force should report none.txt, got {a:?}"
    );
    assert_eq!(a, b, "-v must not be narrowed by the trigram plan");
}

/// A file whose NUL byte sits past the 8 KiB binary sniff window is indexed as
/// ordinary text, so it becomes a normal search candidate. ripgrep hides binary
/// files found by traversal but reports a count for one named explicitly, and
/// both search paths must agree on each.
#[test]
fn count_reports_binary_files_with_late_nul() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();

    let mut content = String::new();
    for i in 0..800 {
        content.push_str(&format!("lateneedle line {i}\n"));
    }
    content.push_str("binary\u{0}marker\n");
    content.push_str("tail lateneedle only\n");
    fs::write(sub.join("big.txt"), &content).unwrap();
    fs::write(sub.join("plain.txt"), "a lateneedle in plain\n").unwrap();

    let idx = dir.path().join("idx").to_str().unwrap().to_string();
    tgrep()
        .args(["index", &fixture_path(&dir), "--index-path", &idx])
        .assert()
        .success();

    let count = |mode: &[&str], target: &str| -> Vec<String> {
        let mut args: Vec<&str> = mode.to_vec();
        args.extend_from_slice(&["-c", "lateneedle", target]);
        let out = tgrep().args(&args).output().unwrap();
        let mut v: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        v.sort();
        v
    };

    // Traversal: the binary file is invisible on both paths.
    let dir_local = count(&["--no-index"], &fixture_path(&dir));
    let dir_indexed = count(&["--index-path", &idx], &fixture_path(&dir));
    assert!(
        !dir_local.iter().any(|l| l.contains("big.txt")),
        "an implicitly found binary file must be hidden, got {dir_local:?}"
    );
    assert_eq!(dir_local, dir_indexed, "-c must agree across search paths");

    // Named explicitly: the count is reported on both paths.
    let big = sub.join("big.txt").to_str().unwrap().to_string();
    let file_local = count(&["--no-index"], &big);
    let file_indexed = count(&["--index-path", &idx], &big);
    assert_eq!(file_local, vec!["801".to_string()], "expected 801 matches");
    assert_eq!(
        file_local, file_indexed,
        "-c must report explicit binary files on both paths"
    );
}

/// Without `-c`, an explicitly named binary file is summarised as a note on
/// both paths, and one found by traversal is hidden on both.
#[test]
fn binary_note_matches_between_indexed_and_brute_force() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();

    let mut content = String::new();
    for i in 0..800 {
        content.push_str(&format!("notaneedle line {i}\n"));
    }
    content.push_str("binary\u{0}marker\n");
    content.push_str("tail notaneedle only\n");
    fs::write(sub.join("big.txt"), &content).unwrap();

    let idx = dir.path().join("idx").to_str().unwrap().to_string();
    tgrep()
        .args(["index", &fixture_path(&dir), "--index-path", &idx])
        .assert()
        .success();

    let run = |mode: &[&str], target: &str| -> String {
        let mut args: Vec<&str> = mode.to_vec();
        args.extend_from_slice(&["--no-heading", "tail notaneedle", target]);
        let out = tgrep().args(&args).output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    let big = sub.join("big.txt").to_str().unwrap().to_string();
    let file_local = run(&["--no-index"], &big);
    let file_indexed = run(&["--index-path", &idx], &big);
    assert!(
        file_local.contains("binary file matches"),
        "expected a binary note, got {file_local:?}"
    );
    assert_eq!(
        file_local, file_indexed,
        "binary notes must agree across search paths"
    );

    let dir_local = run(&["--no-index"], &fixture_path(&dir));
    let dir_indexed = run(&["--index-path", &idx], &fixture_path(&dir));
    assert_eq!(dir_local, "", "traversal must hide binary files");
    assert_eq!(
        dir_local, dir_indexed,
        "binary suppression must agree across search paths"
    );
}

// ---------------------------------------------------------------------------
// File types (-t/-T/--type-add/--type-clear/--type-list)
// ---------------------------------------------------------------------------

#[test]
fn type_filter_restricts_to_rust_files() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-t",
            "rust",
            "fn",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello.rs").or(predicate::str::contains("lib.rs")))
        .stdout(predicate::str::contains("config.toml").not());
}

#[test]
fn type_not_excludes_a_type() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-T",
            "rust",
            "-l",
            "e",
            &fixture_path(&dir),
        ])
        .assert()
        .stdout(predicate::str::contains(".rs").not());
}

#[test]
fn type_flag_is_repeatable() {
    let dir = setup_fixture();
    let out = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-t",
            "rust",
            "-t",
            "toml",
            "-l",
            "e",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(".rs"), "expected rust files, got {s:?}");
    assert!(s.contains(".toml"), "expected toml files, got {s:?}");
    assert!(!s.contains(".txt"), "txt must be excluded, got {s:?}");
}

#[test]
fn unknown_type_is_a_usage_error() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "-t",
            "definitelynotatype",
            "fn",
            &fixture_path(&dir),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unrecognized file type"));
}

#[test]
fn type_add_defines_a_new_type() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--type-add",
            "note:*.txt",
            "-t",
            "note",
            "-l",
            "note",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("notes.txt"));
}

#[test]
fn type_clear_removes_a_builtin_type() {
    let dir = setup_fixture();
    // ripgrep drops the definition entirely, so selecting it afterwards is an
    // unrecognized-type usage error rather than a match-nothing filter.
    tgrep()
        .args([
            "--no-index",
            "--type-clear",
            "rust",
            "-t",
            "rust",
            "fn",
            &fixture_path(&dir),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unrecognized file type"));
}

#[test]
fn type_list_includes_ripgrep_builtins() {
    tgrep()
        .arg("--type-list")
        .assert()
        .success()
        .stdout(predicate::str::contains("rust:"))
        .stdout(predicate::str::contains("cpp:"))
        .stdout(predicate::str::contains("py:"));
}

#[test]
fn type_list_reflects_type_add() {
    tgrep()
        .args(["--type-list", "--type-add", "zzz:*.zzz"])
        .assert()
        .success()
        .stdout(predicate::str::contains("zzz:"));
}

// ---------------------------------------------------------------------------
// Encoding (-E/--encoding)
// ---------------------------------------------------------------------------

/// Write `text` as UTF-16LE, optionally with a BOM.
fn write_utf16le(path: &std::path::Path, text: &str, bom: bool) {
    let mut bytes = Vec::new();
    if bom {
        bytes.extend_from_slice(&[0xFF, 0xFE]);
    }
    for u in text.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    fs::write(path, bytes).unwrap();
}

#[test]
fn utf16_bom_files_are_searched_as_text() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    write_utf16le(&sub.join("wide.txt"), "hello needle here\n", true);

    tgrep()
        .args(["--no-index", "--no-heading", "needle", &fixture_path(&dir)])
        .assert()
        .success()
        .stdout(predicate::str::contains("needle"))
        .stdout(predicate::str::contains("Binary file").not());
}

#[test]
fn explicit_encoding_finds_bomless_utf16() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    write_utf16le(&sub.join("wide.txt"), "hello needle here\n", false);

    // Without -E the file has no BOM, so it is treated as raw bytes.
    tgrep()
        .args(["--no-index", "--no-heading", "needle", &fixture_path(&dir)])
        .assert()
        .code(1);

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-E",
            "utf-16le",
            "needle",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("needle"));
}

#[test]
fn encoding_none_disables_bom_sniffing() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    write_utf16le(&sub.join("wide.txt"), "hello needle here\n", true);

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-E",
            "none",
            "needle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);
}

#[test]
fn no_encoding_restores_auto_detection() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    write_utf16le(&sub.join("wide.txt"), "hello needle here\n", true);

    // `--no-encoding` comes last, so it wins over `-E none`.
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-E",
            "none",
            "--no-encoding",
            "needle",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("needle"));
}

#[test]
fn latin1_encoding_decodes_high_bytes() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    // "café" in Latin-1: the 0xE9 byte is not valid UTF-8.
    fs::write(sub.join("l1.txt"), b"caf\xE9 au lait\n").unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-E",
            "latin1",
            "café",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("café"));
}

#[test]
fn unknown_encoding_is_a_usage_error() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "-E",
            "definitelynotanencoding",
            "fn",
            &fixture_path(&dir),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unsupported encoding"));
}

#[test]
fn encoding_results_agree_across_search_paths() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    write_utf16le(&sub.join("wide.txt"), "hello needle here\n", false);
    fs::write(sub.join("plain.txt"), "needle in plain text\n").unwrap();

    let idx = dir.path().join("idx").to_str().unwrap().to_string();
    tgrep()
        .args(["index", &fixture_path(&dir), "--index-path", &idx])
        .assert()
        .success();

    let local = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-E",
            "utf-16le",
            "needle",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let indexed = tgrep()
        .args([
            "--index-path",
            &idx,
            "--no-heading",
            "-E",
            "utf-16le",
            "needle",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();

    let mut a: Vec<_> = String::from_utf8_lossy(&local.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let mut b: Vec<_> = String::from_utf8_lossy(&indexed.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    a.sort();
    b.sort();
    assert!(!a.is_empty(), "expected matches, got none");
    assert_eq!(a, b, "--encoding must agree across search paths");
}

// ---------------------------------------------------------------------------
// New ripgrep flags: matching, output, and walking
// ---------------------------------------------------------------------------

#[test]
fn line_regexp_requires_a_whole_line_match() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-x",
            "hello",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-x",
            "    a \\+ b",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("a + b"));
}

#[test]
fn line_regexp_beats_word_regexp() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("p.txt"), "(flag)\n").unwrap();

    // `-w` alone would reject a line that starts and ends with punctuation.
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-x",
            "-w",
            r"\(flag\)",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("(flag)"));
}

#[test]
fn pcre2_enables_lookaround() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-P",
            "hello(?! world)",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-P",
            "hello(?= world)",
            &fixture_path(&dir),
        ])
        .assert()
        .success();
}

#[test]
fn default_engine_rejects_lookaround() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--engine",
            "default",
            "hello(?= world)",
            &fixture_path(&dir),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("regex error"));
}

#[test]
fn unknown_engine_is_a_usage_error() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--engine",
            "bogus",
            "hello",
            &fixture_path(&dir),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unrecognized regex engine"));
}

#[test]
fn pcre2_version_reports_the_engine() {
    tgrep()
        .arg("--pcre2-version")
        .assert()
        .success()
        .stdout(predicate::str::contains("fancy-regex"));
}

#[test]
fn regex_size_limit_rejects_an_oversized_pattern() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--regex-size-limit",
            "1",
            "\\w{3,20}x[a-z]+",
            &fixture_path(&dir),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("size limit"));
}

#[test]
fn replace_rewrites_matches() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-r",
            "REP",
            "hello",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("REP world"));
}

#[test]
fn replace_expands_capture_groups() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-r",
            "<${1}>",
            "hel(lo)",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("<lo> world"));
}

#[test]
fn replace_with_only_matching_prints_replacements() {
    let dir = setup_fixture();
    let out = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-o",
            "-r",
            "REP",
            "hello",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.lines().any(|l| l.ends_with("REP")), "got {s:?}");
    assert!(
        !s.contains("world"),
        "-o must not print the rest, got {s:?}"
    );
}

#[test]
fn passthru_prints_every_line() {
    let dir = setup_fixture();
    let out = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--passthru",
            "hello",
            dir.path()
                .join("testdata")
                .join("hello.rs")
                .to_str()
                .unwrap(),
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert_eq!(s.lines().count(), 3, "expected the whole file, got {s:?}");
    assert!(s.contains("fn main()"), "got {s:?}");
}

#[test]
fn stop_on_nonmatch_halts_at_the_first_gap() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("s.txt"), "hit\nhit\nmiss\nhit\n").unwrap();

    let out = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "--stop-on-nonmatch",
            "hit",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        s.lines().count(),
        2,
        "expected to stop at line 3, got {s:?}"
    );
}

#[test]
fn column_flag_prints_match_columns() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--column",
            "world",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(":2:21:"));
}

#[test]
fn byte_offset_flag_prints_line_offsets() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-n",
            "-b",
            "world",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(":2:12:"));
}

#[test]
fn max_columns_omits_long_lines() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(
        sub.join("long.txt"),
        format!("needle {}\n", "x".repeat(200)),
    )
    .unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-M",
            "20",
            "needle",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("[Omitted long matching line]"));
}

#[test]
fn max_columns_preview_truncates_instead() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(
        sub.join("long.txt"),
        format!("needle {}\n", "x".repeat(200)),
    )
    .unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-M",
            "20",
            "--max-columns-preview",
            "needle",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("[... omitted end of long line]"));
}

#[test]
fn count_matches_counts_matches_not_lines() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("m.txt"), "aa aa aa\n").unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-c",
            "aa",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(":1"));
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--count-matches",
            "aa",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(":3"));
}

#[test]
fn include_zero_reports_unmatched_files() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-c",
            "--include-zero",
            "definitelynothere",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("hello.rs:0"));
}

#[test]
fn context_separator_is_configurable() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("g.txt"), "hit\na\nb\nc\nd\nhit\n").unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-C1",
            "--context-separator",
            "=====",
            "hit",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("====="));

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-C1",
            "--no-context-separator",
            "hit",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("--").not());
}

#[test]
fn field_separators_are_configurable() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-n",
            "--field-match-separator",
            "@@",
            "world",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("@@2@@"));
}

#[test]
fn path_separator_rewrites_printed_paths() {
    let dir = setup_fixture();
    let nested = dir.path().join("testdata").join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("deep.txt"), "world\n").unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--path-separator",
            "::",
            "-l",
            "world",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("nested::deep.txt"));
}

#[test]
fn sort_path_orders_results_deterministically() {
    let dir = setup_fixture();
    let out = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--sort",
            "path",
            "-l",
            "e",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let lines: Vec<_> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let mut sorted = lines.clone();
    sorted.sort();
    assert_eq!(lines, sorted, "--sort path must emit ascending order");

    let rev = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--sortr",
            "path",
            "-l",
            "e",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let mut rev_lines: Vec<_> = String::from_utf8_lossy(&rev.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    rev_lines.reverse();
    assert_eq!(rev_lines, sorted, "--sortr path must emit descending order");
}

#[test]
fn unknown_sort_criteria_is_a_usage_error() {
    let dir = setup_fixture();
    tgrep()
        .args(["--no-index", "--sort", "bogus", "e", &fixture_path(&dir)])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unrecognized sort criteria"));
}

#[test]
fn max_depth_limits_recursion() {
    let dir = setup_fixture();
    let deep = dir.path().join("testdata").join("nested");
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("deep.txt"), "needle\n").unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--max-depth",
            "1",
            "-l",
            "needle",
            &fixture_path(&dir),
        ])
        .assert()
        .code(1);
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--max-depth",
            "2",
            "-l",
            "needle",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("deep.txt"));
}

#[test]
fn ignore_file_applies_extra_rules() {
    let dir = setup_fixture();
    let ignore = dir.path().join("extra-ignore");
    fs::write(&ignore, "hello.rs\n").unwrap();

    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--ignore-file",
            ignore.to_str().unwrap(),
            "-l",
            "fn",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello.rs").not())
        .stdout(predicate::str::contains("lib.rs"));
}

#[test]
fn threads_flag_is_accepted() {
    let dir = setup_fixture();
    tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-j",
            "1",
            "hello",
            &fixture_path(&dir),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello world"));
}

#[test]
fn pretty_implies_color_and_heading() {
    let dir = setup_fixture();
    let out = tgrep()
        .args(["--no-index", "-p", "hello", &fixture_path(&dir)])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("\u{1b}["), "expected ANSI colour, got {s:?}");
    assert!(s.contains("hello.rs"), "expected a heading, got {s:?}");
}

#[test]
fn new_flags_agree_across_search_paths() {
    let dir = setup_fixture();
    let idx = dir.path().join("idx").to_str().unwrap().to_string();
    tgrep()
        .args(["index", &fixture_path(&dir), "--index-path", &idx])
        .assert()
        .success();

    let cases: &[&[&str]] = &[
        &["--no-heading", "-r", "REP", "hello"],
        &["--no-heading", "--passthru", "hello"],
        &["--no-heading", "--column", "-b", "hello"],
        &["--no-heading", "-x", "}"],
        &["--no-heading", "--count-matches", "n"],
        &["--no-heading", "-c", "n"],
        &["--no-heading", "-M", "10", "hello"],
        &["--no-heading", "-P", "hello(?= world)"],
        &["--no-heading", "--sort", "path", "-l", "n"],
        &["--no-heading", "-o", "-r", "REP", "hello"],
    ];

    let path = fixture_path(&dir);
    for case in cases {
        let mut local_args = vec!["--no-index"];
        local_args.extend_from_slice(case);
        local_args.push(&path);
        let local = tgrep().args(&local_args).output().unwrap();

        let mut idx_args = vec!["--index-path", &idx];
        idx_args.extend_from_slice(case);
        idx_args.push(&path);
        let indexed = tgrep().args(&idx_args).output().unwrap();

        let mut a: Vec<_> = String::from_utf8_lossy(&local.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        let mut b: Vec<_> = String::from_utf8_lossy(&indexed.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        a.sort();
        b.sort();
        assert_eq!(a, b, "paths disagree for {case:?}");
    }
}

#[test]
fn search_zip_is_rejected_rather_than_ignored() {
    let dir = setup_fixture();
    tgrep()
        .args(["--no-index", "-z", "hello", &fixture_path(&dir)])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("not supported"));
}

#[test]
fn compatibility_no_op_flags_are_accepted() {
    let dir = setup_fixture();
    for flag in ["--mmap", "--no-mmap", "--crlf", "--no-crlf", "--no-config"] {
        tgrep()
            .args([
                "--no-index",
                "--no-heading",
                flag,
                "hello",
                &fixture_path(&dir),
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains("hello world"));
    }
}

#[test]
fn column_is_not_printed_on_context_lines() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("f.txt"), "alpha\nbravo NEEDLE here\ncharlie\n").unwrap();

    let out = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "--column",
            "-C1",
            "NEEDLE",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("2:7:bravo"), "match needs a column, got {s:?}");
    assert!(
        s.contains("1-alpha"),
        "context must have no column, got {s:?}"
    );
    assert!(
        s.contains("3-charlie"),
        "context must have no column, got {s:?}"
    );
}

#[test]
fn include_zero_agrees_across_search_paths() {
    let dir = setup_fixture();
    let idx = dir.path().join("idx").to_str().unwrap().to_string();
    tgrep()
        .args(["index", &fixture_path(&dir), "--index-path", &idx])
        .assert()
        .success();

    let local = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-c",
            "--include-zero",
            "definitelynothere",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let indexed = tgrep()
        .args([
            "--index-path",
            &idx,
            "--no-heading",
            "-c",
            "--include-zero",
            "definitelynothere",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();

    let mut a: Vec<_> = String::from_utf8_lossy(&local.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let mut b: Vec<_> = String::from_utf8_lossy(&indexed.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    a.sort();
    b.sort();
    assert!(!a.is_empty(), "expected zero-count rows, got none");
    assert_eq!(a, b, "--include-zero must agree across search paths");
}

#[test]
fn sort_path_uses_the_same_order_on_every_search_path() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(sub.join("src")).unwrap();
    // `.` and `x` straddle `/` in ASCII, so a raw string sort disagrees with a
    // component-wise path sort here.
    fs::write(sub.join("src.rs"), "needle\n").unwrap();
    fs::write(sub.join("srcx.rs"), "needle\n").unwrap();
    fs::write(sub.join("src").join("lib.rs"), "needle\n").unwrap();

    let idx = dir.path().join("idx").to_str().unwrap().to_string();
    tgrep()
        .args(["index", &fixture_path(&dir), "--index-path", &idx])
        .assert()
        .success();

    let local = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--sort",
            "path",
            "-l",
            "needle",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();
    let indexed = tgrep()
        .args([
            "--index-path",
            &idx,
            "--no-heading",
            "--sort",
            "path",
            "-l",
            "needle",
            &fixture_path(&dir),
        ])
        .output()
        .unwrap();

    // Deliberately NOT sorted: the order itself is what is under test.
    let a = String::from_utf8_lossy(&local.stdout).to_string();
    let b = String::from_utf8_lossy(&indexed.stdout).to_string();
    assert_eq!(a.lines().count(), 3, "expected three files, got {a:?}");
    assert_eq!(a, b, "--sort path must use one order on every search path");
}

// ---------------------------------------------------------------------------
// `--no-require-git` reaches the index, not just the search
//
// `.gitignore` is git-gated by default (ripgrep's own rule), so enlistments
// that are not git checkouts -- Perforce and Source Depot trees, exported
// source drops -- have their `.gitignore` files ignored and index build
// output. `--no-require-git` is the documented escape hatch, and because it is
// a global flag clap accepted it on `index` and `serve` while the value was
// dropped on the floor: the search path honoured it and the index path did
// not, so the two disagreed about which files existed.
// ---------------------------------------------------------------------------

/// A tree with a real `.gitignore` and deliberately no `.git` directory.
fn non_git_enlistment() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("testdata");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("build")).unwrap();
    fs::write(root.join(".gitignore"), "build/\n*.log\n").unwrap();
    fs::write(root.join("src").join("main.rs"), "let needle = 1;\n").unwrap();
    fs::write(root.join("build").join("gen.rs"), "let needle = 2;\n").unwrap();
    fs::write(root.join("out.log"), "needle\n").unwrap();
    dir
}

fn indexed_file_count(root: &str, idx: &str, extra: &[&str]) -> usize {
    let mut args = vec!["index", root, "--index-path", idx];
    args.extend_from_slice(extra);
    let out = tgrep().args(&args).output().unwrap();
    assert!(out.status.success(), "index failed: {out:?}");
    let text = String::from_utf8_lossy(&out.stderr).to_string();
    let line = text
        .lines()
        .find(|l| l.starts_with("Found "))
        .unwrap_or_else(|| panic!("no 'Found' line in: {text}"));
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn index_is_git_gated_by_default_like_ripgrep() {
    let dir = non_git_enlistment();
    let idx = dir.path().join("idx");
    let n = indexed_file_count(&indexed_fixture_path(&dir), idx.to_str().unwrap(), &[]);
    // src/main.rs + build/gen.rs + out.log. `.gitignore` is dot-prefixed and so
    // is filtered by the walk's hidden rule regardless of the git gate.
    assert_eq!(n, 3, "no `.git`, so `.gitignore` must not apply by default");
}

#[test]
fn index_honours_no_require_git() {
    let dir = non_git_enlistment();
    let idx = dir.path().join("idx");
    let n = indexed_file_count(
        &indexed_fixture_path(&dir),
        idx.to_str().unwrap(),
        &["--no-require-git"],
    );
    // Only src/main.rs survives; `.gitignore` is dot-prefixed and hidden anyway.
    assert_eq!(
        n, 1,
        "`--no-require-git` must reach the indexing walk, not just search"
    );
}

/// The actual user-visible symptom: build output stays searchable through the
/// index even though `.gitignore` excludes it.
#[test]
fn indexed_search_excludes_ignored_files_under_no_require_git() {
    let dir = non_git_enlistment();
    let idx = dir.path().join("idx");
    let root = indexed_fixture_path(&dir);
    tgrep()
        .args([
            "index",
            &root,
            "--index-path",
            idx.to_str().unwrap(),
            "--no-require-git",
        ])
        .assert()
        .success();

    let out = tgrep()
        .args([
            "--no-heading",
            "--index-path",
            idx.to_str().unwrap(),
            "--no-require-git",
            "needle",
            &root,
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        stdout.contains("main.rs"),
        "expected the source hit: {stdout:?}"
    );
    assert!(
        !stdout.contains("gen.rs") && !stdout.contains("out.log"),
        "ignored build output must not be searchable: {stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// `-P` patterns still use the index
//
// A PCRE pattern cannot be parsed by regex-syntax, so the planner used to give up
// and hand back MatchAll - every file in the index, read and decoded and matched
// one by one. On a large tree that turned a query with an obvious mandatory
// literal into a full-corpus scan. The planner now relaxes the pattern first
// (dropping the zero-width assertions) and plans from the result, which matches a
// superset and therefore cannot exclude a real hit. These tests pin that the
// answers are unchanged.
// ---------------------------------------------------------------------------

/// Index and direct scans can print paths differently; compare the set of lines
/// with separators and ordering normalised away.
fn normalize(out: &str) -> Vec<String> {
    let mut lines: Vec<String> = out
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.replace('\\', "/"))
        .collect();
    lines.sort();
    lines
}

/// Two files hold the literal - one commented out, one not - plus decoys that do
/// not hold it at all, so a plan that works can be told apart from one that does
/// not.
fn setup_lookaround_fixture() -> (TempDir, String) {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("testdata");
    fs::create_dir_all(&sub).unwrap();

    fs::write(sub.join("live.rs"), "let p = ExchangePrincipal::new();\n").unwrap();
    fs::write(sub.join("commented.rs"), "//ExchangePrincipal is gone\n").unwrap();
    fs::write(sub.join("unrelated.rs"), "let q = SomethingElse::new();\n").unwrap();
    fs::write(sub.join("notes.txt"), "no mention of the type here\n").unwrap();

    let index_dir = dir.path().join("idx");
    tgrep()
        .args([
            "index",
            sub.to_str().unwrap(),
            "--index-path",
            index_dir.to_str().unwrap(),
        ])
        .assert()
        .success();

    (dir, index_dir.to_str().unwrap().to_string())
}

#[test]
fn indexed_lookaround_finds_the_same_lines_as_a_direct_scan() {
    let (dir, idx) = setup_lookaround_fixture();
    let root = indexed_fixture_path(&dir);

    let indexed = String::from_utf8(
        tgrep()
            .args([
                "--no-heading",
                "-P",
                "--index-path",
                &idx,
                r"(?<!//)ExchangePrincipal",
                &root,
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();

    let direct = String::from_utf8(
        tgrep()
            .args([
                "--no-heading",
                "-P",
                "--no-index",
                r"(?<!//)ExchangePrincipal",
                &root,
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();

    assert_eq!(
        normalize(&indexed),
        normalize(&direct),
        "using the index must not change the answer"
    );
    assert!(
        indexed.contains("live.rs"),
        "the uncommented hit survives the relaxed plan: {indexed}"
    );
    assert!(
        !indexed.contains("commented.rs"),
        "the lookbehind is still applied by the real matcher: {indexed}"
    );
}

#[test]
fn indexed_lookahead_finds_the_same_lines_as_a_direct_scan() {
    let (dir, idx) = setup_lookaround_fixture();
    let root = indexed_fixture_path(&dir);

    for pattern in [
        r"ExchangePrincipal(?=::)",
        r"ExchangePrincipal(?!::)",
        r"(?=.*Exchange)ExchangePrincipal",
    ] {
        let indexed = tgrep()
            .args(["--no-heading", "-P", "--index-path", &idx, pattern, &root])
            .assert()
            .get_output()
            .stdout
            .clone();
        let direct = tgrep()
            .args(["--no-heading", "-P", "--no-index", pattern, &root])
            .assert()
            .get_output()
            .stdout
            .clone();
        assert_eq!(
            normalize(&String::from_utf8(indexed).unwrap()),
            normalize(&String::from_utf8(direct).unwrap()),
            "index and direct scan disagree for {pattern}"
        );
    }
}

#[test]
fn indexed_backreference_still_returns_every_match() {
    // Backreferences cannot be relaxed, so the planner falls back to scanning
    // everything. That is slow, not wrong - the results must still be complete.
    let (dir, idx) = setup_lookaround_fixture();
    let root = indexed_fixture_path(&dir);

    let out = String::from_utf8(
        tgrep()
            .args([
                "--no-heading",
                "-P",
                "--index-path",
                &idx,
                r"(Exchange)\1?Principal",
                &root,
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(
        out.contains("live.rs") && out.contains("commented.rs"),
        "an unrelaxable pattern still sees every file: {out}"
    );
}
