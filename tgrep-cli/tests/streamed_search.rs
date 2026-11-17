//! A server sends a search's rows in parts when the request asks it to stream:
//! `partial` replies as chunks of candidate files are searched, then the final
//! reply with the rest. The CLI asks for that whenever its output is one
//! file's rows after another, and prints the parts as they arrive.

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
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
fn a_streamed_search_arrives_in_parts_that_add_up_to_the_whole() {
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
