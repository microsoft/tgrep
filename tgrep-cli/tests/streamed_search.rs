//! A server sends a search's rows in parts when the request asks it to stream:
//! `partial` replies as chunks of candidate files are searched, then the final
//! reply with the rest. The CLI asks for that whenever its output is one
//! file's rows after another, and prints the parts as they arrive.

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const MARKER: &str = "streamed_marker";
/// More than the first two chunks of a streamed search hold, so the reply
/// has to come in several parts.
const FILES: usize = 400;

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

/// `FILES` files with one matching line each, spread over twenty directories.
fn setup_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    for file in 0..FILES {
        let parent = dir.path().join(format!("dir{:02}", file % 20));
        fs::create_dir_all(&parent).unwrap();
        fs::write(
            parent.join(format!("file{file:03}.txt")),
            format!("first line\n{MARKER} {file}\n"),
        )
        .unwrap();
    }
    dir
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

/// Every reply to one search request: the `partial` ones, then the final one.
fn search_replies(port: u16, params: serde_json::Value) -> Vec<serde_json::Value> {
    let request =
        serde_json::json!({"jsonrpc": "2.0", "method": "search", "id": 7, "params": params});
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    writeln!(stream, "{request}").unwrap();
    stream.flush().unwrap();
    let mut reader = BufReader::new(stream);
    let mut replies = Vec::new();
    loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "the connection closed before the final reply"
        );
        let reply: serde_json::Value = serde_json::from_str(&line).expect("invalid JSON reply");
        assert_eq!(reply["id"], 7, "{line}");
        let result = reply
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("no result in reply: {line}"));
        let last = result.get("partial").is_none();
        replies.push(result);
        if last {
            return replies;
        }
    }
}

fn files_of(result: &serde_json::Value) -> Vec<String> {
    result["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["file"].as_str().unwrap().to_string())
        .collect()
}

/// The `file:line` prefixes a CLI search printed, in output order.
fn cli_lines(args: &[&str]) -> Vec<String> {
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
            line.rsplit_once(':')
                .expect("file:line:content")
                .0
                .to_string()
        })
        .collect()
}

