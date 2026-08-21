//! Regression test for oversized files being evicted from the index.
//!
//! The startup stale check compares the index against a metadata walk, and any
//! indexed file missing from that walk is classified as *deleted*. The metadata
//! walk used to hard-code the default 1 MiB size cap, so an index built with a
//! raised `--max-filesize` lost every file above 1 MiB the first time it was
//! served — silently, and permanently once the eviction was flushed to disk.
//!
//! This test builds an index with a raised cap, serves it with the same cap,
//! and asserts the large file survives and is still searchable.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const MARKER: &str = "oversized_survivor_marker";
/// Comfortably over the 1 MiB default cap, comfortably under the 10 MiB one
/// the test raises it to.
const BIG_FILE_BYTES: usize = 2 * 1024 * 1024;

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

/// A repo holding the marker in one small file and one file above the default
/// size cap.
fn setup_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::write(
        root.join("small.txt"),
        format!("{MARKER} in a small file\n"),
    )
    .unwrap();

    let mut big = String::with_capacity(BIG_FILE_BYTES + 64);
    while big.len() < BIG_FILE_BYTES {
        big.push_str("padding padding padding padding padding padding\n");
    }
    big.push_str(&format!("{MARKER} in a big file\n"));
    fs::write(root.join("big.txt"), big).unwrap();

    dir
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

/// Paths that matched, so a failure names the missing file instead of just
/// reporting a count that is one too low.
fn matching_paths(port: u16, pattern: &str) -> Vec<String> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "search",
        "id": 1,
        "params": { "pattern": pattern }
    })
    .to_string();
    let response = send_request(port, &request).expect("search request failed");
    let value: serde_json::Value = serde_json::from_str(&response).expect("invalid JSON response");
    let matches = value
        .pointer("/result/matches")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("missing matches in response: {response}"));

    let mut paths: Vec<String> = matches
        .iter()
        .filter_map(|m| m.get("file").and_then(|p| p.as_str()))
        .map(|p| {
            Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string())
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
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

/// Block until the startup stale check has finished applying its diff.
///
/// Without this the search can outrun the stale check and observe an index the
/// check has not touched yet — which would pass whether or not the eviction bug
/// is present, making the test worthless. Both terminal messages are accepted
/// so the wait ends on the buggy path too, and that path fails on the
/// assertion rather than on a timeout.
fn wait_for_stale_check(log_path: &Path) {
    let start = Instant::now();
    loop {
        assert!(
            start.elapsed() <= Duration::from_secs(60),
            "stale check did not finish within 60 seconds; log:\n{}",
            fs::read_to_string(log_path).unwrap_or_default()
        );
        if let Ok(log) = fs::read_to_string(log_path)
            && (log.contains("stale check: index is up-to-date")
                || log.contains("stale check: updated"))
        {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn serving_an_index_built_with_a_raised_max_filesize_keeps_large_files() {
    let fixture = setup_fixture();
    let root = fixture.path();
    let index_dir = root.join(".tgrep_test_index");

    // Build a complete index with a raised cap so `serve` takes the warm-start
    // path and runs the stale check against an index holding a 2 MiB file.
    let output = Command::new(tgrep_bin())
        .args([
            "index",
            root.to_str().unwrap(),
            "--index-path",
            index_dir.to_str().unwrap(),
            "--max-filesize",
            "10M",
        ])
        .output()
        .expect("failed to run tgrep index");
    assert!(
        output.status.success(),
        "initial index build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Guards the premise: if the build itself dropped the big file, the stale
    // check below could never evict it and the test would pass vacuously.
    let build_log = String::from_utf8_lossy(&output.stderr);
    assert!(
        build_log.contains("Found 2 text files"),
        "expected both files indexed under --max-filesize 10M, got: {build_log}"
    );

    // Deliberately outside the indexed tree: a log file written into `root`
    // would show up as a new file in the very walk under test.
    let log_dir = TempDir::new().unwrap();
    let serve_log = log_dir.path().join("serve.stderr.log");
    let child = Command::new(tgrep_bin())
        .args([
            "serve",
            root.to_str().unwrap(),
            "--index-path",
            index_dir.to_str().unwrap(),
            "--max-filesize",
            "10M",
            "--no-watch",
        ])
        .stderr(Stdio::from(fs::File::create(&serve_log).unwrap()))
        .spawn()
        .expect("failed to spawn tgrep serve");
    let _guard = ServerGuard { child };

    let port = wait_for_port(&index_dir);
    wait_for_stale_check(&serve_log);
    let paths = matching_paths(port, MARKER);

    assert_eq!(
        paths,
        vec!["big.txt".to_string(), "small.txt".to_string()],
        "the stale check evicted the oversized file from the index"
    );
}
