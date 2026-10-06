#![deny(unnameable_test_items)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const PROFILE: &str = r#"{"content":"raw-git-blob-auto-v1","coverage":"tracked-regular-files-v1","max_blob_bytes":67108864}"#;
static LEASE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[path = "shared_daemon/runtime.rs"]
mod runtime;
#[path = "shared_daemon/stateful.rs"]
mod stateful;

fn git(root: &Path, args: &[&str]) -> String {
    let output = runtime::output(
        Command::new("git")
            .current_dir(root)
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE"),
    );
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn cli(root: &Path, args: &[&str]) -> Output {
    runtime::output(
        Command::new(assert_cmd::cargo::cargo_bin("tgrep"))
            .current_dir(root)
            .args(args),
    )
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

struct Fixture {
    temp: TempDir,
    a: PathBuf,
    b: PathBuf,
    storage: PathBuf,
    revision: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        let storage = temp.path().join("storage");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&storage).unwrap();
        git(&a, &["init", "-q"]);
        git(&a, &["config", "user.name", "Shared test"]);
        git(&a, &["config", "user.email", "shared@example.invalid"]);
        git(&a, &["config", "core.autocrlf", "false"]);
        fs::create_dir(a.join("src")).unwrap();
        fs::write(
            a.join("src/main.rs"),
            "fn shared_term() {}\ncontext line\nshared_term again\n",
        )
        .unwrap();
        fs::write(a.join("notes.txt"), "shared_term notes\n").unwrap();
        fs::write(a.join(".hidden"), "shared_term hidden\n").unwrap();
        git(&a, &["add", "."]);
        git(&a, &["commit", "-qm", "base"]);
        let revision = git(&a, &["rev-parse", "HEAD"]);
        git(
            &a,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                b.to_str().unwrap(),
                &revision,
            ],
        );
        Self {
            temp,
            a,
            b,
            storage,
            revision,
        }
    }

    fn start(&self, options: &[&str]) -> Daemon {
        Daemon::start(&self.a, &self.storage, options)
    }

    fn third(&self, revision: &str) -> PathBuf {
        let root = self.temp.path().join("c");
        git(
            &self.a,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                root.to_str().unwrap(),
                revision,
            ],
        );
        root
    }
}

struct Daemon {
    child: Child,
    marker: Value,
}

impl Daemon {
    fn start(root: &Path, storage: &Path, options: &[&str]) -> Self {
        Self::start_with_home(root, storage, options, None)
    }

