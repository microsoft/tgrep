use assert_cmd::Command;
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn tgrep() -> Command {
    Command::cargo_bin("tgrep").unwrap()
}

fn tgrep_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("tgrep")
}

fn fixture() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    let index = temp.path().join("index");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("src").join("notes.txt"), "notes\n").unwrap();
    fs::write(root.join("asset.bin"), [1, 2, 3]).unwrap();
    fs::write(root.join("binary.txt"), b"before\0after").unwrap();
    (temp, root, index)
}

fn build_index(root: &Path, index: &Path) {
    tgrep()
        .args([
            "index",
            root.to_str().unwrap(),
            "--index-path",
            index.to_str().unwrap(),
        ])
        .assert()
        .success();
}

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_server(root: &Path, index: &Path) -> ServerGuard {
    let mut child = std::process::Command::new(tgrep_bin())
        .args([
            "serve",
            "--no-watch",
            "--index-path",
            index.to_str().unwrap(),
            root.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let serve_json = index.join("serve.json");
    let started = Instant::now();

    loop {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "server did not start"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "server exited before accepting connections"
        );
        if let Ok(data) = fs::read_to_string(&serve_json)
            && let Ok(info) = serde_json::from_str::<serde_json::Value>(&data)
            && let Some(port) = info.get("port").and_then(serde_json::Value::as_u64)
            && TcpStream::connect(("127.0.0.1", port as u16)).is_ok()
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    ServerGuard(child)
}

fn output_lines(command: &mut Command) -> Vec<String> {
    let output = command.assert().success().get_output().stdout.clone();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| line.replace('\\', "/"))
        .collect()
}

#[test]
fn indexed_files_include_non_content_paths_and_use_the_snapshot() {
    let (_temp, root, index) = fixture();
    build_index(&root, &index);
    fs::write(root.join("created-after-index.rs"), "fn late() {}\n").unwrap();

    let indexed = output_lines(tgrep().args([
        "--files",
        "--sort",
        "path",
        root.to_str().unwrap(),
        "--index-path",
        index.to_str().unwrap(),
    ]));
    assert!(indexed.iter().any(|path| path.ends_with("/src/main.rs")));
    assert!(indexed.iter().any(|path| path.ends_with("/asset.bin")));
    assert!(indexed.iter().any(|path| path.ends_with("/binary.txt")));
    assert!(
        !indexed
            .iter()
            .any(|path| path.ends_with("/created-after-index.rs")),
        "the indexed path unexpectedly walked the live tree: {indexed:?}"
    );

    let walked = output_lines(tgrep().args([
        "--files",
        "--no-index",
        root.to_str().unwrap(),
        "--index-path",
        index.to_str().unwrap(),
    ]));
    assert!(
        walked
            .iter()
            .any(|path| path.ends_with("/created-after-index.rs"))
    );

    let case_insensitive_ignore = output_lines(tgrep().args([
        "--files",
        "--ignore-file-case-insensitive",
        root.to_str().unwrap(),
        "--index-path",
        index.to_str().unwrap(),
    ]));
    assert!(
        case_insensitive_ignore
            .iter()
            .any(|path| path.ends_with("/created-after-index.rs")),
        "the traversal-changing ignore flag unexpectedly used the snapshot"
    );
}

#[test]
fn legacy_filename_index_falls_back_and_indexed_scope_still_filters() {
    let (_temp, root, index) = fixture();
    build_index(&root, &index);

    let scoped = output_lines(tgrep().args([
        "--files",
        "--sort",
        "path",
        "-t",
        "rust",
        root.join("src").to_str().unwrap(),
        "--index-path",
        index.to_str().unwrap(),
    ]));
    assert_eq!(scoped.len(), 1, "unexpected scoped output: {scoped:?}");
    assert!(scoped[0].ends_with("/src/main.rs"));

    fs::remove_file(index.join(tgrep_core::path_index::EXTRA_PATHS_FILENAME)).unwrap();
    fs::write(root.join("legacy-live.rs"), "fn legacy() {}\n").unwrap();
    let fallback = output_lines(tgrep().args([
        "--files",
        root.to_str().unwrap(),
        "--index-path",
        index.to_str().unwrap(),
    ]));
    assert!(
        fallback
            .iter()
            .any(|path| path.ends_with("/legacy-live.rs")),
        "a legacy index did not fall back to walking: {fallback:?}"
    );
}

#[test]
fn files_uses_the_live_server_filename_index() {
    let (_temp, root, index) = fixture();
    build_index(&root, &index);
    let _server = start_server(&root, &index);

    // Make the local sidecar incomplete after the server has loaded its copy.
    // The binary path below can therefore come only from the files RPC.
    tgrep_core::path_index::write_extra_paths(&index, &[]).unwrap();
    let listed = output_lines(tgrep().args([
        "--files",
        root.to_str().unwrap(),
        "--index-path",
        index.to_str().unwrap(),
    ]));

    assert!(
        listed.iter().any(|path| path.ends_with("/asset.bin")),
        "the client did not use the server's filename index: {listed:?}"
    );
}
