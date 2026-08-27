//! The watcher must not take OS subscriptions for trees it is going to ignore.
//!
//! On Linux `notify` has no recursive inotify mode: `RecursiveMode::Recursive`
//! walks the tree and spends one watch descriptor per directory. Ignored build
//! output is usually most of the directories in a repository, so subscribing to
//! it burns the per-user `fs.inotify.max_user_watches` budget on events that
//! are then discarded — and because notify propagates the first registration
//! failure, a repo large enough to exhaust that budget loses its watcher
//! entirely.
//!
//! This is observable: the kernel reports a process's inotify watches in
//! `/proc/<pid>/fdinfo/<fd>`, one `inotify wd:` line per watch. The test below
//! counts them for a live server and pins that the ignored subtree is absent,
//! while asserting the watcher still works — a watcher that registered nothing
//! at all would trivially satisfy the count.
//!
//! Windows (ReadDirectoryChangesW) and macOS (FSEvents) subscribe once for the
//! whole subtree, so there is no per-directory registration to withhold and
//! nothing here applies; the count assertion is Linux-only for that reason.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Directories inside the ignored tree. Large enough that a recursive
/// subscription is unmistakable in the watch count, small enough to create
/// quickly.
#[cfg(target_os = "linux")]
const IGNORED_DIRS: usize = 60;
/// Directories inside the indexed tree.
#[cfg(target_os = "linux")]
const SOURCE_DIRS: usize = 4;

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

/// Read `(pid, port)` once the server is accepting connections.
fn wait_for_server(index_dir: &Path) -> (u32, u16) {
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
            && let Some(pid) = info.get("pid").and_then(|v| v.as_u64())
            && TcpStream::connect(format!("127.0.0.1:{p}")).is_ok()
        {
            return (pid as u32, p as u16);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Total inotify watch descriptors held by `pid`.
///
/// Each inotify file descriptor's `fdinfo` lists one `inotify wd:` line per
/// watch, so summing them across the process's descriptors gives the number of
/// directories it is subscribed to.
#[cfg(target_os = "linux")]
fn inotify_watch_count(pid: u32) -> usize {
    let dir = format!("/proc/{pid}/fdinfo");
    let Ok(entries) = fs::read_dir(&dir) else {
        panic!("could not read {dir}; /proc must be mounted for this test");
    };
    let mut total = 0;
    for entry in entries.flatten() {
        // Descriptors come and go while we read; a vanished one is not a
        // failure, it simply holds no watches we can count.
        if let Ok(contents) = fs::read_to_string(entry.path()) {
            total += contents
                .lines()
                .filter(|line| line.starts_with("inotify wd:"))
                .count();
        }
    }
    total
}

#[cfg(target_os = "linux")]
#[test]
fn watcher_does_not_subscribe_to_gitignored_directories() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let index_dir = root.join(".tgrep_test_index");

    // A real (if empty) `.git` entry: `.gitignore` rules only apply inside a
    // repository, so without it the fixture would measure the wrong thing.
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join(".gitignore"), "build/\n").unwrap();

    for i in 0..IGNORED_DIRS {
        let sub = root.join("build").join(format!("out{i}"));
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("artifact.txt"), "ignored_build_output\n").unwrap();
    }
    for i in 0..SOURCE_DIRS {
        let sub = root.join("src").join(format!("pkg{i}"));
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            sub.join("lib.rs"),
            format!("fn seeded{i}() {{ let normal_source_marker = {i}; }}\n"),
        )
        .unwrap();
    }

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
    let _server = ServerGuard { child };

    let (pid, port) = wait_for_server(&index_dir);

    // Positive control, and a wait for the watcher to be live: subscriptions
    // are deferred until the ignore matcher is published, so counting before
    // that would pass for the wrong reason.
    assert!(
        wait_for_match(port, "normal_source_marker", Duration::from_secs(30)),
        "expected the seeded source files to be searchable"
    );
    fs::write(
        root.join("src").join("added.rs"),
        "fn added() { let watcher_added_marker = 1; }\n",
    )
    .unwrap();
    assert!(
        wait_for_match(port, "watcher_added_marker", Duration::from_secs(30)),
        "watcher never indexed a newly created ordinary file, so the watch \
         count below would be meaningless"
    );

    let watches = inotify_watch_count(pid);

    // The tree the watcher legitimately needs is the root, `src`, `src/pkg*`,
    // and `.git` is hidden so it is not watched either. Allow generous slack
    // for anything else in the process holding an inotify fd, but stay far
    // below the ~66 a recursive subscription over this fixture would take.
    let allowed = SOURCE_DIRS + 8;
    assert!(
        watches <= allowed,
        "watcher holds {watches} inotify watches for a tree whose only \
         non-ignored directories are the root plus {SOURCE_DIRS} under src/; \
         it is subscribing to the {IGNORED_DIRS} gitignored directories under \
         build/ (expected at most {allowed})"
    );
}

/// New directories still have to be picked up.
///
/// Non-recursive subscriptions are not extended by notify, so a directory
/// created after startup is invisible unless the watcher subscribes to it as
/// it appears. This runs everywhere: on a whole-subtree backend it simply
/// re-confirms the existing behaviour, which is the point — the two paths must
/// agree.
#[test]
fn watcher_indexes_files_in_directories_created_after_startup() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let index_dir = root.join(".tgrep_test_index");

    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join(".gitignore"), "build/\n").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src").join("lib.rs"),
        "fn seeded() { let normal_source_marker = 1; }\n",
    )
    .unwrap();

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
    let _server = ServerGuard { child };

    let (_pid, port) = wait_for_server(&index_dir);
    assert!(
        wait_for_match(port, "normal_source_marker", Duration::from_secs(30)),
        "expected the seeded source file to be searchable"
    );
    thread::sleep(Duration::from_secs(2));

    // A whole new subtree, several levels deep, written in one go. The files
    // land immediately after their directories, which is exactly the race the
    // subscription pass has to close.
    let nested = root.join("src").join("fresh").join("deeper");
    fs::create_dir_all(&nested).unwrap();
    fs::write(
        nested.join("new.rs"),
        "fn fresh() { let nested_new_dir_marker = 2; }\n",
    )
    .unwrap();

    assert!(
        wait_for_match(port, "nested_new_dir_marker", Duration::from_secs(30)),
        "watcher never indexed a file created in a directory that did not \
         exist when the server started"
    );

    // ...and a directory created inside the ignored tree stays ignored.
    let ignored = root.join("build").join("fresh");
    fs::create_dir_all(&ignored).unwrap();
    fs::write(ignored.join("out.txt"), "new_ignored_dir_marker\n").unwrap();
    thread::sleep(Duration::from_secs(3));
    assert_eq!(
        search_matches(port, "new_ignored_dir_marker"),
        0,
        "watcher indexed a file in a directory created under a gitignored path"
    );
}