    fn start_with_home(root: &Path, storage: &Path, options: &[&str], home: Option<&Path>) -> Self {
        let log = storage.join("daemon.log");
        let marker_path = tgrep_core::generations::Repository::discover(root)
            .unwrap()
            .common_dir()
            .join("tgrep-daemon-v1.json");
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("tgrep"));
        if let Some(home) = home {
            command
                .env("HOME", home)
                .env("USERPROFILE", home)
                .env("XDG_CONFIG_HOME", home);
        }
        let child = command
            .args([
                "serve",
                "--shared",
                root.to_str().unwrap(),
                "--shared-storage",
                storage.to_str().unwrap(),
            ])
            .args(options)
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let mut daemon = Self {
            child,
            marker: Value::Null,
        };
        let started = Instant::now();
        loop {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "daemon readiness timed out: {}",
                fs::read_to_string(&log).unwrap()
            );
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "daemon exited: {}",
                fs::read_to_string(&log).unwrap()
            );
            if let Ok(bytes) = fs::read(&marker_path)
                && let Ok(marker) = serde_json::from_slice::<Value>(&bytes)
                && marker["pid"] == daemon.child.id()
            {
                daemon.marker = marker;
                if daemon
                    .try_rpc("hello", json!({}))
                    .is_ok_and(|v| v.get("result").is_some())
                {
                    return daemon;
                }
            }
            thread::sleep(Duration::from_millis(30));
        }
    }

    fn request(&self, method: &str, mut params: Value) -> Value {
        if method == "attach" && params.get("lease").is_none() {
            params["lease"] = json!(format!(
                "test-{}",
                LEASE_SEQUENCE.fetch_add(1, Ordering::SeqCst)
            ));
        }
        json!({
            "jsonrpc":"2.0","protocol":1, "instance":self.marker["instance"],
            "repository":self.marker["repository"], "id":1, "method":method,"params":params
        })
    }

    fn raw(&self, request: &Value) -> std::io::Result<Value> {
        let mut stream =
            TcpStream::connect(("127.0.0.1", self.marker["port"].as_u64().unwrap() as u16))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        writeln!(stream, "{request}")?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        Ok(serde_json::from_str(&line)?)
    }

    fn try_rpc(&self, method: &str, params: Value) -> std::io::Result<Value> {
        self.raw(&self.request(method, params))
    }

    fn rpc(&self, method: &str, params: Value) -> Value {
        let response = self.try_rpc(method, params).unwrap();
        assert!(response.get("error").is_none(), "{method}: {response}");
        assert_eq!(response["result"]["instance"], self.marker["instance"]);
        assert_eq!(response["result"]["protocol"], 1);
        response["result"].clone()
    }

    fn attach(&self, root: &Path, revision: &str) -> Value {
        let result = self.rpc("attach", json!({"root":root,"revision":revision,"profile":serde_json::from_str::<Value>(PROFILE).unwrap()}));
        self.ready(root);
        result
    }

    fn attach_without_response(&self, params: Value) {
        let mut stream =
            TcpStream::connect(("127.0.0.1", self.marker["port"].as_u64().unwrap() as u16))
                .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        writeln!(stream, "{}", self.request("attach", params.clone())).unwrap();
        drop(stream);
        let started = Instant::now();
        loop {
            let response = self
                .try_rpc("lookup", json!({"root": params["root"]}))
                .unwrap();
            if response.get("result").is_some() {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "lost-response attach did not complete: {response}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn lookup(&self, root: &Path) -> Value {
        self.rpc("lookup", json!({"root":root}))
    }

    fn ready(&self, root: &Path) -> Value {
        let started = Instant::now();
        loop {
            let status = self.lookup(root);
            if status["ready"] == true {
                return status;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "view never ready: {status}"
            );
            thread::sleep(Duration::from_millis(30));
        }
    }

    fn refresh(&self, root: &Path, lease: &Value, changed: &[&str], full: bool) -> Value {
        self.rpc("refresh", json!({
            "root":root,"view":lease["view"],"lease":lease["lease"],"changed":changed,"full":full
        }))
    }

    fn search(&self, root: &Path, pattern: &str) -> Value {
        let view = self.lookup(root);
        self.rpc(
            "search",
            json!({"root":root, "view":view["view"], "query":{"pattern":pattern}}),
        )
    }

    fn stop(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn exact_base_sharing_leases_and_revision_pins() {
    let f = Fixture::new();
    let d = f.start(&["--no-watch"]);
    let a = d.attach(&f.a, &f.revision);
    let b = d.attach(&f.b, &f.revision);
    assert_eq!(a["generation"], b["generation"]);
    assert_eq!(a["attach_build"]["blobs_extracted"], 3);
    assert_eq!(b["attach_build"]["blobs_extracted"], 0);
    assert_eq!(b["attach_build"]["reused_generation"], true);
    for (root, gitfiles) in [(&f.a, 0), (&f.b, 1)] {
        let status = d.lookup(root);
        assert_eq!(status["base_sharing_views"], 2);
        assert_eq!(status["last_reconcile"]["files_read"], 3 + gitfiles);
        assert_eq!(status["last_reconcile"]["files_decoded"], 3 + gitfiles);
        assert_eq!(status["last_reconcile"]["files_extracted"], gitfiles);
        assert_eq!(status["last_reconcile"]["base_reused"], 3);
    }
    let hidden = d.rpc(
        "files",
        json!({"root":f.b,"view":b["view"],"query":{"hidden":true}}),
    );
    assert!(
        hidden["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path == ".git")
    );
    assert_eq!(
        success(cli(&f.b, &["--files", "--hidden", "--sort", "path", "."])),
        success(cli(
            &f.b,
            &["--files", "--hidden", "--no-index", "--sort", "path", "."]
        ))
    );
    let gitfile = d.rpc(
        "search",
        json!({"root":f.b,"view":b["view"],"query":{"pattern":"gitdir:","hidden":true}}),
    );
    assert!(
        gitfile["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["file"] == ".git")
    );
    let noop = d.refresh(&f.b, &b, &[], true);
    assert_eq!(noop["last_reconcile"]["files_extracted"], 0);
    assert_eq!(noop["last_reconcile"]["overlay_reused"], 1);
    let duplicate = d.attach(&f.a, &f.revision);
    assert_eq!(a["view"], duplicate["view"]);
    assert_ne!(a["lease"], duplicate["lease"]);
    let detached = d.rpc(
        "detach",
        json!({"root":f.a,"view":a["view"],"lease":a["lease"]}),
    );
    assert_eq!(detached["remaining_leases"], 1);
    assert!(f.a.join(".git/tgrep-view-v1.json").exists());
    let output = cli(&f.a, &["--stats", "--", "shared_term", "."]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("via shared daemon v1"));
    success(output);
    fs::write(f.a.join("new.txt"), "new_generation_term\n").unwrap();
    git(&f.a, &["add", "."]);
    git(&f.a, &["commit", "-qm", "new generation"]);
    let revision = git(&f.a, &["rev-parse", "HEAD"]);
    let c = f.third(&revision);
    let newer = d.attach(&c, &revision);
    assert_ne!(newer["generation"], a["generation"]);
    assert_eq!(newer["attach_build"]["blobs_extracted"], 1);
    assert_eq!(newer["attach_build"]["reused_indexed_files"], 3);
    assert_eq!(d.lookup(&f.a)["generation"], a["generation"]);
    d.refresh(&f.a, &duplicate, &["new.txt"], false);
    assert!(
        !d.search(&f.a, "new_generation_term")["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        d.search(&f.b, "new_generation_term")["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mismatch = d.try_rpc("attach", json!({"root":f.a,"revision":revision,"profile":serde_json::from_str::<Value>(PROFILE).unwrap()})).unwrap();
    assert!(
        mismatch["error"]["message"]
            .as_str()
            .unwrap()
            .contains("different base")
    );
    d.rpc(
        "detach",
        json!({"root":f.a,"view":duplicate["view"],"lease":duplicate["lease"]}),
    );
    assert!(!f.a.join(".git/tgrep-view-v1.json").exists());
    assert_eq!(d.lookup(&f.b)["ready"], true);
}

#[test]
fn last_detach_releases_root_handle_without_stopping_sibling_queries() {
    let f = Fixture::new();
    let d = f.start(&["--no-watch"]);
    d.attach(&f.a, &f.revision);
    let b = d.attach(&f.b, &f.revision);
    let duplicate = d.attach(&f.b, &f.revision);
    let first = d.rpc(
        "detach",
        json!({"root":f.b,"view":b["view"],"lease":b["lease"]}),
    );
    assert_eq!(first["remaining_leases"], 1);
    assert_eq!(d.search(&f.b, "shared_term")["backend"], "shared-v1");
    let last = d.rpc(
        "detach",
        json!({"root":f.b,"view":duplicate["view"],"lease":duplicate["lease"]}),
    );
    assert_eq!(last["remaining_leases"], 0);
    assert!(last["registration_warning"].is_null());
    git(
        &f.a,
        &["worktree", "remove", "--force", f.b.to_str().unwrap()],
    );
    assert!(!f.b.exists());
    assert_eq!(d.search(&f.a, "shared_term")["backend"], "shared-v1");
    assert_eq!(d.lookup(&f.a)["ready"], true);
}

#[test]
fn isolated_changes_candidate_masking_membership_and_scan_parity() {
    let f = Fixture::new();
    let d = f.start(&["--no-watch"]);
    let a = d.attach(&f.a, &f.revision);
    d.attach(&f.b, &f.revision);
    fs::write(
        f.a.join("src/main.rs"),
        "fn private_term() {}\ncontext line\nprivate_term again\n",
    )
    .unwrap();
    fs::rename(f.a.join("notes.txt"), f.a.join("renamed.txt")).unwrap();
    git(&f.a, &["add", "-A"]);
    git(&f.a, &["commit", "-qm", "private commit"]);
    fs::write(f.a.join("staged.txt"), "private_term staged\n").unwrap();
    git(&f.a, &["add", "staged.txt"]);
    fs::write(f.a.join("untracked.py"), "private_term untracked\n").unwrap();
    fs::write(f.a.join("ignored.txt"), "private_term ignored\n").unwrap();
    fs::write(f.a.join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(f.a.join("binary.txt"), b"private_term\0hidden binary").unwrap();
    fs::write(f.a.join("asset.bin"), "private_term extension\n").unwrap();
    fs::remove_file(f.a.join(".hidden")).unwrap();
    let refreshed = d.refresh(&f.a, &a, &[], true);
    assert!(refreshed["processed_epoch"].as_u64().unwrap() >= a["epoch"].as_u64().unwrap());
    assert!(
        d.search(&f.a, "shared_term")["matches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["file"] != "src/main.rs")
    );
    assert!(
        d.search(&f.b, "private_term")["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for args in [
        vec!["-n"],
        vec!["-l"],
        vec!["-c"],
        vec!["--count-matches"],
        vec!["-C", "1"],
        vec!["--vimgrep"],
        vec!["--column", "--byte-offset"],
        vec!["-o"],
        vec!["-t", "rust"],
        vec!["-g", "*.txt", "-g", "!ignored.txt"],
        vec!["--hidden"],
        vec!["--max-depth", "1"],
        vec!["-i"],
        vec!["-w"],
        vec!["-e", "private_.*", "-e", "unmatched"],
    ] {
        let before = d.lookup(&f.a)["queries"].as_u64().unwrap();
        let mut shared = vec!["--sort", "path", "--color", "never"];
        shared.extend(args.clone());
        if !args.contains(&"-e") {
            shared.extend(["--", "private_term", "."]);
        } else {
            shared.push(".");
        }
        let indexed = cli(&f.a, &shared);
        let mut scan = vec!["--no-index"];
        scan.extend(shared);
        let scanned = cli(&f.a, &scan);
        assert_eq!(indexed.status.code(), scanned.status.code(), "{args:?}");
        assert_eq!(
            indexed.stdout,
            scanned.stdout,
            "{args:?}\n{}",
            String::from_utf8_lossy(&indexed.stderr)
        );
        assert_eq!(
            d.lookup(&f.a)["queries"].as_u64().unwrap(),
            before + 1,
            "must use daemon for {args:?}"
        );
    }
    let listed = success(cli(&f.a, &["--files", "--sort", "path", "--hidden", "."]));
    assert!(
        listed.contains("binary.txt")
            && listed.contains("asset.bin")
            && !listed.contains("ignored.txt")
    );
    assert_eq!(
        listed,
        success(cli(
            &f.a,
            &[
                "--no-index",
                "--files",
                "--sort",
                "path",
                "--hidden",
                "-g",
                "!.git",
                "."
            ]
        ))
    );
    let scope = success(cli(&f.a, &["-n", "--stats", "--", "private_term", "src"]));
    assert!(scope.starts_with(&format!("src{}main.rs:1:", std::path::MAIN_SEPARATOR)));
    let multi = success(cli(
        &f.a,
        &[
            "-l",
            "--sort",
            "path",
            "--",
            "private_term",
            "src",
            f.b.to_str().unwrap(),
        ],
    ));
    assert!(multi.contains("main.rs") && !multi.contains(f.b.to_str().unwrap()));
    let json_output = success(cli(
        &f.a,
        &["--json", "-C", "1", "--", "private_term", "src"],
    ));
    let rows: Vec<Value> = json_output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        rows.iter()
            .any(|row| row["type"] == "match" && row["data"]["line_number"] == 1)
    );
    assert!(
        rows.iter()
            .any(|row| row["type"] == "context" && row["data"]["line_number"] == 2)
    );
    assert_eq!(
        rows.iter().filter(|row| row["type"] == "summary").count(),
        1
    );
}

#[test]
fn cli_lifecycle_unsupported_options_and_explicit_index_intent() {
    let f = Fixture::new();
    let d = f.start(&["--no-watch"]);
    let attached: Value = serde_json::from_str(&success(cli(
        &f.a,
        &["shared", "attach", ".", "--revision", &f.revision],
    )))
    .unwrap();
    d.ready(&f.a);
    fs::write(f.a.join("late.txt"), "late_term\n").unwrap();
    success(cli(
        &f.a,
        &[
            "shared",
            "refresh",
            ".",
            "--lease",
            attached["lease"].as_str().unwrap(),
            "--changed",
            "late.txt",
        ],
    ));
    assert!(success(cli(&f.a, &["status", "."])).contains("\"ready\": true"));
    for args in [
        vec!["--no-ignore"],
        vec!["--text"],
        vec!["--binary"],
        vec!["--follow"],
        vec!["--one-file-system"],
        vec!["--no-require-git"],
        vec!["--encoding", "latin1"],
        vec!["--max-filesize", "1M"],
        vec!["--files-without-match"],
        vec!["--include-zero", "-c"],
    ] {
        let before = d.lookup(&f.a)["queries"].as_u64().unwrap();
        let mut query = args.clone();
        query.extend(["--sort", "path", "--", "late_term", "."]);
        let indexed = cli(&f.a, &query);
        let mut scan = vec!["--no-index"];
        scan.extend(query);
        let scanned = cli(&f.a, &scan);
        assert_eq!(indexed.status.code(), scanned.status.code(), "{args:?}");
        assert_eq!(indexed.stdout, scanned.stdout, "{args:?}");
        assert!(
            String::from_utf8_lossy(&indexed.stderr).contains("scanning filesystem"),
            "{args:?}"
        );
        assert_eq!(d.lookup(&f.a)["queries"].as_u64().unwrap(), before);
    }
    let explicit = cli(&f.a, &["--", "late_term", "late.txt"]);
    assert!(explicit.status.success());
    assert!(String::from_utf8_lossy(&explicit.stderr).contains("scanning filesystem"));
    let index = f.temp.path().join("ordinary-index");
    success(cli(
        &f.a,
        &["index", ".", "--index-path", index.to_str().unwrap()],
    ));
    let explicit = cli(
        &f.a,
        &[
            "--stats",
            "--index-path",
            index.to_str().unwrap(),
            "--",
            "late_term",
            ".",
        ],
    );
    assert!(!String::from_utf8_lossy(&explicit.stderr).contains("via shared"));
    success(explicit);
    success(cli(
        &f.a,
        &[
            "shared",
            "detach",
            ".",
            "--lease",
            attached["lease"].as_str().unwrap(),
        ],
    ));
    assert!(!f.a.join(".git/tgrep-view-v1.json").exists());
}

#[test]
fn no_watch_full_repair_and_restart_checkpoint_revalidation() {
    let f = Fixture::new();
    let mut d = f.start(&["--no-watch"]);
    let a = d.attach(&f.a, &f.revision);
    fs::write(f.a.join("private.txt"), "checkpoint_term\n").unwrap();
    d.refresh(&f.a, &a, &["private.txt"], false);
    let path = f.a.join("notes.txt");
    let metadata = fs::metadata(&path).unwrap();
    fs::write(&path, "ZXQJVPKBMWH notes\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(d.lookup(&f.a)["watch_mode"], "disabled");
    assert!(
        d.search(&f.a, "ZXQJVPKBMWH")["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let repaired = d.refresh(&f.a, &a, &[], true);
    assert_eq!(repaired["last_reconcile"]["full"], true);
    assert!(
        !d.search(&f.a, "ZXQJVPKBMWH")["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let old_instance = d.marker["instance"].clone();
    d.stop();
    fs::write(f.a.join("after-crash.txt"), "fresh_restart_term\n").unwrap();
    let fallback = cli(&f.a, &["--", "fresh_restart_term", "."]);
    assert!(String::from_utf8_lossy(&fallback.stderr).contains("scanning filesystem"));
    success(fallback);
    let d = f.start(&["--no-watch"]);
    assert_ne!(d.marker["instance"], old_instance);
    let stale = cli(&f.a, &["--", "fresh_restart_term", "."]);
    assert!(String::from_utf8_lossy(&stale.stderr).contains("stale"));
    success(stale);
    let new = d.attach(&f.a, &f.revision);
    assert_eq!(new["checkpoint_restored"], true);
    assert_eq!(new["generation"], a["generation"]);
    assert_eq!(new["attach_build"]["blobs_extracted"], 0);
    assert!(
        !d.search(&f.a, "fresh_restart_term")["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !d.search(&f.a, "checkpoint_term")["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let old_detach = d
        .try_rpc(
            "detach",
            json!({"root":f.a,"view":a["view"],"lease":a["lease"]}),
        )
        .unwrap();
    assert!(old_detach.get("error").is_some());
    assert!(f.a.join(".git/tgrep-view-v1.json").exists());
}

#[test]
fn passthru_rpc_is_rejected_while_cli_preserves_scan_output() {
    let f = Fixture::new();
    fs::write(f.a.join("nonmatching.txt"), "nonmatching_file_payload\n").unwrap();
    let d = f.start(&["--no-watch"]);
    let a = d.attach(&f.a, &f.revision);
    let normal = d.rpc(
        "search",
        json!({"root":f.a,"view":a["view"],"query":{"pattern":"shared_term","passthru":false}}),
    );
    assert_eq!(normal["backend"], "shared-v1");
    let before = d.lookup(&f.a)["queries"].as_u64().unwrap();
    for pattern in ["shared_term", "absentZXQJVPKMW"] {
        let response = d
            .try_rpc(
                "search",
                json!({"root":f.a,"view":a["view"],"query":{"pattern":pattern,"passthru":true}}),
            )
            .unwrap();
        assert!(response.get("result").is_none(), "{response}");
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("passthru requires a filesystem scan"),
            "{response}"
        );
        let args = ["--passthru", "--sort", "path", "--", pattern, "."];
        let attached = cli(&f.a, &args);
        let mut scan = vec!["--no-index"];
        scan.extend(args);
        let scanned = cli(&f.a, &scan);
        assert_eq!(attached.status.code(), scanned.status.code());
        assert_eq!(attached.stdout, scanned.stdout);
        assert!(String::from_utf8_lossy(&attached.stdout).contains("nonmatching_file_payload"));
        assert!(
            String::from_utf8_lossy(&attached.stderr).contains("scanning filesystem"),
            "{attached:?}"
        );
    }
    assert_eq!(d.lookup(&f.a)["queries"].as_u64().unwrap(), before);
}

#[test]
fn protocol_root_scope_and_aggregate_limit_rejections() {
    let f = Fixture::new();
    let d = f.start(&[
        "--no-watch",
        "--shared-max-views",
        "1",
        "--shared-max-leases",
        "2",
        "--watcher-queue-cap",
        "2",
    ]);
    let a = d.attach(&f.a, &f.revision);
    let mut wrong = d.request("hello", json!({}));
    wrong["protocol"] = json!(2);
    assert!(d.raw(&wrong).unwrap().get("error").is_some());
    wrong["protocol"] = json!(1);
    wrong["instance"] = json!("port-reused-by-another-daemon");
    assert!(d.raw(&wrong).unwrap().get("error").is_some());
    let error = d.try_rpc("attach", json!({"root":f.b,"revision":f.revision,"profile":serde_json::from_str::<Value>(PROFILE).unwrap()})).unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("view limit")
    );
    d.attach(&f.a, &f.revision);
    let error = d.try_rpc("attach", json!({"root":f.a,"revision":f.revision,"profile":serde_json::from_str::<Value>(PROFILE).unwrap()})).unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("lease limit")
    );
    let error = d
        .try_rpc(
            "refresh",
            json!({"root":f.a,"view":a["view"],"lease":a["lease"],"changed":["one","two","three"]}),
        )
        .unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("full repair")
    );
    d.ready(&f.a);
    for scope in [
        "../",
        "/tmp",
        "C:/",
        "src/../../",
        "src\\..",
        "notes.txt",
        "notes.txt/",
        "missing/",
    ] {
        let error = d
            .try_rpc(
                "files",
                json!({"root":f.a,"view":a["view"],"query":{"scope":scope}}),
            )
            .unwrap();
        assert!(error.get("error").is_some(), "{scope}: {error}");
    }
    let error = d
        .try_rpc(
            "search",
            json!({"root":f.a,"view":a["view"],"query":{"pattern":"x","hidden":"yes"}}),
        )
        .unwrap();
    assert!(error.get("error").is_some());
    let error = d
        .try_rpc(
            "search",
            json!({"root":f.b,"view":a["view"],"query":{"pattern":"x"}}),
        )
        .unwrap();
    assert!(error.get("error").is_some());
    let duplicate = cli(
        &f.a,
        &[
            "serve",
            "--shared",
            ".",
            "--shared-storage",
            f.storage.to_str().unwrap(),
            "--no-watch",
        ],
    );
    assert_eq!(duplicate.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already owns"));
    let nested = f.a.join("nested");
    fs::create_dir(&nested).unwrap();
    git(&nested, &["init", "-q"]);
    fs::write(nested.join("file.txt"), "nested_term\n").unwrap();
    let result = cli(&nested, &["--stats", "--", "nested_term", "."]);
    assert!(!String::from_utf8_lossy(&result.stderr).contains("via shared"));
    success(result);
    let error = d
        .try_rpc(
            "files",
            json!({"root":f.a,"view":a["view"],"query":{"scope":"nested/"}}),
        )
        .unwrap();
    assert!(error.get("error").is_some());
}

#[test]
fn polling_and_native_events_reconcile_without_queries() {
    for options in [
        vec!["--watch-mode", "poll", "--poll-interval", "1"],
        vec![
            "--watch-mode",
            "auto",
            "--poll-interval",
            "60",
            "--shared-max-views",
            "2",
        ],
        vec![
            "--watch-mode",
            "auto",
            "--poll-interval",
            "1",
            "--watch-budget",
            "1",
        ],
    ] {
        let f = Fixture::new();
        let d = f.start(&options);
        let a = d.attach(&f.a, &f.revision);
        let initial = d.lookup(&f.a);
        if options.contains(&"60") {
            assert_eq!(initial["watch_mode"], "native", "{initial}");
            d.attach(&f.b, &f.revision);
            let before = d.ready(&f.b);
            let gitfile = f.b.join(".git");
            fs::write(&gitfile, fs::read(&gitfile).unwrap()).unwrap();
            let started = Instant::now();
            loop {
                let status = d.lookup(&f.b);
                if status["ready"] == true
                    && status["published_epoch"].as_u64() > before["published_epoch"].as_u64()
                {
                    break;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(15),
                    "gitfile event was ignored: {status}"
                );
                thread::sleep(Duration::from_millis(40));
            }
        } else {
            assert_eq!(initial["watch_mode"], "poll");
        }
        fs::create_dir(f.a.join("new-dir")).unwrap();
        fs::write(f.a.join("new-dir/new.txt"), "automatic_term\n").unwrap();
        let started = Instant::now();
        loop {
            let status = d.lookup(&f.a);
            if status["ready"] == true
                && status["published_epoch"].as_u64() > initial["published_epoch"].as_u64()
            {
                // Another event can close readiness between lookup and search.
                let response = d
                    .try_rpc(
                        "search",
                        json!({"root":f.a,"view":a["view"],"query":{"pattern":"automatic_term"}}),
                    )
                    .unwrap();
                if response.get("error").is_some() {
                    assert_eq!(response["error"]["code"], -32001, "{response}");
                    assert_eq!(
                        response["error"]["message"],
                        "shared reconciliation in progress; retry or scan",
                        "{response}"
                    );
                } else if !response["result"]["matches"].as_array().unwrap().is_empty() {
                    break;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "automatic repair failed: {status}"
            );
            thread::sleep(Duration::from_millis(40));
        }
        fs::write(f.a.join(".ignore"), "new-dir/\n").unwrap();
        let started = Instant::now();
        loop {
            let result = d
                .try_rpc("files", json!({"root":f.a,"view":a["view"],"query":{}}))
                .unwrap();
            if let Some(paths) = result["result"]["files"].as_array()
                && !paths.iter().any(|p| p == "new-dir/new.txt")
            {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "ignore repair failed: {result}"
            );
            thread::sleep(Duration::from_millis(40));
        }
    }
}

#[test]
fn crlf_transform_and_sparse_membership_are_honest() {
    let f = Fixture::new();
    git(&f.a, &["config", "core.autocrlf", "true"]);
    fs::write(f.a.join(".gitattributes"), "* -text\none.txt text eol=crlf\ntwo.txt text eol=crlf\nthree.txt text eol=crlf\nfour.txt text eol=crlf\n").unwrap();
    for name in ["one.txt", "two.txt", "three.txt", "four.txt"] {
        fs::write(f.a.join(name), format!("transformed_{name}\n")).unwrap();
    }
    git(&f.a, &["add", "."]);
    git(&f.a, &["commit", "-qm", "CRLF profile"]);
    let rev = git(&f.a, &["rev-parse", "HEAD"]);
    let c = f.third(&rev);
    for name in ["one.txt", "two.txt", "three.txt", "four.txt"] {
        assert!(
            fs::read(c.join(name))
                .unwrap()
                .windows(2)
                .any(|bytes| bytes == b"\r\n")
        );
    }
    let d = f.start(&["--no-watch"]);
    let lease = d.attach(&c, &rev);
    let status = d.lookup(&c);
    // Four CRLF transformations plus the ordinary linked-worktree .git file.
    assert_eq!(status["last_reconcile"]["files_extracted"], 5, "{status}");
    let noop = d.refresh(&c, &lease, &[], true);
    assert_eq!(noop["last_reconcile"]["files_extracted"], 0);
    assert_eq!(noop["last_reconcile"]["overlay_reused"], 5);
    git(&c, &["sparse-checkout", "set", "--no-cone", "src/"]);
    d.refresh(&c, &lease, &[], true);
    assert!(!c.join("one.txt").exists());
    assert!(
        d.search(&c, "transformed_")["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let listed = success(cli(&c, &["--files", "."]));
    assert!(!listed.contains("one.txt"));
    assert!(listed.contains("main.rs"));
}

#[test]
fn stale_markers_missing_generation_and_foreign_marker_detach_scan_safely() {
    let f = Fixture::new();
    success(cli(&f.a, &["index", "."]));
    let mut d = f.start(&["--no-watch"]);
    let a = d.attach(&f.a, &f.revision);
    fs::write(f.a.join("not-in-any-base.txt"), "unique_fallback_token\n").unwrap();
    let view_marker = f.a.join(".git/tgrep-view-v1.json");
    let original = fs::read(&view_marker).unwrap();
    for payload in [b"{invalid".to_vec(), {
        let mut marker: Value = serde_json::from_slice(&original).unwrap();
        marker["daemon"]["protocol"] = json!(999);
        serde_json::to_vec(&marker).unwrap()
    }] {
        fs::write(&view_marker, payload).unwrap();
        let fallback = cli(&f.a, &["--stats", "--", "unique_fallback_token", "."]);
        assert!(String::from_utf8_lossy(&fallback.stderr).contains("scanning filesystem"));
        assert!(success(fallback).contains("unique_fallback_token"));
        assert!(success(cli(&f.a, &["--files", "."])).contains("not-in-any-base.txt"));
        assert_eq!(cli(&f.a, &["status", "."]).status.code(), Some(2));
    }
    let mut foreign: Value = serde_json::from_slice(&original).unwrap();
    foreign["view"] = json!("belongs-to-a-different-view");
    fs::write(&view_marker, serde_json::to_vec(&foreign).unwrap()).unwrap();
    d.rpc(
        "detach",
        json!({"root":f.a,"view":a["view"],"lease":a["lease"]}),
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&view_marker).unwrap()).unwrap(),
        foreign
    );
    let a = d.attach(&f.a, &f.revision);
    d.stop();
    let key: tgrep_core::generations::GenerationKey =
        serde_json::from_value(a["generation"].clone()).unwrap();
    let generation = f
        .storage
        .join("bases")
        .join(key.repository_identity())
        .join(key.storage_name());
    fs::remove_file(generation.join("lookup.bin")).unwrap();
    let d = f.start(&["--no-watch"]);
    let response = d.try_rpc("attach", json!({"root":f.a,"revision":f.revision,"profile":serde_json::from_str::<Value>(PROFILE).unwrap()})).unwrap();
    assert!(response.get("error").is_some(), "{response}");
    let fallback = cli(&f.a, &["--", "unique_fallback_token", "."]);
    assert!(String::from_utf8_lossy(&fallback.stderr).contains("scanning filesystem"));
    success(fallback);
}

#[test]
fn cli_preserves_null_id_and_matching_id_queue_errors() {
    let f = Fixture::new();
    let d = f.start(&["--no-watch"]);
    for id in [json!(null), json!(1)] {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut marker = d.marker.clone();
        marker["port"] = json!(listener.local_addr().unwrap().port());
        fs::write(
            f.a.join(".git/tgrep-daemon-v1.json"),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        let fake = thread::spawn(move || {
            let started = Instant::now();
            loop {
                match listener.accept() {
                    Ok((mut connection, _)) => {
                        connection.set_nonblocking(false).unwrap();
                        connection
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        connection
                            .set_write_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        let mut line = String::new();
                        BufReader::new(connection.try_clone().unwrap())
                            .read_line(&mut line)
                            .unwrap();
                        let request: Value = serde_json::from_str(&line).unwrap();
                        assert_eq!(request["method"], "hello");
                        writeln!(
                            connection,
                            "{}",
                            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":"shared connection queue full"}})
                        )
                        .unwrap();
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(started.elapsed() < Duration::from_secs(15));
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        });
        let output = cli(
            &f.a,
            &[
                "shared",
                "attach",
                ".",
                "--revision",
                &f.revision,
                "--lease",
                "queue-test",
            ],
        );
        fake.join().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("shared connection queue full"),
            "{output:?}"
        );
        assert!(output.stdout.is_empty(), "{output:?}");
    }
}

#[test]
fn wrong_protocol_port_and_incompatible_roots_profiles_fail_closed() {
    let f = Fixture::new();
    let d = f.start(&["--no-watch"]);
    d.attach(&f.a, &f.revision);
    let other = f.temp.path().join("other");
    fs::create_dir(&other).unwrap();
    git(&other, &["init", "-q"]);
    let profile: Value = serde_json::from_str(PROFILE).unwrap();
    for root in [&other, &f.a.join("src")] {
        let response = d
            .try_rpc(
                "attach",
                json!({"root":root,"revision":f.revision,"profile":profile}),
            )
            .unwrap();
        assert!(response.get("error").is_some(), "{response}");
    }
    let mut incompatible = profile.clone();
    incompatible["max_blob_bytes"] = Value::Null;
    let response = d
        .try_rpc(
            "attach",
            json!({"root":f.b,"revision":f.revision,"profile":incompatible}),
        )
        .unwrap();
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("profile")
    );
    let output = cli(&f.b, &["--stats", "--", "shared_term", "."]);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("via shared"));
    success(output);
    let output = cli(&f.b, &["--shared", "--", "shared_term", "."]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("no shared attachment"));
    success(output);
    fs::write(f.a.join("after-base.txt"), "wrong_port_fallback\n").unwrap();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut marker = d.marker.clone();
    marker["port"] = json!(listener.local_addr().unwrap().port());
    fs::write(
        f.a.join(".git/tgrep-daemon-v1.json"),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
    let fake = thread::spawn(move || {
        let started = Instant::now();
        loop {
            match listener.accept() {
                Ok((mut connection, _)) => {
                    connection.set_nonblocking(false).unwrap();
                    connection
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    connection
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut line = String::new();
                    BufReader::new(connection.try_clone().unwrap())
                        .read_line(&mut line)
                        .unwrap();
                    writeln!(
                        connection,
                        "{}",
                        json!({"jsonrpc":"2.0","id":1,"result":{"hidden_complete":true}})
                    )
                    .unwrap();
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        started.elapsed() < Duration::from_secs(15),
                        "fake daemon never queried"
                    );
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    let output = cli(&f.a, &["--", "wrong_port_fallback", "."]);
    fake.join().unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("wrong-protocol"));
    success(output);
}

#[test]
fn blocked_generation_worker_does_not_block_ready_view_queries() {
    let f = Fixture::new();
    let d = f.start(&["--no-watch"]);
    d.attach(&f.a, &f.revision);
    d.attach(&f.b, &f.revision);
    let c = f.third(&f.revision);
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .open(
            f.storage
                .join("bases")
                .join(d.marker["repository"].as_str().unwrap())
                .join("publication.lock"),
        )
        .unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    let request = d.request("attach", json!({"root":c,"revision":f.revision,"profile":serde_json::from_str::<Value>(PROFILE).unwrap()}));
    let port = d.marker["port"].as_u64().unwrap() as u16;
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        writeln!(stream, "{request}").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        tx.send(serde_json::from_str::<Value>(&line).unwrap())
            .unwrap();
    });
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(500)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    let result = d.search(&f.b, "shared_term");
    assert!(!result["matches"].as_array().unwrap().is_empty());
    assert_eq!(result["backend"], "shared-v1");
    assert!(
        rx.try_recv().is_err(),
        "generation lock must still block attach"
    );
    drop(lock);
    assert!(
        rx.recv_timeout(Duration::from_secs(15))
            .unwrap()
            .get("error")
            .is_none()
    );
    worker.join().unwrap();
}

#[test]
fn candidate_read_failures_close_readiness_and_never_return_partial_success() {
    let f = Fixture::new();
    let d = f.start(&["--no-watch"]);
    let a = d.attach(&f.a, &f.revision);
    fs::remove_file(f.a.join("notes.txt")).unwrap();
    fs::create_dir(f.a.join("notes.txt")).unwrap();
    let response = d
        .try_rpc(
            "search",
            json!({
                "root":f.a,"view":a["view"],"query":{"pattern":"shared_term"}
            }),
        )
        .unwrap();
    assert!(response.get("result").is_none(), "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("reading shared candidate notes.txt")
    );
    let recovered = d.ready(&f.a);
    assert_eq!(recovered["last_reconcile"]["full"], true);
    let matches = d.search(&f.a, "shared_term");
    assert!(
        matches["matches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["file"] != "notes.txt")
    );
}

#[cfg(unix)]
#[test]
fn replaced_root_cannot_publish_matches_files_or_empty_results() {
    let f = Fixture::new();
    fs::create_dir(f.b.join("empty")).unwrap();
    let d = f.start(&["--no-watch"]);
    d.attach(&f.a, &f.revision);
    let b = d.attach(&f.b, &f.revision);
    let moved = f.temp.path().join("original-b");
    let gitfile = fs::read(f.b.join(".git")).unwrap();
    for (method, query) in [
        ("search", json!({"pattern":"shared_term"})),
        ("search", json!({"pattern":"absentZXQJVPKMW"})),
        ("files", json!({"hidden":true})),
        ("files", json!({"scope":"empty/"})),
    ] {
        fs::rename(&f.b, &moved).unwrap();
        fs::create_dir_all(f.b.join("src")).unwrap();
        fs::create_dir(f.b.join("empty")).unwrap();
        fs::write(f.b.join(".git"), &gitfile).unwrap();
        for relative in ["src/main.rs", "notes.txt", ".hidden"] {
            fs::write(
                f.b.join(relative),
                "shared_term replacement_must_not_be_returned\n",
            )
            .unwrap();
        }
        let response = d
            .try_rpc(method, json!({"root":f.b,"view":b["view"],"query":query}))
            .unwrap();
        assert!(response.get("result").is_none(), "{method}: {response}");
        assert!(response["error"]["message"].is_string(), "{response}");
        let status = d.lookup(&f.b);
        assert_eq!(status["ready"], false, "{status}");
        assert!(status["last_error"].is_string(), "{status}");
        assert_eq!(d.search(&f.a, "shared_term")["backend"], "shared-v1");
        fs::remove_dir_all(&f.b).unwrap();
        fs::rename(&moved, &f.b).unwrap();
        assert_eq!(d.refresh(&f.b, &b, &[], true)["ready"], true);
    }
}

#[test]
fn configured_storage_is_external_and_excluded_from_all_shared_views() {
    let f = Fixture::new();
    let invalid = f.a.join("inside");
    fs::create_dir(&invalid).unwrap();
    let output = cli(
        &f.a,
        &[
            "serve",
            "--shared",
            ".",
            "--shared-storage",
            invalid.to_str().unwrap(),
            "--no-watch",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("inside a worktree"));
    let d = f.start(&["--no-watch"]);
    let a = d.attach(&f.a, &f.revision);
    let result = d.rpc(
        "files",
        json!({"root":f.a,"view":a["view"],"query":{"hidden":true}}),
    );
    assert!(result["files"].as_array().unwrap().iter().all(|p| {
        let path = p.as_str().unwrap();
        !path.contains("tgrep-") && !path.contains("overlay.json") && !path.contains("lookup.bin")
    }));
    // Corrupting a checkpoint is recoverable by the runtime, not a reason to
    // silently construct a base-only replacement view on restart.
    let root = fs::canonicalize(&f.a).unwrap();
    let root_id = blake3::hash(root.to_str().unwrap().as_bytes())
        .to_hex()
        .to_string();
    let key: tgrep_core::generations::GenerationKey =
        serde_json::from_value(a["generation"].clone()).unwrap();
    let checkpoint = f
        .storage
        .join("overlays")
        .join(key.repository_identity())
        .join(root_id)
        .join(key.storage_name())
        .join("overlay.json");
    drop(d);
    fs::write(checkpoint, "{}").unwrap();
    let d = f.start(&["--no-watch"]);
    let result = d.try_rpc("attach", json!({"root":f.a,"revision":f.revision,"profile":serde_json::from_str::<Value>(PROFILE).unwrap()})).unwrap();
    assert!(result.get("error").is_some(), "{result}");
    fs::write(f.a.join("late.txt"), "late_safe_scan\n").unwrap();
    success(cli(&f.a, &["--", "late_safe_scan", "."]));
}

#[test]
fn lost_attach_response_is_recoverable_without_allocating_another_lease() {
    let f = Fixture::new();
    let d = f.start(&["--no-watch", "--shared-max-leases", "2"]);
    let params = json!({"root":f.a,"revision":"HEAD","profile":serde_json::from_str::<Value>(PROFILE).unwrap(),"lease":"retry-client"});
    d.attach_without_response(params.clone());
    let original = d.ready(&f.a);
    assert_eq!(original["leases"], 1);
    fs::write(f.a.join("new.txt"), "new commit after interrupted attach\n").unwrap();
    git(&f.a, &["add", "."]);
    git(&f.a, &["commit", "-qm", "move HEAD after response loss"]);
    let moved = git(&f.a, &["rev-parse", "HEAD"]);
    let retry: Value = serde_json::from_str(&success(cli(
        &f.a,
        &[
            "shared",
            "attach",
            ".",
            "--revision",
            "HEAD",
            "--lease",
            "retry-client",
        ],
    )))
    .unwrap();
    assert_eq!(retry["view"], original["view"]);
    assert_eq!(retry["generation"], original["generation"]);
    assert_eq!(retry["requested_commit"], f.revision);
    assert_eq!(retry["leases"], 1);
    let independent = d.attach(&f.a, &f.revision);
    assert_ne!(independent["lease"], retry["lease"]);
    let retry_at_limit = d.rpc("attach", params.clone());
    assert_eq!(retry_at_limit["leases"], 2);
    assert_eq!(retry_at_limit["attach_build"], retry["attach_build"]);
    for (root, revision) in [(&f.b, "HEAD"), (&f.a, moved.as_str())] {
        let mut changed = params.clone();
        changed["root"] = json!(root);
        changed["revision"] = json!(revision);
        let response = d.try_rpc("attach", changed).unwrap();
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("different root or revision"),
            "{response}"
        );
    }
    for token in ["".to_string(), "../unsafe".to_string(), "x".repeat(129)] {
        let mut changed = params.clone();
        changed["lease"] = json!(token);
        let response = d.try_rpc("attach", changed).unwrap();
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("lease must"),
            "{response}"
        );
    }
    let mut missing = d.request("attach", params);
    missing["params"].as_object_mut().unwrap().remove("lease");
    assert!(d.raw(&missing).unwrap().get("error").is_some());
    success(cli(
        &f.a,
        &["shared", "detach", ".", "--lease", "retry-client"],
    ));
    assert_eq!(d.lookup(&f.a)["leases"], 1);
    d.rpc(
        "detach",
        json!({"root":f.a,"view":independent["view"],"lease":independent["lease"]}),
    );
    d.attach_without_response(json!({"root":f.b,"revision":f.revision,"profile":serde_json::from_str::<Value>(PROFILE).unwrap(),"lease":"detach-after-loss"}));
    success(cli(
        &f.b,
        &["shared", "detach", ".", "--lease", "detach-after-loss"],
    ));
    assert!(
        d.try_rpc("lookup", json!({"root":f.b}))
            .unwrap()
            .get("error")
            .is_some()
    );
}

#[test]
fn rejected_reattach_does_not_publish_a_generation() {
    let f = Fixture::new();
    let d = f.start(&["--no-watch"]);
    let attached = d.attach(&f.a, &f.revision);
    git(
        &f.a,
        &[
            "commit",
            "--allow-empty",
            "-qm",
            "same tree different commit",
        ],
    );
    let same_tree = d.attach(&f.a, "HEAD");
    assert_eq!(same_tree["generation"], attached["generation"]);
    assert_ne!(same_tree["requested_commit"], attached["requested_commit"]);
    assert_eq!(same_tree["attach_build"]["blobs_extracted"], 0);
    assert_eq!(same_tree["attach_build"]["tracked_entries"], 3);
    let directory = f
        .storage
        .join("bases")
        .join(d.marker["repository"].as_str().unwrap());
    let generations = || {
        let mut names: Vec<_> = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with("gen-"))
            .collect();
        names.sort();
        names
    };
    let before = generations();
    assert_eq!(before.len(), 1);
    for content in ["new revision one\n", "new revision two\n"] {
        fs::write(f.a.join("new.txt"), content).unwrap();
        git(&f.a, &["add", "."]);
        git(&f.a, &["commit", "-qm", "incompatible tree"]);
        let response = d.try_rpc("attach", json!({"root":f.a,"revision":"HEAD","profile":serde_json::from_str::<Value>(PROFILE).unwrap()})).unwrap();
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("different base"),
            "{response}"
        );
        assert_eq!(generations(), before);
        assert_eq!(d.lookup(&f.a)["generation"], attached["generation"]);
        assert_eq!(d.lookup(&f.a)["leases"], 2);
    }
}

#[test]
fn attached_queries_run_one_client_discovery_and_no_server_git_processes() {
    let f = Fixture::new();
    let home = f.temp.path().join("trace-home");
    fs::create_dir(&home).unwrap();
    let trace = f.temp.path().join("git-trace.jsonl");
    git(
        &f.a,
        &[
            "config",
            "--file",
            home.join(".gitconfig").to_str().unwrap(),
            "trace2.eventTarget",
            trace.to_str().unwrap(),
        ],
    );
    let d = Daemon::start_with_home(&f.a, &f.storage, &["--no-watch"], Some(&home));
    d.attach(&f.a, &f.revision);
    let b = d.attach(&f.b, &f.revision);
    let starts = || {
        fs::read_to_string(&trace)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["event"] == "start")
            .count()
    };
    let before = starts();
    assert!(
        before > 0,
        "real Git trace must be active, including discovery during attach"
    );
    for _ in 0..3 {
        assert_eq!(d.lookup(&f.b)["view"], b["view"]);
        assert_eq!(d.search(&f.b, "shared_term")["backend"], "shared-v1");
        d.rpc("files", json!({"root":f.b,"view":b["view"],"query":{}}));
        d.rpc("status", json!({"root":f.b,"view":b["view"],"query":{}}));
    }
    assert_eq!(
        starts(),
        before,
        "server hot requests must launch no Git subprocesses"
    );
    for args in [
        vec!["--stats", "--", "shared_term", "src"],
        vec!["--files", "src"],
        vec!["status", "."],
    ] {
        let before = starts();
        let output = Command::new(assert_cmd::cargo::cargo_bin("tgrep"))
            .current_dir(&f.b)
            .args(&args)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", &home)
            .output()
            .unwrap();
        if args[0] == "--stats" {
            assert!(String::from_utf8_lossy(&output.stderr).contains("via shared"));
        }
        success(output);
        assert_eq!(
            starts() - before,
            3,
            "{args:?}: exactly one validated client discovery"
        );
    }
    let nested = f.b.join("src/nested");
    fs::create_dir(&nested).unwrap();
    git(&nested, &["init", "-q"]);
    let response = d
        .try_rpc(
            "files",
            json!({"root":f.b,"view":b["view"],"query":{"scope":"src/nested/"}}),
        )
        .unwrap();
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("boundary"),
        "{response}"
    );
    let git_dir = PathBuf::from(git(&f.b, &["rev-parse", "--absolute-git-dir"]));
    let common = fs::read(git_dir.join("commondir")).unwrap();
    fs::write(
        git_dir.join("commondir"),
        nested.join(".git").to_str().unwrap(),
    )
    .unwrap();
    let response = d.try_rpc("lookup", json!({"root":f.b})).unwrap();
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("repository changed"),
        "{response}"
    );
    fs::write(git_dir.join("commondir"), common).unwrap();
    fs::write(
        f.b.join(".git"),
        format!("gitdir: {}\n", f.a.join(".git").display()),
    )
    .unwrap();
    let response = d.try_rpc("lookup", json!({"root":f.b})).unwrap();
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Git directory changed"),
        "{response}"
    );
}

#[test]
fn final_detach_releases_budgets_despite_corrupt_or_missing_marker() {
    let f = Fixture::new();
    let d = f.start(&[
        "--no-watch",
        "--shared-max-views",
        "1",
        "--shared-max-leases",
        "1",
    ]);
    for removed in [false, true] {
        let a = d.attach(&f.a, &f.revision);
        let marker = f.a.join(".git/tgrep-view-v1.json");
        if removed {
            fs::remove_file(&marker).unwrap();
        } else {
            fs::write(&marker, "{invalid").unwrap();
        }
        let detached: Value = serde_json::from_str(&success(cli(
            &f.a,
            &[
                "shared",
                "detach",
                ".",
                "--lease",
                a["lease"].as_str().unwrap(),
            ],
        )))
        .unwrap();
        assert_eq!(detached["detached"], true);
        assert_eq!(detached["remaining_leases"], 0);
        if removed {
            assert!(detached["registration_warning"].is_null());
        } else {
            assert!(
                detached["registration_warning"]
                    .as_str()
                    .unwrap()
                    .contains("cleanup failed")
            );
            assert_eq!(fs::read_to_string(&marker).unwrap(), "{invalid");
        }
        assert!(
            d.try_rpc("lookup", json!({"root":f.a}))
                .unwrap()
                .get("error")
                .is_some()
        );
        let b = d.attach(&f.b, &f.revision);
        d.rpc(
            "detach",
            json!({"root":f.b,"view":b["view"],"lease":b["lease"]}),
        );
    }
}

#[test]
fn native_watching_includes_searchable_nested_tgrep_directories() {
    let f = Fixture::new();
    fs::create_dir(f.a.join(".tgrep")).unwrap();
    fs::write(f.a.join(".tgrep/excluded.txt"), "excluded_storage_marker\n").unwrap();
    fs::create_dir(f.a.join("src/.tgrep")).unwrap();
    fs::write(
        f.a.join("src/.tgrep/private.txt"),
        "old_nested_hidden_marker\n",
    )
    .unwrap();
    let d = f.start(&[
        "--watch-mode",
        "auto",
        "--poll-interval",
        "60",
        "--shared-max-views",
        "1",
    ]);
    let a = d.attach(&f.a, &f.revision);
    let before = d.ready(&f.a);
    assert_eq!(before["watch_mode"], "native", "{before}");
    assert_eq!(before["watch_count"], 3, "{before}");
    let files = d.rpc(
        "files",
        json!({"root":f.a,"view":a["view"],"query":{"hidden":true}}),
    );
    assert!(
        files["files"]
            .as_array()
            .unwrap()
            .contains(&json!("src/.tgrep/private.txt"))
    );
    assert!(
        !files["files"]
            .as_array()
            .unwrap()
            .contains(&json!(".tgrep/excluded.txt"))
    );
    fs::write(
        f.a.join("src/.tgrep/private.txt"),
        "new_nested_hidden_marker\n",
    )
    .unwrap();
    let started = Instant::now();
    loop {
        let status = d.lookup(&f.a);
        if status["ready"] == true
            && status["published_epoch"].as_u64() > before["published_epoch"].as_u64()
        {
            let result = d.rpc("search", json!({"root":f.a,"view":a["view"],"query":{"pattern":"new_nested_hidden_marker","hidden":true}}));
            if !result["matches"].as_array().unwrap().is_empty() {
                assert_eq!(result["backend"], "shared-v1");
                break;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "nested .tgrep native event was ignored: {status}"
        );
        thread::sleep(Duration::from_millis(40));
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_gitfile_metadata_supports_the_full_shared_lifecycle() {
    native_metadata_lifecycle(false);
}

#[cfg(unix)]
#[test]
fn non_utf8_commondir_metadata_supports_the_full_shared_lifecycle() {
    native_metadata_lifecycle(true);
}

#[cfg(unix)]
fn native_metadata_lifecycle(native_commondir: bool) {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::symlink;

    let f = Fixture::new();
    let main = f
        .temp
        .path()
        .join(OsString::from_vec(b"repo-\xff".to_vec()));
    match fs::rename(&f.a, &main) {
        Ok(()) => {}
        Err(error) if cfg!(target_os = "macos") && error.raw_os_error() == Some(92) => {
            eprintln!(
                "filesystem rejects non-UTF-8 names with EILSEQ; native metadata fixture unavailable"
            );
            return;
        }
        Err(error) => panic!("creating repository with native metadata path: {error}"),
    }
    git(&main, &["worktree", "repair"]);
    let repository = tgrep_core::generations::Repository::discover(&f.b).unwrap();
    assert!(fs::read(f.b.join(".git")).unwrap().contains(&0xff));
    if native_commondir {
        let alias = f.temp.path().join("unicode-git-dir");
        symlink(repository.git_dir(), &alias).unwrap();
        fs::write(f.b.join(".git"), format!("gitdir: {}\n", alias.display())).unwrap();
        let mut target = repository.common_dir().as_os_str().as_bytes().to_vec();
        target.push(b'\n');
        fs::write(repository.git_dir().join("commondir"), target).unwrap();
        assert!(!fs::read(f.b.join(".git")).unwrap().contains(&0xff));
        assert!(
            fs::read(repository.git_dir().join("commondir"))
                .unwrap()
                .contains(&0xff)
        );
    }
    let home = f.temp.path().join("trace-home");
    fs::create_dir(&home).unwrap();
    let trace = f.temp.path().join("git-trace.jsonl");
    let global = home.join("global-ignore");
    fs::write(
        &global,
        "global-only.txt\n!info-over-global.txt\ninfo-unignore-global.txt\n",
    )
    .unwrap();
    let info = repository.common_dir().join("info/exclude");
    let info_rules = "info-only.txt\ninfo-over-global.txt\n!info-unignore-global.txt\ngit-over-info.txt\nnested-keep.txt\nboundary.txt\n";
    fs::write(&info, info_rules).unwrap();
    fs::write(
        f.b.join(".gitignore"),
        "!git-over-info.txt\ndot-over-git.txt\nboundary-git.txt\n",
    )
    .unwrap();
    fs::write(f.b.join(".ignore"), "!dot-over-git.txt\n").unwrap();
    fs::create_dir(f.b.join("nested")).unwrap();
    fs::write(
        f.b.join("nested/.gitignore"),
        "nested-only.txt\n!nested-keep.txt\n",
    )
    .unwrap();
    let nested_repo = f.b.join("nested-repo");
    fs::create_dir(&nested_repo).unwrap();
    git(&nested_repo, &["init", "-q"]);
    fs::write(nested_repo.join(".git/info/exclude"), "own-exclude.txt\n").unwrap();
    for relative in [
        "info-only.txt",
        "global-only.txt",
        "info-over-global.txt",
        "info-unignore-global.txt",
        "git-over-info.txt",
        "dot-over-git.txt",
        "nested/nested-only.txt",
        "nested/nested-keep.txt",
        "nested-repo/boundary.txt",
        "nested-repo/boundary-git.txt",
        "nested-repo/own-exclude.txt",
        "nested-repo/global-only.txt",
    ] {
        fs::write(f.b.join(relative), "shared_term ignore_precedence\n").unwrap();
    }
    git(
        &main,
        &[
            "config",
            "--file",
            home.join(".gitconfig").to_str().unwrap(),
            "trace2.eventTarget",
            trace.to_str().unwrap(),
        ],
    );
    git(
        &main,
        &[
            "config",
            "--file",
            home.join(".gitconfig").to_str().unwrap(),
            "core.excludesFile",
            global.to_str().unwrap(),
        ],
    );
    let d = Daemon::start_with_home(&f.b, &f.storage, &["--no-watch"], Some(&home));
    let a = d.attach(&f.b, &f.revision);
    let b = d.attach(&f.b, &f.revision);
    let starts = || {
        String::from_utf8_lossy(&fs::read(&trace).unwrap())
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["event"] == "start")
            .count()
    };
    let before = starts();
    assert!(before > 0, "trace must include actual daemon Git starts");
    assert_eq!(d.lookup(&f.b)["view"], a["view"]);
    assert_eq!(
        d.rpc("status", json!({"root":f.b,"view":a["view"],"query":{}}))["ready"],
        true
    );
    assert_eq!(d.search(&f.b, "shared_term")["backend"], "shared-v1");
    assert!(
        d.rpc("files", json!({"root":f.b,"view":a["view"],"query":{}}))["files"]
            .as_array()
            .unwrap()
            .contains(&json!("notes.txt"))
    );
    assert_eq!(
        starts(),
        before,
        "native metadata must not add server Git starts"
    );
    let listed = d.rpc("files", json!({"root":f.b,"view":a["view"],"query":{}}));
    let listed = listed["files"].as_array().unwrap();
    for relative in [
        "info-unignore-global.txt",
        "git-over-info.txt",
        "dot-over-git.txt",
        "nested/nested-keep.txt",
        "nested-repo/boundary.txt",
        "nested-repo/boundary-git.txt",
    ] {
        assert!(listed.contains(&json!(relative)), "{relative}: {listed:?}");
    }
    for relative in [
        "info-only.txt",
        "global-only.txt",
        "info-over-global.txt",
        "nested/nested-only.txt",
        "nested-repo/own-exclude.txt",
        "nested-repo/global-only.txt",
    ] {
        assert!(!listed.contains(&json!(relative)), "{relative}: {listed:?}");
    }
    for args in [
        vec!["--stats", "--", "shared_term", "."],
        vec!["--files", "."],
        vec!["status", "."],
    ] {
        let before = starts();
        let output = Command::new(assert_cmd::cargo::cargo_bin("tgrep"))
            .current_dir(&f.b)
            .args(&args)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", &home)
            .output()
            .unwrap();
        if args[0] == "--stats" {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("via shared"),
                "{output:?}"
            );
        }
        success(output);
        assert_eq!(starts() - before, 3, "{args:?}: one client discovery only");
    }
    let run = |args: &[&str]| {
        Command::new(assert_cmd::cargo::cargo_bin("tgrep"))
            .current_dir(&f.b)
            .args(args)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", &home)
            .output()
            .unwrap()
    };
    for args in [
        vec!["--files", "--sort", "path", "."],
        vec!["--sort", "path", "--", "shared_term", "."],
    ] {
        let indexed = run(&args);
        let mut scan = vec!["--no-index"];
        scan.extend(args);
        let scanned = run(&scan);
        assert_eq!(indexed.status.code(), scanned.status.code());
        assert_eq!(success(indexed), success(scanned));
    }
    fs::write(&info, "[z-a]\n").unwrap();
    let failed = d
        .try_rpc(
            "refresh",
            json!({"root":f.b,"view":a["view"],"lease":a["lease"],"full":true}),
        )
        .unwrap();
    assert!(
        failed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("discovery"),
        "{failed}"
    );
    assert_eq!(d.lookup(&f.b)["ready"], false);
    fs::write(&info, format!("{info_rules}!info-only.txt\n")).unwrap();
    fs::write(f.b.join("notes.txt"), "native_metadata_refresh\n").unwrap();
    assert_eq!(d.refresh(&f.b, &a, &["notes.txt"], false)["ready"], true);
    assert!(
        !d.search(&f.b, "native_metadata_refresh")["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        d.rpc("files", json!({"root":f.b,"view":a["view"],"query":{}}))["files"]
            .as_array()
            .unwrap()
            .contains(&json!("info-only.txt"))
    );
    success(cli(
        &f.b,
        &[
            "shared",
            "refresh",
            ".",
            "--lease",
            b["lease"].as_str().unwrap(),
            "--full",
        ],
    ));
    assert_eq!(
        d.rpc(
            "detach",
            json!({"root":f.b,"view":a["view"],"lease":a["lease"]})
        )["remaining_leases"],
        1
    );
    success(cli(
        &f.b,
        &[
            "shared",
            "detach",
            ".",
            "--lease",
            b["lease"].as_str().unwrap(),
        ],
    ));
    assert!(!repository.git_dir().join("tgrep-view-v1.json").exists());
}

#[cfg(unix)]
#[test]
fn non_utf8_canonical_roots_fail_before_json_or_generation_publication() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    let f = Fixture::new();
    let root = f
        .temp
        .path()
        .join(OsString::from_vec(b"non-utf8-\xff".to_vec()));
    match fs::create_dir(&root) {
        Ok(()) => {}
        Err(error) if cfg!(target_os = "macos") && error.raw_os_error() == Some(92) => {
            eprintln!(
                "filesystem rejects non-UTF-8 names with EILSEQ; no such root can be created"
            );
            return;
        }
        Err(error) => panic!("creating non-UTF-8 worktree directory: {error}"),
    }
    success(
        Command::new("git")
            .current_dir(&f.a)
            .args(["worktree", "add", "--detach", "-q"])
            .arg(&root)
            .arg(&f.revision)
            .output()
            .unwrap(),
    );
    let alias = f.temp.path().join("unicode-alias");
    symlink(&root, &alias).unwrap();
    let d = f.start(&[
        "--no-watch",
        "--shared-max-views",
        "1",
        "--shared-max-leases",
        "1",
    ]);
    for path in [&root, &alias] {
        let output = Command::new(assert_cmd::cargo::cargo_bin("tgrep"))
            .current_dir(&f.a)
            .args(["shared", "attach"])
            .arg(path)
            .args(["--revision", &f.revision, "--lease", "invalid-root"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("UTF-8 root"),
            "{output:?}"
        );
    }
    let response = d.try_rpc("attach", json!({"root":alias,"revision":f.revision,"profile":serde_json::from_str::<Value>(PROFILE).unwrap(),"lease":"invalid-root"})).unwrap();
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("UTF-8 root"),
        "{response}"
    );
    let directory = f
        .storage
        .join("bases")
        .join(d.marker["repository"].as_str().unwrap());
    assert!(
        fs::read_dir(directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("gen-")
        }),
        "invalid roots must not publish retained generations"
    );
    assert_eq!(d.attach(&f.a, &f.revision)["leases"], 1);
}

#[test]
fn shared_limits_require_explicit_opt_in() {
    let f = Fixture::new();
    for flag in ["--shared-max-views", "--shared-max-leases"] {
        let output = cli(&f.a, &["serve", "missing-root", flag, "1"]);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("required") && stderr.contains("--shared"),
            "{stderr}"
        );
    }
    let legacy = cli(&f.a, &["serve", "missing-root", "--no-watch"]);
    assert_eq!(legacy.status.code(), Some(2), "{legacy:?}");
    assert!(
        !String::from_utf8_lossy(&legacy.stderr).contains("--shared"),
        "{legacy:?}"
    );
}

#[test]
fn canonical_directory_hints_refresh_same_size_restored_mtime_edits() {
    let f = Fixture::new();
    let d = f.start(&["--no-watch"]);
    let a = d.attach(&f.a, &f.revision);
    d.attach(&f.b, &f.revision);
    let path = f.a.join("src/main.rs");
    let mut previous = "shared_term";
    for (hint, next) in [("src/", "ZXQJVPKBMWH"), ("src//./", "KZVXJQWBMPH")] {
        let metadata = fs::metadata(&path).unwrap();
        let updated = fs::read_to_string(&path).unwrap().replace(previous, next);
        fs::write(&path, updated).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), metadata.len());
        assert!(
            d.search(&f.a, next)["matches"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let refreshed: Value = serde_json::from_str(&success(cli(
            &f.a,
            &[
                "shared",
                "refresh",
                ".",
                "--lease",
                a["lease"].as_str().unwrap(),
                "--changed",
                hint,
            ],
        )))
        .unwrap();
        assert_eq!(refreshed["ready"], true);
        assert_eq!(refreshed["last_reconcile"]["full"], false);
        assert!(
            refreshed["last_reconcile"]["hint_lookups"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(refreshed["processed_epoch"].as_u64().unwrap() >= a["epoch"].as_u64().unwrap());
        assert!(
            !d.search(&f.a, next)["matches"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            d.search(&f.b, next)["matches"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        previous = next;
    }
}

#[test]
fn persistent_reconcile_failures_back_off_without_blocking_explicit_repair() {
    let f = Fixture::new();
    let d = f.start(&["--no-watch"]);
    let a = d.attach(&f.a, &f.revision);
    d.attach(&f.b, &f.revision);
    let key: tgrep_core::generations::GenerationKey =
        serde_json::from_value(a["generation"].clone()).unwrap();
    let root = fs::canonicalize(&f.a).unwrap();
    let root_id = blake3::hash(root.to_str().unwrap().as_bytes())
        .to_hex()
        .to_string();
    let checkpoint = f
        .storage
        .join("overlays")
        .join(key.repository_identity())
        .join(root_id)
        .join(key.storage_name())
        .join("overlay.json");
    fs::remove_file(&checkpoint).unwrap();
    fs::create_dir(&checkpoint).unwrap();
    let before = d.lookup(&f.a)["reconcile_attempts"].as_u64().unwrap();
    let failure = d
        .try_rpc(
            "refresh",
            json!({"root":f.a,"view":a["view"],"lease":a["lease"],"full":true}),
        )
        .unwrap();
    assert!(failure.get("error").is_some(), "{failure}");
    let started = Instant::now();
    let failed = loop {
        let status = d.lookup(&f.a);
        assert_eq!(status["ready"], false);
        assert!(status["last_error"].is_string());
        if status["consecutive_failures"].as_u64().unwrap() >= 2 {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "background retry never happened: {status}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(failed["reconcile_attempts"], before + 2);
    assert_eq!(failed["retry_delay_ms"], 2000);
    let stable = Instant::now();
    while stable.elapsed() < Duration::from_millis(600) {
        let status = d.lookup(&f.a);
        assert_eq!(
            status["reconcile_attempts"], failed["reconcile_attempts"],
            "{status}"
        );
        assert_eq!(d.search(&f.b, "shared_term")["backend"], "shared-v1");
        thread::sleep(Duration::from_millis(30));
    }
    fs::remove_dir(&checkpoint).unwrap();
    let repaired = d.refresh(&f.a, &a, &[], true);
    assert_eq!(repaired["ready"], true);
    assert_eq!(repaired["consecutive_failures"], 0);
    assert_eq!(repaired["reconcile_attempts"], before + 3);
    assert!(repaired["last_error"].is_null());
    assert_eq!(repaired["retry_delay_ms"], 200);
}

#[test]
fn oversized_single_file_response_errors_and_cli_scans_without_truncation() {
    let f = Fixture::new();
    fs::write(
        f.a.join("oversized.txt"),
        format!("oversize_marker{}\n", "\u{1}".repeat(12 * 1024 * 1024)),
    )
    .unwrap();
    let d = f.start(&["--no-watch"]);
    let a = d.attach(&f.a, &f.revision);
    let response = d.try_rpc("search", json!({"root":f.a,"view":a["view"],"query":{"pattern":"oversize_marker","detail":true,"positions":true}})).unwrap();
    assert!(response.get("result").is_none(), "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("response exceeds 64 MiB"),
        "{response}"
    );
    let output = cli(&f.a, &["-c", "--", "oversize_marker", "."]);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("scanning filesystem"),
        "{output:?}"
    );
    assert!(success(output).contains("oversized.txt:1"));
    assert_eq!(d.search(&f.a, "shared_term")["backend"], "shared-v1");
}

#[cfg(unix)]
#[test]
fn dangling_view_marker_never_reenables_a_stale_ordinary_index() {
    use std::os::unix::fs::symlink;

    let f = Fixture::new();
    success(cli(&f.a, &["index", "."]));
    let d = f.start(&["--no-watch"]);
    let a = d.attach(&f.a, &f.revision);
    let marker = f.a.join(".git/tgrep-view-v1.json");
    fs::remove_file(&marker).unwrap();
    symlink(f.temp.path().join("missing-marker-target"), &marker).unwrap();
    fs::write(f.a.join("after-index.txt"), "dangling_marker_fallback\n").unwrap();
    let output = cli(&f.a, &["--stats", "--", "dangling_marker_fallback", "."]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("scanning filesystem"));
    assert!(success(output).contains("dangling_marker_fallback"));
    assert!(success(cli(&f.a, &["--files", "."])).contains("after-index.txt"));
    assert_eq!(cli(&f.a, &["status", "."]).status.code(), Some(2));
    let detached = d.rpc(
        "detach",
        json!({"root":f.a,"view":a["view"],"lease":a["lease"]}),
    );
    assert_eq!(detached["remaining_leases"], 0);
    assert!(detached["registration_warning"].is_string());
    assert!(
        fs::symlink_metadata(&marker)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(d.attach(&f.a, &f.revision)["leases"], 1);
}
