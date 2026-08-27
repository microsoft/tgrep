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

/// Wait until `pattern` stops matching.
///
/// For content that has to be *dropped* from the index. The barrier the other
/// assertions use — a second file appearing — only proves that some later event
/// was processed, which on a backend that coalesces or reorders events (macOS)
/// says nothing about the drop.
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

/// A subtree that arrives already populated can carry its own ignore rules.
///
/// A clone, a `git mv`, a branch switch or an unpacked archive all land this
/// way: the directory and everything under it appear in one step, so there is
/// no moment at which the `.gitignore` inside it is seen on its own. The
/// recovery pass skips dot-prefixed files, so without explicitly looking for
/// ignore rules it would index the subtree against rules that never mentioned
/// it — and the wrongly indexed files would stay until something touched them
/// again or the hourly reconcile came round.
#[test]
fn watcher_honors_ignore_rules_inside_a_subtree_that_arrives_whole() {
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

    // Staged under a dot-prefixed name so the watcher ignores it while it is
    // being built, and on the same filesystem so the move below is atomic.
    let staging = root.join(".staging");
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join(".gitignore"), "secret.txt\n").unwrap();
    fs::write(staging.join("secret.txt"), "moved_subtree_secret_marker\n").unwrap();
    fs::write(
        staging.join("keep.rs"),
        "fn keep() { let moved_subtree_keep_marker = 3; }\n",
    )
    .unwrap();

    fs::rename(&staging, root.join("vendor")).unwrap();

    assert!(
        wait_for_match(port, "moved_subtree_keep_marker", Duration::from_secs(60)),
        "watcher never indexed the non-ignored file in a subtree that arrived whole"
    );
    assert_eq!(
        search_matches(port, "moved_subtree_secret_marker"),
        0,
        "watcher indexed a file excluded by a .gitignore that arrived inside \
         the same subtree, so the subtree was indexed against stale rules"
    );
}

/// A directory removed and recreated at the same path must still be watched.
///
/// The kernel drops an inotify watch when its directory goes away and does not
/// say so, leaving the path recorded as watched with no descriptor behind it.
/// Nothing later can tell that entry from a live one — it is in the desired set
/// *and* in the watched set — so without explicitly clearing it on removal the
/// directory stops being watched for the life of the process.
///
/// `rm -rf build && mkdir build`, a branch switch, and a `git clean` all do
/// exactly this.
#[test]
fn watcher_rewatches_a_directory_that_is_removed_and_recreated() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let index_dir = root.join(".tgrep_test_index");

    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join(".gitignore"), "build/\n").unwrap();
    fs::create_dir_all(root.join("src").join("gen")).unwrap();
    fs::write(
        root.join("src").join("lib.rs"),
        "fn seeded() { let normal_source_marker = 1; }\n",
    )
    .unwrap();
    fs::write(
        root.join("src").join("gen").join("old.rs"),
        "fn old() { let pre_delete_marker = 2; }\n",
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
        wait_for_match(port, "pre_delete_marker", Duration::from_secs(30)),
        "expected the seeded file in src/gen to be searchable"
    );
    thread::sleep(Duration::from_secs(2));

    let gen_dir = root.join("src").join("gen");
    fs::remove_dir_all(&gen_dir).unwrap();
    thread::sleep(Duration::from_secs(2));
    fs::create_dir_all(&gen_dir).unwrap();

    // Deliberately after the recreation has been processed. A file written in
    // the same breath would be picked up by the subscription pass's own scan
    // and would say nothing about whether the watch itself was re-established.
    thread::sleep(Duration::from_secs(3));
    fs::write(
        gen_dir.join("new.rs"),
        "fn regenerated() { let post_recreate_marker = 3; }\n",
    )
    .unwrap();

    assert!(
        wait_for_match(port, "post_recreate_marker", Duration::from_secs(30)),
        "watcher never saw a file written to a directory that was removed and \
         recreated, so its subscription was not re-established"
    );
}

