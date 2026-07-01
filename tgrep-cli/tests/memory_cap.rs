//! Integration test: the memory-bounded indexer must still produce a
//! **complete** index.
//!
//! Starts `tgrep serve` with a tiny `--max-memory` budget so the bulk indexer
//! is forced to flush its in-memory overlay to disk repeatedly mid-build, then
//! verifies that (a) the published index ends up marked complete and (b) every
//! file — including ones indexed in the very last batch, after several
//! incremental flushes — is searchable.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Number of files to index. Exceeds the indexer's internal batch size (500)
/// so that, with a 1 MB cap, multiple incremental flushes fire, while staying
/// small enough to keep the test fast and CI-friendly.
const NUM_FILES: usize = 700;

fn tgrep_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("tgrep")
}

/// Create a fixture where every file contains a unique, greppable token
/// (`UNIQUETOKEN<i>`) so we can assert exact per-file coverage after the build.
fn setup_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();
    for i in 0..NUM_FILES {
        let content = format!(
            "fn handler_{i}() {{\n    // marker UNIQUETOKEN{i}\n    let value_{i} = {i};\n}}\n"
        );
        fs::write(src.join(format!("mod_{i:05}.rs")), content).unwrap();
    }
    dir
}

struct ServerGuard {
    child: Child,
    port: u16,
    index_dir: PathBuf,
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

/// Start `tgrep serve` with a 1 MB memory cap so the indexer flushes mid-build.
fn start_capped_server(root: &Path) -> ServerGuard {
    let index_dir = root.join(".tgrep_memcap_index");
    fs::create_dir_all(&index_dir).unwrap();

    let child = Command::new(tgrep_bin())
        .args([
            "serve",
            "--no-watch",
            "--max-memory",
            "1", // 1 MB — process baseline already exceeds this, forcing flushes
            "--index-path",
            index_dir.to_str().unwrap(),
            root.to_str().unwrap(),
        ])
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("failed to start tgrep serve");

    let serve_json = index_dir.join("serve.json");
    let start = Instant::now();
    let port = loop {
        assert!(
            start.elapsed() <= Duration::from_secs(30),
            "tgrep serve did not start within 30s"
        );
        if let Ok(data) = fs::read_to_string(&serve_json)
            && let Ok(info) = serde_json::from_str::<serde_json::Value>(&data)
            && let Some(p) = info.get("port").and_then(|v| v.as_u64())
            && TcpStream::connect(format!("127.0.0.1:{p}")).is_ok()
        {
            break p as u16;
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    ServerGuard {
        child,
        port,
        index_dir,
    }
}

/// Poll status until the background build reports it has finished indexing.
fn wait_for_indexing_done(port: u16) {
    let start = Instant::now();
    loop {
        assert!(
            start.elapsed() <= Duration::from_secs(120),
            "indexing did not finish within 120s"
        );
        if let Ok(resp) = send_request(port, r#"{"jsonrpc":"2.0","method":"status","id":0}"#)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&resp)
            && v.pointer("/result/indexing").and_then(|v| v.as_bool()) == Some(false)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Poll the on-disk meta.json until the final flush marks the index complete.
fn wait_for_complete_meta(index_dir: &Path) -> serde_json::Value {
    let meta_path = index_dir.join("meta.json");
    let start = Instant::now();
    loop {
        assert!(
            start.elapsed() <= Duration::from_secs(120),
            "index never reached complete=true"
        );
        if let Ok(data) = fs::read_to_string(&meta_path)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&data)
            && v.get("complete").and_then(|c| c.as_bool()) == Some(true)
        {
            return v;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn search_count(port: u16, pattern: &str) -> u64 {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "search",
        "id": 1,
        "params": { "pattern": pattern }
    })
    .to_string();
    let resp = send_request(port, &req).expect("search request failed");
    let v: serde_json::Value = serde_json::from_str(&resp).expect("invalid JSON");
    assert!(v.get("error").is_none(), "search error: {v}");
    v.pointer("/result/num_matches")
        .and_then(|n| n.as_u64())
        .expect("missing num_matches")
}

#[test]
fn memory_capped_build_still_produces_complete_searchable_index() {
    let dir = setup_fixture();
    let root = dir.path().join("src");
    let server = start_capped_server(&root);
    let port = server.port;

    wait_for_indexing_done(port);
    let meta = wait_for_complete_meta(&server.index_dir);

    // Every file made it into the on-disk index.
    assert_eq!(
        meta.get("num_files").and_then(|n| n.as_u64()),
        Some(NUM_FILES as u64),
        "complete index must contain all files; meta = {meta}"
    );

    // A token shared by every file resolves to every file — full coverage.
    assert_eq!(
        search_count(port, "UNIQUETOKEN"),
        NUM_FILES as u64,
        "all files should be searchable after a memory-capped build"
    );

    // Tokens unique to the very first and very last files (the last one indexed
    // only after several incremental flushes) are both findable.
    assert_eq!(search_count(port, "UNIQUETOKEN0\\b"), 1);
    assert_eq!(
        search_count(port, &format!("UNIQUETOKEN{}\\b", NUM_FILES - 1)),
        1,
        "file indexed in the final batch (post-flush) must be searchable"
    );
}