#[test]
fn parts_add_up() {
    let fixture = setup_fixture();
    let root = fixture.path().to_str().unwrap();
    let index = TempDir::new().unwrap();
    let index_dir = index.path().join("index");
    let index_arg = index_dir.to_str().unwrap();

    let output = Command::new(tgrep_bin())
        .args(["index", root, "--index-path", index_arg])
        .output()
        .expect("failed to run tgrep index");
    assert!(
        output.status.success(),
        "index build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let child = Command::new(tgrep_bin())
        .args(["serve", root, "--index-path", index_arg, "--no-watch"])
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn tgrep serve");
    let _guard = ServerGuard { child };
    let port = wait_for_port(&index_dir);

    let whole = search_replies(port, serde_json::json!({"pattern": MARKER}));
    assert_eq!(whole.len(), 1, "a search that did not ask replies once");
    let whole_files = files_of(&whole[0]);
    assert_eq!(whole_files.len(), FILES);
    assert_eq!(whole[0]["num_matches"], FILES);

    let parts = search_replies(port, serde_json::json!({"pattern": MARKER, "stream": true}));
    assert!(parts.len() >= 3, "{} replies", parts.len());
    let (last, earlier) = parts.split_last().unwrap();
    let mut searched = 0;
    for part in earlier {
        assert_eq!(part["partial"], true);
        assert_eq!(part["candidates"], FILES);
        // The client validates every part like a whole reply.
        assert_eq!(part["hidden_complete"], true);
        assert_eq!(part["ordered_filters"], true);
        let so_far = part["searched"].as_u64().unwrap();
        assert!(so_far > searched && so_far < FILES as u64, "{so_far}");
        searched = so_far;
    }
    assert_eq!(earlier[0]["searched"], 64, "the first part comes early");
    assert_eq!(
        last["num_matches"], FILES,
        "the final reply counts every row"
    );
    let streamed_files: Vec<String> = parts.iter().flat_map(files_of).collect();
    assert_eq!(streamed_files, whole_files, "same rows in the same order");

    // The CLI prints the same lines whether the server streamed them (plain
    // output) or replied once (`--sort` needs the whole result).
    let streamed = cli_lines(&["--index-path", index_arg, MARKER, root]);
    let sorted = cli_lines(&["--index-path", index_arg, "--sort", "path", MARKER, root]);
    assert_eq!(streamed.len(), FILES, "every line once");
    assert_eq!(
        streamed.iter().collect::<BTreeSet<_>>(),
        sorted.iter().collect::<BTreeSet<_>>()
    );
}

/// Index a fresh fixture and return it with its index directory.
fn indexed_fixture() -> (TempDir, TempDir, std::path::PathBuf) {
    let fixture = setup_fixture();
    let index = TempDir::new().unwrap();
    let index_dir = index.path().join("index");
    let output = Command::new(tgrep_bin())
        .args(["index", fixture.path().to_str().unwrap(), "--index-path"])
        .arg(&index_dir)
        .output()
        .expect("failed to run tgrep index");
    assert!(
        output.status.success(),
        "index build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (fixture, index, index_dir)
}

/// The first `partial` reply a real server sends for the fixture, for a stub
/// to replay. The server is stopped again before this returns.
fn first_part_from_a_server(root: &Path, index_dir: &Path) -> serde_json::Value {
    let child = Command::new(tgrep_bin())
        .arg("serve")
        .arg(root)
        .arg("--index-path")
        .arg(index_dir)
        .arg("--no-watch")
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn tgrep serve");
    let _guard = ServerGuard { child };
    let port = wait_for_port(index_dir);
    let mut parts = search_replies(port, serde_json::json!({"pattern": MARKER, "stream": true}));
    assert!(parts.len() >= 2, "{} replies", parts.len());
    let first = parts.swap_remove(0);
    assert_eq!(first["partial"], true);
    assert!(!first["matches"].as_array().unwrap().is_empty());
    first
}

const STUB_INSTANCE: &str = "stub-server";

/// Run `client` against a stand-in for a server: a listener recorded in
/// `serve.json` that reads one search request and leaves `serve` to answer it.
fn with_stub_server<T>(
    index_dir: &Path,
    serve: impl FnOnce(serde_json::Value, TcpStream) + Send,
    client: impl FnOnce() -> T,
) -> T {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    fs::write(
        index_dir.join("serve.json"),
        serde_json::json!({
            "pid": std::process::id(),
            "port": listener.local_addr().unwrap().port(),
            "instance": STUB_INSTANCE,
        })
        .to_string(),
    )
    .unwrap();
    thread::scope(|scope| {
        let stub = scope.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "the client did not connect");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let mut request = String::new();
            BufReader::new(&stream).read_line(&mut request).unwrap();
            serve(serde_json::from_str(&request).unwrap(), stream);
        });
        let result = client();
        stub.join().unwrap();
        result
    })
}

fn stub_reply(result: &serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": 1, "instance": STUB_INSTANCE, "result": result})
        .to_string()
}

#[test]
fn cli_prints_parts_as_they_arrive() {
    let (fixture, _index, index_dir) = indexed_fixture();
    let first = first_part_from_a_server(fixture.path(), &index_dir);
    let rows = first["matches"].as_array().unwrap().len();

    let (shown, part_shown) = mpsc::channel::<()>();
    let (status, printed) = with_stub_server(
        &index_dir,
        move |request, stream| {
            assert_eq!(request["params"]["stream"], true, "{request}");
            writeln!(&stream, "{}", stub_reply(&first)).unwrap();
            // The rest is held back until the first part is on the CLI's
            // stdout, which a client that waited for the whole reply would
            // never reach.
            part_shown
                .recv_timeout(Duration::from_secs(30))
                .expect("the first part was not printed before the final reply");
            let mut last = first.clone();
            last.as_object_mut().unwrap().remove("partial");
            last["matches"] = serde_json::json!([]);
            writeln!(&stream, "{}", stub_reply(&last)).unwrap();
        },
        || {
            let child = Command::new(tgrep_bin())
                .args(["--color", "never", "--no-heading", "-n", "--index-path"])
                .arg(&index_dir)
                .arg(MARKER)
                .arg(fixture.path())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("failed to run tgrep");
            let mut search = ServerGuard { child };
            let stdout = search.child.stdout.take().unwrap();
            let (line_out, lines) = mpsc::channel();
            let reader = thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    if line_out.send(line.unwrap()).is_err() {
                        break;
                    }
                }
            });
            let mut printed = Vec::new();
            while printed.len() < rows {
                let line = lines
                    .recv_timeout(Duration::from_secs(30))
                    .expect("the first part was not printed while the reply was still open");
                printed.push(line);
            }
            shown.send(()).unwrap();
            let status = search.child.wait().unwrap();
            reader.join().unwrap();
            printed.extend(lines.try_iter());
            (status, printed)
        },
    );
    assert!(status.success(), "{status}");
    assert_eq!(printed.len(), rows, "only the first part had rows");
}