/// The walk decides what belongs in the index; the watcher must agree with it.
///
/// `should_skip_watcher_path` only filters by location — excludes, ignore
/// rules, hidden paths. It says nothing about the two rules the walker applies
/// per file: binary extensions are skipped, and so is anything over
/// `--max-filesize`. A file arriving through the watcher therefore used to be
/// indexed even when a walk of the very same tree would have rejected it, so
/// the index disagreed with itself depending on whether a file was present at
/// startup or written afterwards — and the next reconcile silently deleted it.
///
/// Both rejections are asserted alongside an eligible file written at the same
/// moment. Without that control the test would pass just as happily against a
/// watcher that had stopped indexing anything at all.
#[test]
fn watcher_applies_the_same_file_eligibility_rules_as_the_walker() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let index_dir = root.join(".tgrep_test_index");

    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src").join("lib.rs"),
        "fn seeded() { let seeded_source_marker = 1; }\n",
    )
    .unwrap();
    // Small enough to be indexed now, and grown past the cap below.
    fs::write(
        root.join("src").join("grows.rs"),
        "fn grows() { let outgrew_the_cap_marker = 5; }\n",
    )
    .unwrap();

    let status = Command::new(tgrep_bin())
        .args([
            "index",
            root.to_str().unwrap(),
            "--index-path",
            index_dir.to_str().unwrap(),
            "--max-filesize",
            "2K",
        ])
        .status()
        .expect("failed to run tgrep index");
    assert!(status.success(), "initial index build failed");

    let child = Command::new(tgrep_bin())
        .args([
            "serve",
            "--index-path",
            index_dir.to_str().unwrap(),
            "--max-filesize",
            "2K",
            root.to_str().unwrap(),
        ])
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("failed to start tgrep serve");
    let _server = ServerGuard { child };

    let (_pid, port) = wait_for_server(&index_dir);
    assert!(
        wait_for_match(port, "seeded_source_marker", Duration::from_secs(30)),
        "expected the seeded file to be searchable"
    );

    let src = root.join("src");
    // Text content, so nothing but the extension can keep it out.
    fs::write(
        src.join("asset.png"),
        "fn decoy() { let binary_extension_marker = 2; }\n",
    )
    .unwrap();
    // Same marker, pushed past the 2K cap by padding.
    let mut oversized = String::from("fn big() { let oversized_file_marker = 3; }\n");
    oversized.push_str(&"// padding\n".repeat(400));
    fs::write(src.join("huge.rs"), oversized).unwrap();
    // Already in the index, and now over the cap: what was indexed has to go,
    // not just stop being updated.
    let mut grown = String::from("fn grows() { let outgrew_the_cap_marker = 5; }\n");
    grown.push_str(&"// padding\n".repeat(400));
    fs::write(src.join("grows.rs"), grown).unwrap();
    fs::write(
        src.join("extra.rs"),
        "fn extra() { let eligible_file_marker = 4; }\n",
    )
    .unwrap();

    assert!(
        wait_for_match(port, "eligible_file_marker", Duration::from_secs(30)),
        "the eligible file written next to the rejected ones was never indexed, \
         so this test cannot say anything about the rejections"
    );

    assert_eq!(
        search_matches(port, "binary_extension_marker"),
        0,
        "the watcher indexed a file whose extension the walker rejects"
    );
    assert_eq!(
        search_matches(port, "oversized_file_marker"),
        0,
        "the watcher indexed a file larger than --max-filesize"
    );
    assert!(
        wait_for_no_match(port, "outgrew_the_cap_marker", Duration::from_secs(30)),
        "a file that grew past --max-filesize kept its indexed content"
    );
}

#[cfg(unix)]
#[test]
fn watcher_does_not_index_through_symlinks() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let index_dir = root.join(".tgrep_test_index");

    // Deliberately outside the served root: this stands in for anything a
    // symlink committed to a branch could point at.
    let outside = TempDir::new().unwrap();
    let secret = outside.path().join("secret.txt");
    fs::write(&secret, "fn leak() { let outside_root_marker = 1; }\n").unwrap();

    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src").join("lib.rs"),
        "fn seeded() { let seeded_source_marker = 1; }\n",
    )
    .unwrap();
    // Indexed as a real file first, then replaced by a link below.
    fs::write(
        root.join("src").join("swapped.rs"),
        "fn swapped() { let replaced_by_symlink_marker = 2; }\n",
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
        wait_for_match(port, "seeded_source_marker", Duration::from_secs(30)),
        "expected the seeded file to be searchable"
    );
    assert_eq!(
        search_matches(port, "replaced_by_symlink_marker"),
        1,
        "expected the file that is about to be replaced to start out indexed"
    );

    let src = root.join("src");
    std::os::unix::fs::symlink(&secret, src.join("link.rs")).unwrap();
    fs::remove_file(src.join("swapped.rs")).unwrap();
    std::os::unix::fs::symlink(&secret, src.join("swapped.rs")).unwrap();
    fs::write(
        src.join("extra.rs"),
        "fn extra() { let eligible_file_marker = 3; }\n",
    )
    .unwrap();

    assert!(
        wait_for_match(port, "eligible_file_marker", Duration::from_secs(30)),
        "the eligible file written alongside the symlinks was never indexed, \
         so this test cannot say anything about the symlinks"
    );

    assert!(
        wait_for_no_match(port, "replaced_by_symlink_marker", Duration::from_secs(30)),
        "a real file replaced by a symlink kept its old content in the index"
    );
    assert_eq!(
        search_matches(port, "outside_root_marker"),
        0,
        "the watcher followed a symlink and indexed content from outside the served root"
    );
}
