//! Integration tests for concurrent search requests to the tgrep server.
//!
//! Starts a `tgrep serve` instance and fires multiple parallel search requests
//! over TCP to verify the server handles concurrency correctly.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use std::{env, fs};

use tempfile::TempDir;

/// Create a temp directory with enough files to make concurrent searches meaningful.
fn setup_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("src");
    fs::create_dir_all(&sub).unwrap();

    // Create a variety of source files
    for i in 0..20 {
        let content = format!(
            "fn function_{i}() {{\n    let value = {i};\n    println!(\"result: {{}}\", value);\n}}\n\n\
             pub struct Widget{i} {{\n    name: String,\n    count: u32,\n}}\n\n\
             impl Widget{i} {{\n    pub fn new() -> Self {{\n        Widget{i} {{ name: String::new(), count: 0 }}\n    }}\n}}\n"
        );
        fs::write(sub.join(format!("mod_{i}.rs")), content).unwrap();
    }

    // A few files with unique content for targeted searches
    fs::write(
        sub.join("unique_alpha.rs"),
        "fn alpha_handler() {\n    let alpha_value = 42;\n    process_alpha(alpha_value);\n}\n",
    )
    .unwrap();

    fs::write(
        sub.join("unique_beta.rs"),
        "fn beta_handler() {\n    let beta_value = 99;\n    process_beta(beta_value);\n}\n",
    )
    .unwrap();

    dir
}

/// Finds the tgrep binary path from cargo build output.
fn tgrep_bin() -> std::path::PathBuf {
    let mut path = env::current_exe().unwrap();
    // Go up from deps dir to the target debug/release dir
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("tgrep");
    if cfg!(windows) {
        path.set_extension("exe");
    }
    assert!(
        path.exists(),
        "tgrep binary not found at {}",
        path.display()
    );
    path
}

struct ServerGuard {
    child: Child,
    port: u16,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts a tgrep server and waits until it's ready to accept connections.
fn start_server(root: &Path) -> ServerGuard {
    let index_dir = root.join(".tgrep_test_index");
    fs::create_dir_all(&index_dir).unwrap();

    let child = Command::new(tgrep_bin())
        .args([
            "serve",
            "--no-watch",
            "--index-path",
            index_dir.to_str().unwrap(),
            root.to_str().unwrap(),
        ])
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("failed to start tgrep serve");

    // Read stderr to find the port
    let _stderr = child.stderr.as_ref().unwrap();

    // Wait for serve.json to appear
    let serve_json = index_dir.join("serve.json");
    let start = Instant::now();
    let port = loop {
        if start.elapsed() > Duration::from_secs(30) {
            panic!("tgrep serve did not start within 30 seconds");
        }
        if let Ok(data) = fs::read_to_string(&serve_json)
            && let Ok(info) = serde_json::from_str::<serde_json::Value>(&data)
            && let Some(p) = info.get("port").and_then(|v| v.as_u64())
        {
            // Verify we can connect
            if TcpStream::connect(format!("127.0.0.1:{p}")).is_ok() {
                break p as u16;
            }
        }
        thread::sleep(Duration::from_millis(100));
    };

    // Wait for indexing to complete by polling status
    let start = Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(60) {
            // Proceed anyway; tests may still pass with partial index
            break;
        }
        if let Ok(resp) = send_request(port, r#"{"jsonrpc":"2.0","method":"status","id":0}"#)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&resp)
        {
            let indexing = v
                .pointer("/result/indexing")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            if !indexing {
                break;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }

    ServerGuard { child, port }
}

/// Send a single JSON-RPC request and read the response.
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

/// Build a search JSON-RPC request.
fn search_request(id: u64, pattern: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "search",
        "id": id,
        "params": { "pattern": pattern }
    })
    .to_string()
}

fn search_request_with_opts(id: u64, pattern: &str, opts: serde_json::Value) -> String {
    let mut params = opts;
    params.as_object_mut().unwrap().insert(
        "pattern".to_string(),
        serde_json::Value::String(pattern.to_string()),
    );
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "search",
        "id": id,
        "params": params
    })
    .to_string()
}

// ─── Concurrent search tests ─────────────────────────────────────────