#[test]
fn truncated_reply_is_not_retried() {
    let (fixture, _index, index_dir) = indexed_fixture();
    let first = first_part_from_a_server(fixture.path(), &index_dir);
    let rows = first["matches"].as_array().unwrap().len();
    assert!(rows < FILES);

    // The server sends one part with matches and then closes the connection.
    let output = with_stub_server(
        &index_dir,
        move |_request, stream| writeln!(&stream, "{}", stub_reply(&first)).unwrap(),
        || {
            Command::new(tgrep_bin())
                .args(["--color", "never", "--no-heading", "-n", "--index-path"])
                .arg(&index_dir)
                .arg(MARKER)
                .arg(fixture.path())
                .output()
                .expect("failed to run tgrep")
        },
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("results are incomplete"), "{stderr}");
    assert!(!stderr.contains("falling back"), "{stderr}");
    let printed: Vec<&str> = stdout.lines().collect();
    assert_eq!(printed.len(), rows, "the part's rows and nothing else");
    assert_eq!(
        printed.iter().collect::<BTreeSet<_>>().len(),
        rows,
        "every row once"
    );
}

/// The `stats` of the `summary` line of a `--json` search.
fn json_summary_stats(index_dir: &Path, root: &Path) -> (serde_json::Value, String) {
    let output = Command::new(tgrep_bin())
        .args(["--json", "--index-path"])
        .arg(index_dir)
        .arg(MARKER)
        .arg(root)
        .output()
        .expect("failed to run tgrep");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stderr}");
    let summary = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|event| event["type"] == "summary")
        .expect("a summary line");
    (summary["data"]["stats"].clone(), stderr)
}

#[test]
fn fallback_does_not_double_count() {
    let (fixture, _index, index_dir) = indexed_fixture();
    let (alone, _) = json_summary_stats(&index_dir, fixture.path());
    assert_eq!(alone["searches"], FILES);

    // A part that searched three files without a match, then the connection
    // closes: nothing was reported, so the client searches the index itself.
    let part = serde_json::json!({
        "partial": true,
        "matches": [],
        "candidates": FILES,
        "searched": 3,
        "hidden_complete": true,
        "ordered_filters": true,
        "multiline_replace": true,
        "report_replacements": true,
        "search_stats": [
            {"file": "dir00/file000.txt", "bytes_searched": 30, "binary_offset": null},
            {"file": "dir01/file001.txt", "bytes_searched": 30, "binary_offset": null},
            {"file": "dir02/file002.txt", "bytes_searched": 30, "binary_offset": null},
        ],
    });
    let (after_fallback, stderr) = with_stub_server(
        &index_dir,
        move |_request, stream| writeln!(&stream, "{}", stub_reply(&part)).unwrap(),
        || json_summary_stats(&index_dir, fixture.path()),
    );
    // The part was accepted and the reply then ended, which is what makes the
    // client fall back.
    assert!(stderr.contains("server closed the connection"), "{stderr}");
    assert!(stderr.contains("falling back"), "{stderr}");
    for counter in ["searches", "bytes_searched", "matches", "matched_lines"] {
        assert_eq!(after_fallback[counter], alone[counter], "{counter}");
    }
}
