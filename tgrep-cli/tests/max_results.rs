//! `--max-results` ends a search after the file that brings the number of
//! matching lines to the limit, whichever backend answers: a full scan, the
//! on-disk index, or a server. The server applies the limit itself, so that a
//! broad query stops reading candidate files instead of only printing fewer.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const MARKER: &str = "limited_marker";
const FILES: usize = 5;
const LINES_PER_FILE: usize = 2;

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

/// Five files in separate directories, each with two matching lines and one
/// line that does not match.
fn setup_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    for file in 0..FILES {
        let parent = dir.path().join(format!("dir{file}"));
        fs::create_dir(&parent).unwrap();
        fs::write(
            parent.join("file.txt"),
            format!("{MARKER} one\nnothing here\n{MARKER} two\n"),
        )
        .unwrap();
    }
    dir
}

/// The matching lines a search printed, as `(file, line)` in output order.
fn search(args: &[&str]) -> Vec<(String, String)> {
    let output = Command::new(tgrep_bin())
        .args(["--color", "never", "--no-heading", "-n"])
        .args(args)
        .output()
        .expect("failed to run tgrep");
    assert!(
        output.status.success(),
        "tgrep {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            let (file, rest) = line.split_once(".txt:").expect("file:line:content");
            let (number, _) = rest.split_once(':').expect("line:content");
            (file.to_string(), number.to_string())
        })
        .collect()
}

/// Whole files only: `limit` lines are reached inside the last file printed.
fn assert_limited(lines: &[(String, String)], limit: usize, backend: &str) {
    let expected_files = limit.div_ceil(LINES_PER_FILE);
    assert_eq!(
        lines.len(),
        expected_files * LINES_PER_FILE,
        "{backend}: {lines:?}"
    );
    assert_eq!(file_count(lines), expected_files, "{backend}: {lines:?}");
}

/// How many files `lines` are from.
fn file_count(lines: &[(String, String)]) -> usize {
    let mut files: Vec<&str> = lines.iter().map(|(file, _)| file.as_str()).collect();
    files.dedup();
    files.len()
}

