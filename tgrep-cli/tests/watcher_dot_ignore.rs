//! End-to-end coverage for the live watcher's ignore handling.
//!
//! Two properties are checked together on purpose, because each one alone can
//! pass for the wrong reason:
//!
//!  * a file created under a directory excluded by `.ignore` must never be
//!    indexed, and
//!  * an ordinary file created at the same moment *must* be.
//!
//! Without the second assertion a watcher that had simply stopped working —
//! for example one whose events never reach the indexing worker — would
//! satisfy the first. Without the first, the ignore rules could be ignored
//! entirely and nothing would notice.
//!
//! The fixture deliberately has **no** `.git` directory. `.gitignore` rules
//! only apply inside a repository, but `.ignore` applies everywhere, so this
//! also pins that distinction.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn tgrep_bin() -> std::path::PathBuf {
    assert_cmd::cargo::cargo_bin("tgrep")
}

struct ServerGuard {
    child: Child,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn send_request(port: u16, request: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    writeln!(stream, "{request}")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(response)
}

fn search_matches(port: u16, pattern: &str) -> u64 {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "search",
        "id": 1,
        "params": { "pattern": pattern }
    })
    .to_string();
    let response = send_request(port, &request).expect("search request failed");
    let value: serde_json::Value = serde_json::from_str(&response).expect("invalid JSON response");
    value
        .pointer("/result/num_matches")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| panic!("missing num_matches in response: {response}"))
}

fn wait_for_port(index_dir: &Path) -> u16 {
    let serve_json = index_dir.join("serve.json");
    let start = Instant::now();
    loop {
        assert!(
            start.elapsed() <= Duration::from_secs(60),
            "tgrep serve did not start within 60 seconds"
        );
        if let Ok(data) = fs::read_to_string(&serve_json)
            && let Ok(info) = serde_json::from_str::<serde_json::Value>(&data)
            && let Some(p) = info.get("port").and_then(|v| v.as_u64())
            && TcpStream::connect(format!("127.0.0.1:{p}")).is_ok()
        {
            return p as u16;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Poll until `pattern` is searchable, returning whether it ever showed up.
fn wait_for_match(port: u16, pattern: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if search_matches(port, pattern) > 0 {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Poll until `pattern` stops being searchable, returning whether it went away.
fn wait_for_no_match(port: u16, pattern: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if search_matches(port, pattern) == 0 {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn build_index(root: &Path, index_dir: &Path) {
    let status = Command::new(tgrep_bin())
        .args([
            "index",
            root.to_str().unwrap(),
            "--index-path",
            index_dir.to_str().unwrap(),
        ])
        .status()
        .expect("failed to run tgrep index");
    assert!(status.success(), "initial index build failed");
}

fn spawn_server(root: &Path, index_dir: &Path) -> ServerGuard {
    let child = Command::new(tgrep_bin())
        .args([
            "serve",
            "--index-path",
            index_dir.to_str().unwrap(),
            root.to_str().unwrap(),
        ])
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("failed to start tgrep serve");
    ServerGuard { child }
}

#[test]
fn watcher_honors_dot_ignore_and_still_indexes_new_files() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let index_dir = root.join(".tgrep_test_index");

    // No `.git` here: `.ignore` must apply on its own.
    fs::write(root.join(".ignore"), "secret/\n").unwrap();
    fs::create_dir_all(root.join("secret")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src").join("lib.rs"),
        "fn seeded() { let normal_source_marker = 1; }\n",
    )
    .unwrap();

    build_index(root, &index_dir);
    let _server = spawn_server(root, &index_dir);

    let port = wait_for_port(&index_dir);

    // Positive control on the seeded index, and a wait for the watcher to be
    // live: the matcher is published on a background thread, and until it is
    // the watcher drops events rather than risk indexing ignored paths.
    assert!(
        wait_for_match(port, "normal_source_marker", Duration::from_secs(30)),
        "expected the seeded source file to be searchable"
    );
    thread::sleep(Duration::from_secs(2));

    fs::write(
        root.join("secret").join("creds.txt"),
        "dot_ignored_leak_marker\n",
    )
    .unwrap();
    fs::write(
        root.join("src").join("added.rs"),
        "fn added() { let watcher_added_marker = 2; }\n",
    )
    .unwrap();

    // The ordinary file proves the watcher is actually delivering events to
    // the indexing worker, so the ignored-file assertion below means something.
    assert!(
        wait_for_match(port, "watcher_added_marker", Duration::from_secs(30)),
        "watcher never indexed a newly created ordinary file"
    );

    assert_eq!(
        search_matches(port, "dot_ignored_leak_marker"),
        0,
        "watcher indexed a file under a directory excluded by .ignore"
    );
}

/// A `.ignore` written *after* the server is live must refresh the ignore
/// rules, exactly as a `.gitignore` write does.
///
/// The startup matcher is built from the walk that the initial index used, so
/// a `.ignore` that already exists is honored for free — which is what the
/// test above covers. Rules that appear later only take effect if the watcher
/// recognizes the `.ignore` write as an ignore-rules change and schedules the
/// refresh; when it does not, the stale matcher stays published and the
/// already-indexed content under the newly excluded directory stays
/// searchable indefinitely (the periodic reconcile is on an hourly timer).
///
/// The fixture seeds the ignored file *before* indexing, so the assertion is a
/// transition — searchable, then not — rather than a fixed sleep racing the
/// refresh.
#[test]
fn late_dot_ignore_refreshes_the_watchers_ignore_rules() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let index_dir = root.join(".tgrep_test_index");

    // No `.git` here either: `.ignore` is not git-gated, so this pins the
    // refresh path for the one ignore source that works outside a repo.
    fs::create_dir_all(root.join("secret")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("secret").join("creds.txt"),
        "late_ignored_leak_marker\n",
    )
    .unwrap();
    fs::write(
        root.join("src").join("lib.rs"),
        "fn seeded() { let normal_source_marker = 1; }\n",
    )
    .unwrap();

    build_index(root, &index_dir);
    let _server = spawn_server(root, &index_dir);

    let port = wait_for_port(&index_dir);

    assert!(
        wait_for_match(port, "normal_source_marker", Duration::from_secs(30)),
        "expected the seeded source file to be searchable"
    );
    // Positive control: with no `.ignore` yet, the seeded file under `secret/`
    // is legitimately indexed. Without this the assertion below could pass
    // simply because the file was never indexed in the first place.
    assert!(
        wait_for_match(port, "late_ignored_leak_marker", Duration::from_secs(30)),
        "expected the file under secret/ to be indexed before any .ignore exists"
    );
    thread::sleep(Duration::from_secs(2));

    fs::write(root.join(".ignore"), "secret/\n").unwrap();

    assert!(
        wait_for_no_match(port, "late_ignored_leak_marker", Duration::from_secs(60)),
        "a .ignore written while the server was live never refreshed the ignore \
         rules; content under the newly excluded directory is still searchable"
    );

    // The refresh must not take the rest of the index with it.
    assert!(
        search_matches(port, "normal_source_marker") > 0,
        "the ignore-rules refresh dropped a file that is not ignored"
    );
}