#[test]
fn concurrent_identical_searches() {
    let dir = setup_fixture();
    let server = start_server(dir.path().join("src").as_path());
    let port = server.port;

    // Fire 10 identical search requests in parallel
    let handles: Vec<_> = (0..10)
        .map(|i| {
            thread::spawn(move || {
                let req = search_request(i, "function");
                let resp = send_request(port, &req).expect("request failed");
                let v: serde_json::Value = serde_json::from_str(&resp).expect("invalid JSON");
                assert!(v.get("error").is_none(), "got error: {v}");
                let num_matches = v
                    .pointer("/result/num_matches")
                    .and_then(|n| n.as_u64())
                    .expect("missing num_matches");
                (i, num_matches)
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // All concurrent requests should return the same result count
    let first_count = results[0].1;
    assert!(first_count > 0, "expected matches for 'function'");
    for (id, count) in &results {
        assert_eq!(
            *count, first_count,
            "request {id} got {count} matches, expected {first_count}"
        );
    }
}

#[test]
fn concurrent_different_searches() {
    let dir = setup_fixture();
    let server = start_server(dir.path().join("src").as_path());
    let port = server.port;

    let patterns = vec![
        "function",
        "Widget",
        "String",
        "println",
        "pub struct",
        "impl",
        "new",
        "count",
        "value",
        "let",
    ];

    let patterns = Arc::new(patterns);

    // Fire different searches concurrently
    let handles: Vec<_> = (0..patterns.len())
        .map(|i| {
            let patterns = Arc::clone(&patterns);
            thread::spawn(move || {
                let req = search_request(i as u64, patterns[i]);
                let resp = send_request(port, &req).expect("request failed");
                let v: serde_json::Value = serde_json::from_str(&resp).expect("invalid JSON");
                assert!(
                    v.get("error").is_none(),
                    "got error for '{}': {v}",
                    patterns[i]
                );
                let num_matches = v
                    .pointer("/result/num_matches")
                    .and_then(|n| n.as_u64())
                    .expect("missing num_matches");
                (patterns[i], num_matches)
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Each pattern should have at least one match
    for (pattern, count) in &results {
        assert!(
            *count > 0,
            "expected matches for pattern '{pattern}', got 0"
        );
    }
}

#[test]
fn concurrent_searches_with_different_options() {
    let dir = setup_fixture();
    let server = start_server(dir.path().join("src").as_path());
    let port = server.port;

    // Fire searches with varying options concurrently
    let handles: Vec<_> = vec![
        thread::spawn(move || {
            let req = search_request_with_opts(
                1,
                "widget",
                serde_json::json!({"case_insensitive": true}),
            );
            let resp = send_request(port, &req).expect("request failed");
            let v: serde_json::Value = serde_json::from_str(&resp).expect("invalid JSON");
            assert!(
                v.get("error").is_none(),
                "case insensitive search failed: {v}"
            );
            let n = v
                .pointer("/result/num_matches")
                .and_then(|n| n.as_u64())
                .unwrap();
            assert!(
                n > 0,
                "case_insensitive search for 'widget' should match Widget"
            );
            ("case_insensitive", n)
        }),
        thread::spawn(move || {
            let req = search_request_with_opts(2, "fn", serde_json::json!({"files_only": true}));
            let resp = send_request(port, &req).expect("request failed");
            let v: serde_json::Value = serde_json::from_str(&resp).expect("invalid JSON");
            assert!(v.get("error").is_none(), "files_only search failed: {v}");
            let n = v
                .pointer("/result/num_matches")
                .and_then(|n| n.as_u64())
                .unwrap();
            assert!(n > 0, "files_only search for 'fn' should find files");
            ("files_only", n)
        }),
        thread::spawn(move || {
            // max_count is per-file, so use max_count=1 to limit each file to 1 match
            let req = search_request_with_opts(3, "value", serde_json::json!({"max_count": 1}));
            let resp = send_request(port, &req).expect("request failed");
            let v: serde_json::Value = serde_json::from_str(&resp).expect("invalid JSON");
            assert!(v.get("error").is_none(), "max_count search failed: {v}");
            let matches = v
                .pointer("/result/matches")
                .and_then(|m| m.as_array())
                .unwrap();
            // With max_count=1, each file should contribute at most 1 match
            let mut files_seen = std::collections::HashMap::new();
            for m in matches {
                let file = m.get("file").and_then(|f| f.as_str()).unwrap_or("");
                *files_seen.entry(file.to_string()).or_insert(0u64) += 1;
            }
            for (file, count) in &files_seen {
                assert!(
                    *count <= 1,
                    "file {file} has {count} matches with max_count=1"
                );
            }
            ("max_count", matches.len() as u64)
        }),
        thread::spawn(move || {
            let req =
                search_request_with_opts(4, "alpha_handler", serde_json::json!({"glob": ["*.rs"]}));
            let resp = send_request(port, &req).expect("request failed");
            let v: serde_json::Value = serde_json::from_str(&resp).expect("invalid JSON");
            assert!(v.get("error").is_none(), "glob search failed: {v}");
            let n = v
                .pointer("/result/num_matches")
                .and_then(|n| n.as_u64())
                .unwrap();
            assert!(n > 0, "glob search for 'alpha_handler' should find matches");
            ("glob_filter", n)
        }),
        thread::spawn(move || {
            let req =
                search_request_with_opts(5, "String", serde_json::json!({"fixed_string": true}));
            let resp = send_request(port, &req).expect("request failed");
            let v: serde_json::Value = serde_json::from_str(&resp).expect("invalid JSON");
            assert!(v.get("error").is_none(), "fixed_string search failed: {v}");
            let n = v
                .pointer("/result/num_matches")
                .and_then(|n| n.as_u64())
                .unwrap();
            assert!(
                n > 0,
                "fixed_string search for 'String' should find matches"
            );
            ("fixed_string", n)
        }),
    ];

    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn concurrent_high_volume_searches() {
    let dir = setup_fixture();
    let server = start_server(dir.path().join("src").as_path());
    let port = server.port;

    // Stress test: 50 concurrent requests
    let num_requests = 50;
    let handles: Vec<_> = (0..num_requests)
        .map(|i| {
            thread::spawn(move || {
                let pattern = match i % 5 {
                    0 => "fn",
                    1 => "struct",
                    2 => "impl",
                    3 => "pub",
                    _ => "let",
                };
                let req = search_request(i, pattern);
                let resp = send_request(port, &req).expect("request failed");
                let v: serde_json::Value = serde_json::from_str(&resp).expect("invalid JSON");
                assert!(
                    v.get("error").is_none(),
                    "request {i} (pattern={pattern}) got error: {v}"
                );
                let num_matches = v
                    .pointer("/result/num_matches")
                    .and_then(|n| n.as_u64())
                    .expect("missing num_matches");
                assert!(
                    num_matches > 0,
                    "request {i} (pattern={pattern}) got 0 matches"
                );
                num_matches
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Verify all requests completed successfully (they did if we got here)
    assert_eq!(results.len(), num_requests as usize);
}

#[test]
fn concurrent_searches_on_single_connection() {
    let dir = setup_fixture();
    let server = start_server(dir.path().join("src").as_path());
    let port = server.port;

    // Multiple threads sharing separate connections but also test pipelining
    // on a single connection: send multiple requests before reading responses
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();

    let patterns = ["fn", "struct", "impl", "pub", "let"];

    // Send all requests
    for (i, pattern) in patterns.iter().enumerate() {
        let req = search_request(i as u64, pattern);
        writeln!(stream, "{req}").unwrap();
    }
    stream.flush().unwrap();

    // Read all responses
    let mut reader = BufReader::new(stream);
    for (i, pattern) in patterns.iter().enumerate() {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).expect("invalid JSON response");
        assert!(
            v.get("error").is_none(),
            "pipelined request {i} (pattern={pattern}) got error: {v}"
        );
        let num_matches = v
            .pointer("/result/num_matches")
            .and_then(|n| n.as_u64())
            .expect("missing num_matches");
        assert!(
            num_matches > 0,
            "pipelined request {i} (pattern={pattern}) got 0 matches"
        );
    }
}

#[test]
fn concurrent_search_and_status_requests() {
    let dir = setup_fixture();
    let server = start_server(dir.path().join("src").as_path());
    let port = server.port;

    // Mix search and status requests concurrently
    let handles: Vec<_> = (0..20)
        .map(|i| {
            thread::spawn(move || {
                if i % 3 == 0 {
                    // Status request
                    let req = r#"{"jsonrpc":"2.0","method":"status","id":100}"#;
                    let resp = send_request(port, req).expect("status request failed");
                    let v: serde_json::Value = serde_json::from_str(&resp).expect("invalid JSON");
                    assert!(v.get("error").is_none(), "status request failed: {v}");
                    assert!(
                        v.pointer("/result/num_files").is_some(),
                        "status missing num_files"
                    );
                } else {
                    // Search request
                    let req = search_request(i, "fn");
                    let resp = send_request(port, &req).expect("search request failed");
                    let v: serde_json::Value = serde_json::from_str(&resp).expect("invalid JSON");
                    assert!(v.get("error").is_none(), "search request {i} failed: {v}");
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}