/// An index of `root` in a directory of its own.
fn build_index(root: &str) -> (TempDir, PathBuf) {
    let index = TempDir::new().unwrap();
    let index_dir = index.path().join("index");
    let output = Command::new(tgrep_bin())
        .args(["index", root, "--index-path"])
        .arg(&index_dir)
        .output()
        .expect("failed to run tgrep index");
    assert!(
        output.status.success(),
        "index build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (index, index_dir)
}

/// A server for `root` and the port it listens on.
fn start_server(root: &str, index_dir: &Path) -> (ServerGuard, u16) {
    let child = Command::new(tgrep_bin())
        .args(["serve", root, "--no-watch", "--index-path"])
        .arg(index_dir)
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn tgrep serve");
    let guard = ServerGuard { child };
    let port = wait_for_port(index_dir);
    (guard, port)
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
            && let Some(port) = info.get("port").and_then(|v| v.as_u64())
            && TcpStream::connect(format!("127.0.0.1:{port}")).is_ok()
        {
            return port as u16;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn search_rpc(port: u16, params: serde_json::Value) -> serde_json::Value {
    let request =
        serde_json::json!({"jsonrpc": "2.0", "method": "search", "id": 1, "params": params});
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    writeln!(stream, "{request}").unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    let value: serde_json::Value = serde_json::from_str(&response).expect("invalid JSON response");
    value
        .get("result")
        .cloned()
        .unwrap_or_else(|| panic!("no result in response: {response}"))
}

#[test]
fn scan_stops_at_limit() {
    let fixture = setup_fixture();
    let root = fixture.path().to_str().unwrap();

    let mut all = search(&["--no-index", MARKER, root]);
    assert_eq!(all.len(), FILES * LINES_PER_FILE);
    all.sort();

    for limit in [1, 2, 3, 4] {
        let limit_arg = limit.to_string();
        let lines = search(&["--no-index", "--max-results", &limit_arg, MARKER, root]);
        assert_limited(&lines, limit, "scan");
    }
    // A scan visits files in no fixed order, so compare the sets.
    let mut lines = search(&["--no-index", "--max-results", "1000", MARKER, root]);
    lines.sort();
    assert_eq!(lines, all, "a limit that is not reached changes nothing");
}

#[test]
fn limit_spans_paths() {
    let fixture = setup_fixture();
    let first = fixture.path().join("dir0");
    let second = fixture.path().join("dir1");
    let lines = search(&[
        "--no-index",
        "--max-results",
        "2",
        MARKER,
        first.to_str().unwrap(),
        second.to_str().unwrap(),
    ]);
    assert_limited(&lines, 2, "two search paths");
}

#[test]
fn index_and_server_stop_at_limit() {
    let fixture = setup_fixture();
    let root = fixture.path().to_str().unwrap();
    let (_index, index_dir) = build_index(root);
    let index_arg = index_dir.to_str().unwrap();

    let indexed = search(&[
        "--index-path",
        index_arg,
        "--max-results",
        "3",
        MARKER,
        root,
    ]);
    assert_limited(&indexed, 3, "on-disk index");
    let sorted_args = [
        "--index-path",
        index_arg,
        "--sort",
        "path",
        "--max-results",
        "3",
        MARKER,
        root,
    ];
    let sorted_indexed = search(&sorted_args);
    assert_limited(&sorted_indexed, 3, "on-disk index, sorted");
    assert!(
        sorted_indexed[0].0.contains("dir0"),
        "a sorted search starts with the first path: {sorted_indexed:?}"
    );

    let (_guard, port) = start_server(root, &index_dir);

    let served = search(&[
        "--index-path",
        index_arg,
        "--max-results",
        "3",
        MARKER,
        root,
    ]);
    assert_eq!(
        served, indexed,
        "the server and the index report the same files"
    );
    // A sorted search counts its limit in sorted order, whatever order the
    // server searches in.
    assert_eq!(search(&sorted_args), sorted_indexed);

    // The server truncates the reply itself rather than leaving it to the client.
    let limited = search_rpc(
        port,
        serde_json::json!({"pattern": MARKER, "max_results": 3}),
    );
    assert_eq!(limited["matches"].as_array().unwrap().len(), 4, "{limited}");
    assert_eq!(limited["truncated"], true, "{limited}");

    let exact = search_rpc(
        port,
        serde_json::json!({"pattern": MARKER, "max_results": 4}),
    );
    assert_eq!(exact["matches"].as_array().unwrap().len(), 4, "{exact}");
    assert_eq!(exact["truncated"], true, "{exact}");

    let unreached = search_rpc(
        port,
        serde_json::json!({"pattern": MARKER, "max_results": 1000}),
    );
    assert_eq!(
        unreached["matches"].as_array().unwrap().len(),
        FILES * LINES_PER_FILE,
        "{unreached}"
    );
    assert!(unreached.get("truncated").is_none(), "{unreached}");

    let unlimited = search_rpc(port, serde_json::json!({"pattern": MARKER}));
    assert_eq!(
        unlimited["matches"].as_array().unwrap().len(),
        FILES * LINES_PER_FILE,
        "{unlimited}"
    );
    assert!(unlimited.get("truncated").is_none(), "{unlimited}");
}

/// Files of one name under separate search paths are separate files.
#[test]
fn same_name_across_paths() {
    let dir = TempDir::new().unwrap();
    let mut paths = Vec::new();
    for name in ["a", "b", "c"] {
        let path = dir.path().join(name);
        fs::create_dir(&path).unwrap();
        fs::write(path.join("file.txt"), format!("{MARKER}\n")).unwrap();
        paths.push(path.to_str().unwrap().to_string());
    }
    let mut args = vec!["--no-index", "--max-results", "2", MARKER];
    args.extend(paths.iter().map(String::as_str));
    assert_eq!(search(&args).len(), 2);
}

/// The limit counts lines of the source. A match of two lines counts as two,
/// also where it is printed as one row.
#[test]
fn multiline_counts_source_lines() {
    let fixture = setup_fixture();
    let root = fixture.path().to_str().unwrap();
    let (_index, index_dir) = build_index(root);
    let index_arg = index_dir.to_str().unwrap();
    let pattern = format!("{MARKER} one\\nnothing");

    let check = |backend: &str, backend_args: &[&str]| {
        for rendering in [&["--vimgrep"][..], &["--replace", "X"]] {
            for order in [&[][..], &["--sort", "path"]] {
                for (limit, files) in [("2", 1), ("3", 2), ("4", 2)] {
                    let mut args = vec!["-U", "--max-results", limit];
                    args.extend(backend_args);
                    args.extend(rendering);
                    args.extend(order);
                    args.extend([pattern.as_str(), root]);
                    let lines = search(&args);
                    assert_eq!(lines.len(), files, "{backend} {args:?}: one row a file");
                    assert_eq!(file_count(&lines), files, "{backend} {args:?}");
                }
            }
        }
    };
    check("scan", &["--no-index"]);
    check("on-disk index", &["--index-path", index_arg]);

    let (_guard, port) = start_server(root, &index_dir);
    check("server", &["--index-path", index_arg]);

    // The count carries from one search path to the next.
    let paths: Vec<PathBuf> = (0..3)
        .map(|at| fixture.path().join(format!("dir{at}")))
        .collect();
    let mut args = vec!["-U", "--vimgrep", "--max-results", "3"];
    args.extend(["--index-path", index_arg, pattern.as_str()]);
    args.extend(paths.iter().map(|path| path.to_str().unwrap()));
    assert_eq!(search(&args).len(), 2, "server, three paths");

    let limited = search_rpc(
        port,
        serde_json::json!({
            "pattern": pattern, "multiline": true, "vimgrep": true, "max_results": 3,
        }),
    );
    assert_eq!(limited["matches"].as_array().unwrap().len(), 2, "{limited}");
    assert_eq!(limited["truncated"], true, "{limited}");
}

/// A binary file that a directory search does not report does not use up the
/// limit either, whichever backend finds it.
#[test]
fn hidden_binary_not_counted() {
    const TEXT_FILES: usize = 4;
    let dir = TempDir::new().unwrap();
    // Binary only past the part of a file that indexing inspects, so these
    // are candidates of an indexed search too.
    let mut late_binary = format!("{MARKER}\n{}\n", "x".repeat(16 * 1024)).into_bytes();
    late_binary.push(0);
    for file in 0..64 {
        fs::write(dir.path().join(format!("binary{file}.txt")), &late_binary).unwrap();
    }
    for file in 0..TEXT_FILES {
        fs::write(
            dir.path().join(format!("text{file}.txt")),
            format!("{MARKER}\n"),
        )
        .unwrap();
    }
    let root = dir.path().to_str().unwrap();
    let (_index, index_dir) = build_index(root);
    let index_arg = index_dir.to_str().unwrap();

    // A limit of every text line is only reached by the last text file, in
    // whatever order the files are searched.
    let limit = TEXT_FILES.to_string();
    let limited = ["--max-results", &limit, MARKER, root];
    let all_text = |lines: Vec<(String, String)>, backend: &str| {
        assert_eq!(lines.len(), TEXT_FILES, "{backend}: {lines:?}");
    };
    all_text(search(&[&["--no-index"], &limited[..]].concat()), "scan");
    let indexed = [&["--index-path", index_arg], &limited[..]].concat();
    all_text(search(&indexed), "on-disk index");
    let (_guard, _port) = start_server(root, &index_dir);
    all_text(search(&indexed), "server");
}

/// `--stats` totals cover the files printed, not those the limit left out.
#[test]
fn stats_cover_reported_files() {
    let fixture = setup_fixture();
    let root = fixture.path().to_str().unwrap();
    let (_index, index_dir) = build_index(root);
    let (_guard, _port) = start_server(root, &index_dir);

    for order in [&[][..], &["--sort", "path"]] {
        let output = Command::new(tgrep_bin())
            .args(["--stats", "--max-results", "3", "--index-path"])
            .arg(&index_dir)
            .args(order)
            .args([MARKER, root])
            .output()
            .expect("failed to run tgrep");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("via server"), "{order:?}: {stderr}");
        // Two files of two lines each reach a limit of three.
        assert!(
            stderr.contains("4 matches (4 matched lines)"),
            "{order:?}: {stderr}"
        );
    }
}

#[test]
fn conflicting_modes_rejected() {
    let fixture = setup_fixture();
    let root = fixture.path().to_str().unwrap();
    for mode in [
        "-l",
        "--files-without-match",
        "-c",
        "--count-matches",
        "-q",
        "--files",
    ] {
        let output = Command::new(tgrep_bin())
            .args(["--no-index", mode, "--max-results", "2", MARKER, root])
            .output()
            .expect("failed to run tgrep");
        assert_eq!(output.status.code(), Some(2), "{mode}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--max-results"),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
