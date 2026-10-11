//! Regression test for the warm-start gitignore gap.
//!
//! On a warm start (a complete index already on disk) the server does not run a
//! background build, so its `indexing` flag is false from the very first
//! filesystem event. The `.gitignore` matcher, however, is built on a
//! background thread and is not available immediately. If the watcher is
//! allowed to run in that gap it sees `gitignore == None`, applies no ignore
//! rules, and indexes exactly the build output the walker deliberately skips —
//! permanently polluting the index with files a search should never return.
//!
//! This test pins the watcher shut until the matcher is published.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// A repository whose `.gitignore` excludes `build/`.
///
/// The `.git` directory is real (if empty) on purpose: the `ignore` crate
/// applies `.gitignore` rules only when it can see a git repository, so
/// without it the walker would ignore the ignore file and the test would be
/// measuring the wrong thing entirely. An empty directory is enough for that
/// check, which keeps the fixture independent of the `git` binary.
fn setup_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join(".gitignore"), "build/\n").unwrap();
    fs::create_dir_all(root.join("build")).unwrap();

    // Enough files that building the matcher takes long enough for the
    // writer loop below to land inside the startup window.
    for pkg in 0..40 {
        let sub = root.join("src").join(format!("pkg{pkg}"));
        fs::create_dir_all(&sub).unwrap();
        for f in 0..50 {
            fs::write(
                sub.join(format!("file{f}.rs")),
                format!("fn tracked_{pkg}_{f}() {{ let normal_source_marker = {f}; }}\n"),
            )
            .unwrap();
        }
    }

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

#[test]
fn warm_start_cleans_abandoned_build_output_before_serving() {
    let fixture = TempDir::new().unwrap();
    let root = fixture.path();
    let index_dir = root.join(".tgrep");
    fs::write(root.join("source.rs"), "fn startup_cleanup_marker() {}\n").unwrap();
    tgrep_core::builder::build_index(root, Some(&index_dir), true, false, &[]).unwrap();
    let artifacts = [
        ".reload-build",
        ".filename-index-staging",
        ".stale-delta",
        ".stale-merge",
        ".flush-staging",
        "spill-123-0.tmp",
    ];
    for name in artifacts {
        let path = index_dir.join(name);
        fs::create_dir(&path).unwrap();
        fs::write(path.join("segment.bin"), b"abandoned build").unwrap();
    }
    let pending = index_dir.join(".retired").join("generation-123-1");
    fs::create_dir_all(&pending).unwrap();
    fs::write(
        pending.join("index.bin"),
        b"recover interrupted publication",
    )
    .unwrap();
    fs::write(index_dir.join("unrelated.tmp"), b"keep").unwrap();
    let log = fs::File::create(index_dir.join("test-startup.log")).unwrap();
    let child = Command::new(tgrep_bin())
        .args(["serve", "--no-watch", "--index-path"])
        .arg(&index_dir)
        .arg(root)
        .stderr(log)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _server = ServerGuard { child };
    let port = wait_for_port(&index_dir);

    for name in artifacts {
        assert!(!index_dir.join(name).exists(), "{name}");
    }
    assert_eq!(
        fs::read(pending.join("index.bin")).unwrap(),
        b"recover interrupted publication"
    );
    assert_eq!(fs::read(index_dir.join("unrelated.tmp")).unwrap(), b"keep");
    assert_eq!(search_matches(port, "startup_cleanup_marker"), 1);

    let active_spill = index_dir.join("spill-123-1.tmp");
    fs::write(&active_spill, b"active server output").unwrap();
    let output = assert_cmd::Command::new(tgrep_bin())
        .timeout(Duration::from_secs(30))
        .args(["index", "--index-path"])
        .arg(&index_dir)
        .arg(root)
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("another tgrep server is already running or an index build")
    );
    assert_eq!(fs::read(active_spill).unwrap(), b"active server output");
}

#[test]
fn warm_start_watcher_does_not_index_gitignored_files() {
    let fixture = setup_fixture();
    let root = fixture.path();
    let index_dir = root.join(".tgrep_test_index");

    // Build a complete index up front so `serve` takes the warm-start path
    // (no background build, so `indexing` is false immediately).
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

    // Hammer a gitignored file across the *whole* startup window. The writer
    // starts before `serve` is spawned and keeps going until well after the
    // port is up, so however long startup takes, writes straddle both the
    // moment the watcher comes live and the moment the matcher lands.
    //
    // A fixed-duration loop that ran before waiting for readiness would be
    // unsound as a regression test: on a slow or loaded runner every write
    // could complete before the watcher existed, no event would ever reach
    // the gap, and the test would pass having exercised nothing.
    let ignored = root.join("build").join("leaked.txt");
    let stop = Arc::new(AtomicBool::new(false));
    let writes = Arc::new(AtomicU64::new(0));
    let writer = {
        let stop = Arc::clone(&stop);
        let writes = Arc::clone(&writes);
        thread::spawn(move || {
            let mut last_err = None;
            while !stop.load(Ordering::Relaxed) {
                match fs::write(&ignored, "gitignored_leak_marker build artifact\n") {
                    Ok(()) => {
                        writes.fetch_add(1, Ordering::Relaxed);
                    }
                    // Kept rather than swallowed so a probe that never ran
                    // reports why instead of masquerading as a pass.
                    Err(e) => last_err = Some(e),
                }
                thread::sleep(Duration::from_millis(10));
            }
            last_err
        })
    };

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

    // Take ownership immediately so the child is reaped even if the readiness
    // wait below times out and panics.
    let _server = ServerGuard { child };

    let port = wait_for_port(&index_dir);

    // Keep writing past readiness: the matcher is built on a background
    // thread and can land well after the port opens.
    thread::sleep(Duration::from_secs(2));
    stop.store(true, Ordering::Relaxed);
    let last_err = writer.join().expect("writer thread panicked");

    assert!(
        writes.load(Ordering::Relaxed) > 0,
        "the gitignored probe file was never written, so this test proved \
         nothing about the warm-start window: {last_err:?}"
    );

    // Let the stale check and any resulting flush settle.
    thread::sleep(Duration::from_secs(3));

    // Positive control: without this a broken search path would let the
    // real assertion below pass for entirely the wrong reason.
    assert!(
        search_matches(port, "normal_source_marker") > 0,
        "expected the indexed source files to be searchable"
    );

    assert_eq!(
        search_matches(port, "gitignored_leak_marker"),
        0,
        "watcher indexed a gitignored file during the warm-start window \
         before the .gitignore matcher was published"
    );
}
