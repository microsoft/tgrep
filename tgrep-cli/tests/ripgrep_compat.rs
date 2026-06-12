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

#[test]
fn count_files_rejects_windows_os_repo_root() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(
        dir.path().join(".git").join("config"),
        "[remote \"origin\"]\n    url = https://dev.azure.com/microsoft/OS/_git/OS\n",
    )
    .unwrap();
    fs::write(dir.path().join("file.txt"), "hello\n").unwrap();

    tgrep()
        .args(["count-files", dir.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Windows OS repo"));
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
    // Primary pattern as positional arg, extra pattern via -e
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-e",
            "fn add",
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
    // Primary pattern as positional, extra via --regexp
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--regexp",
            "version",
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
    // Put extra pattern in the file; primary pattern is positional
    fs::write(&pattern_file, "version\n").unwrap();

    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "-f",
            pattern_file.to_str().unwrap(),
            "fn main",
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
    // Each line should be valid JSON
    for line in stdout.lines() {
        let parsed: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|_| panic!("invalid JSON line: {line}"));
        assert_eq!(parsed["type"], "match");
        assert!(parsed["line"].is_number());
        assert!(parsed["content"].is_string());
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
    let parsed: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!(parsed["line"], 1);
    assert!(parsed["content"].as_str().unwrap().contains("fn main"));
    let file = parsed["file"].as_str().unwrap();
    assert!(
        file.contains("hello.rs"),
        "expected hello.rs in file, got: {file}"
    );
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
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines.len() >= 2,
        "expected at least 2 JSON lines, got: {stdout}"
    );
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(first["type"], "match");
    assert_eq!(second["type"], "context");
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
    let output = tgrep()
        .args(["--no-index", "--no-heading", "fn", &fixture_path(&dir)])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // In flat format: every match line should contain file:line:content
    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        assert!(
            parts.len() >= 3,
            "expected file:line:content in flat mode, got: {line}"
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

    // -F with positional pattern "a+b" and -e "c+d"
    let output = tgrep()
        .args([
            "--no-index",
            "--no-heading",
            "--no-filename",
            "-N",
            "-F",
            "-e",
            "c+d",
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
        .args(["--no-index", "-v", "-L", ".", &fixture_path(&dir)])
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
            "-L",
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
    for line in stdout.lines() {
        let parsed: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|_| panic!("invalid JSON: {line}"));
        assert_eq!(parsed["type"], "match");
        assert!(parsed["line"].is_number());
        assert!(parsed["content"].as_str().unwrap().contains("fn main"));
    }
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
