use super::*;
use std::cell::RefCell;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicUsize;
use tempfile::TempDir;

thread_local! {
    static STARTUP_HOOK: RefCell<Option<StaleRefreshHook>> = const { RefCell::new(None) };
}

pub(super) fn startup_hook() -> Option<StaleRefreshHook> {
    STARTUP_HOOK.with(|hook| hook.borrow().clone())
}

fn populate(root: &Path) {
    std::fs::create_dir_all(root.join(".git")).unwrap();
    for number in 0..1200 {
        std::fs::write(
            root.join(format!("file-{number:04}.rs")),
            format!("fn checkpoint_marker_{number:04}() {{}}\n"),
        )
        .unwrap();
    }
}

fn search(state: &ServerState, pattern: &str) -> serde_json::Value {
    serde_json::from_str(&handle_search(
        None,
        &serde_json::json!({"pattern": pattern}),
        state,
    ))
    .unwrap()
}

#[test]
fn checkpoint_failure_preserves_progress_and_reconciles_on_resume() {
    let temp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    populate(&root);
    let mut state = tests::test_server_state(&root, &root.join(".tgrep"));
    Arc::get_mut(&mut state).unwrap().watch_enabled = false;
    state.indexing.store(true, Ordering::SeqCst);
    let weak = Arc::downgrade(&state);
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&checkpoints);
    let global_threads = rayon::current_num_threads();
    let global_setting = std::env::var_os("RAYON_NUM_THREADS");
    *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::AfterBuildCheckpoint) {
            assert_eq!(rayon::current_num_threads(), 1);
            assert!(
                thread::current()
                    .name()
                    .unwrap()
                    .starts_with("tgrep-index-")
            );
            let state = weak.upgrade().unwrap();
            assert!(state.indexing.load(Ordering::SeqCst));
            assert!(!state.hidden_complete.load(Ordering::SeqCst));
            assert!(!state.index.read().unwrap().live.has_pending_changes());
            assert!(search(&state, "checkpoint_marker").get("error").is_some());
            assert!(
                !tgrep_core::meta::IndexMeta::load(&state.index_dir)
                    .unwrap()
                    .complete
            );
            assert_eq!(observed.fetch_add(1, Ordering::SeqCst), 0);
            std::fs::write(state.index_dir.join(".bootstrap-build"), b"blocked staging").unwrap();
        }
    }));
    background_index_build(&state, &root, &state.index_dir);
    assert_eq!(checkpoints.load(Ordering::SeqCst), 1);
    assert!(state.index_build_error.lock().unwrap().is_some());
    assert_eq!(rayon::current_num_threads(), global_threads);
    assert_eq!(std::env::var_os("RAYON_NUM_THREADS"), global_setting);
    let mut saved: Vec<_> = state
        .index
        .read()
        .unwrap()
        .reader_paths()
        .into_iter()
        .collect();
    saved.sort();
    assert_eq!(saved.len(), 1024);
    let evidence = tgrep_core::meta::read_file_evidence(&state.index_dir).unwrap();
    assert_eq!(evidence.versions.len(), saved.len());

    std::fs::remove_file(state.index_dir.join(".bootstrap-build")).unwrap();
    std::fs::write(root.join(&saved[0]), "fn changed_after_checkpoint() {}\n").unwrap();
    std::fs::remove_file(root.join(&saved[1])).unwrap();
    std::fs::write(root.join(".gitignore"), format!("{}\n", saved[2])).unwrap();
    std::fs::write(root.join("late.rs"), "fn arrived_after_checkpoint() {}\n").unwrap();
    *state.stale_refresh_hook.lock().unwrap() = None;
    state.indexing.store(true, Ordering::SeqCst);
    background_index_build(&state, &root, &state.index_dir);
    assert!(!state.indexing.load(Ordering::SeqCst));
    assert!(state.index_build_error.lock().unwrap().is_none());
    assert!(state.hidden_complete.load(Ordering::SeqCst));
    assert!(
        tgrep_core::meta::IndexMeta::load(&state.index_dir)
            .unwrap()
            .complete
    );
    for marker in [
        "changed_after_checkpoint",
        "arrived_after_checkpoint",
        "checkpoint_marker_1199",
    ] {
        assert_eq!(
            search(&state, marker)["result"]["num_matches"],
            1,
            "{marker}"
        );
    }
    let paths = state.index.read().unwrap().reader_paths();
    assert!(!paths.contains(&saved[1]));
    assert!(!paths.contains(&saved[2]));
}

#[test]
fn checkpoint_does_not_skip_evidence_for_missing_reader_entries() {
    let temp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    std::fs::write(root.join("seeded.rs"), "fn seeded_marker() {}\n").unwrap();
    let mut state = tests::test_server_state(&root, &root.join(".tgrep"));
    Arc::get_mut(&mut state).unwrap().watch_enabled = false;
    builder::build_index(&root, Some(&state.index_dir), true, false, &[]).unwrap();
    *state.index.write().unwrap() = HybridIndex::open(&state.index_dir, &root).unwrap();
    let missing = root.join("missing.rs");
    let bytes = b"fn missing_checkpoint_entry() {}\n";
    std::fs::write(&missing, bytes).unwrap();
    let version = builder::file_version(&std::fs::metadata(&missing).unwrap());
    let mut evidence = tgrep_core::meta::read_file_evidence(&state.index_dir).unwrap();
    evidence.insert_verified(
        "missing.rs".into(),
        version.stamp().clone(),
        Some(tgrep_core::meta::ContentId::from_indexed_bytes(bytes)),
        Some(version),
    );
    tgrep_core::meta::write_file_evidence(&evidence, &state.index_dir).unwrap();
    let weak = Arc::downgrade(&state);
    *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::AfterBuildCheckpoint) {
            assert!(
                weak.upgrade()
                    .unwrap()
                    .index
                    .read()
                    .unwrap()
                    .has_active_path("missing.rs")
            );
        }
    }));
    state.indexing.store(true, Ordering::SeqCst);
    background_index_build(&state, &root, &state.index_dir);
    assert!(state.index_build_error.lock().unwrap().is_none());
    assert_eq!(
        search(&state, "missing_checkpoint_entry")["result"]["num_matches"],
        1
    );
}

#[test]
fn checkpointed_build_accepts_empty_short_and_binary_only_files() {
    for bytes in [b"".as_slice(), b"x", b"xy", b"\0binary"] {
        let temp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::write(root.join("source.rs"), bytes).unwrap();
        let mut state = tests::test_server_state(&root, &root.join(".tgrep"));
        Arc::get_mut(&mut state).unwrap().watch_enabled = false;
        state.indexing.store(true, Ordering::SeqCst);
        background_index_build(&state, &root, &state.index_dir);
        assert!(state.index_build_error.lock().unwrap().is_none());
        assert!(!state.indexing.load(Ordering::SeqCst));
        assert!(state.hidden_complete.load(Ordering::SeqCst));
        assert!(
            tgrep_core::meta::IndexMeta::load(&state.index_dir)
                .unwrap()
                .complete
        );
    }
}

struct ServerChild {
    child: Child,
    log: PathBuf,
}

impl ServerChild {
    fn start(root: &Path, log: PathBuf, pause: bool) -> Self {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "serve::build_tests::checkpoint_child_process",
                "--nocapture",
            ])
            .env("TGREP_CHECKPOINT_TEST_ROOT", root)
            .env("TGREP_CHECKPOINT_TEST_PAUSE", if pause { "1" } else { "0" })
            .env("RAYON_NUM_THREADS", "2")
            .stdout(Stdio::null())
            .stderr(File::create(&log).unwrap())
            .spawn()
            .unwrap();
        Self { child, log }
    }

    fn wait_for(&mut self, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "server exited: {}",
                std::fs::read_to_string(&self.log).unwrap()
            );
            if condition() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "server did not reach its checkpoint: {}",
                std::fs::read_to_string(&self.log).unwrap()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}

impl Drop for ServerChild {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(error) => eprintln!("could not inspect owned checkpoint child: {error}"),
        }
        if let Err(error) = self.child.kill() {
            eprintln!("could not kill owned checkpoint child: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("could not reap owned checkpoint child: {error}");
        }
    }
}

fn rpc(port: u16, method: &str, params: serde_json::Value) -> serde_json::Value {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
        })
    )
    .unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    serde_json::from_str(&response).unwrap()
}

#[test]
fn killed_server_resumes_its_checkpoint_and_serves_indexed_results() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    populate(&root);
    let index_dir = root.join(".tgrep");
    let mut first = ServerChild::start(&root, temp.path().join("first.log"), true);
    first.wait_for(|| index_dir.join("checkpoint-test-paused").exists());
    let port = ServerInfo::load(&index_dir).unwrap().port;
    let status = rpc(port, "status", serde_json::json!({}));
    assert_eq!(status["result"]["indexing"], true);
    assert_eq!(status["result"]["hidden_complete"], false);
    assert!(
        rpc(
            port,
            "search",
            serde_json::json!({"pattern": "checkpoint_marker"})
        )
        .get("error")
        .is_some()
    );
    first.stop();
    let reader = tgrep_core::reader::IndexReader::open(&index_dir).unwrap();
    assert_eq!(reader.num_files(), 1024);
    assert!(
        !tgrep_core::meta::IndexMeta::load(&index_dir)
            .unwrap()
            .complete
    );
    let saved = reader.all_paths()[0].clone();
    let pending = (0..1200)
        .find(|number| !reader.all_paths().contains(&format!("file-{number:04}.rs")))
        .unwrap();
    drop(reader);
    std::fs::write(root.join(&saved), "fn changed_after_restart() {}\n").unwrap();
    std::fs::write(root.join("late.rs"), "fn created_after_restart() {}\n").unwrap();
    let spill = index_dir.join("spill-123-456.tmp");
    std::fs::create_dir(&spill).unwrap();
    std::fs::write(spill.join("segment.bin"), b"abandoned").unwrap();

    let mut resumed = ServerChild::start(&root, temp.path().join("resumed.log"), false);
    let resumed_pid = resumed.child.id();
    resumed.wait_for(|| {
        ServerInfo::load(&index_dir).is_ok_and(|info| {
            info.pid == resumed_pid
                && rpc(info.port, "status", serde_json::json!({}))["result"]["hidden_complete"]
                    == true
        })
    });
    assert!(!spill.exists());
    let port = ServerInfo::load(&index_dir).unwrap().port;
    for marker in [
        "changed_after_restart",
        "created_after_restart",
        &format!("checkpoint_marker_{pending:04}"),
    ] {
        assert_eq!(
            rpc(port, "search", serde_json::json!({"pattern": marker}))["result"]["num_matches"],
            1,
            "{marker}"
        );
    }
    assert!(
        std::fs::read_to_string(&resumed.log)
            .unwrap()
            .contains("partial")
    );
    assert!(
        tgrep_core::meta::IndexMeta::load(&index_dir)
            .unwrap()
            .complete
    );
    resumed.stop();
}

#[test]
#[ignore = "owned subprocess for checkpoint termination coverage"]
fn checkpoint_child_process() {
    let root = PathBuf::from(std::env::var_os("TGREP_CHECKPOINT_TEST_ROOT").unwrap());
    let index_dir = root.join(".tgrep");
    let pause = std::env::var("TGREP_CHECKPOINT_TEST_PAUSE").unwrap() == "1";
    assert_eq!(rayon::current_num_threads(), 2);
    STARTUP_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::AfterBuildCheckpoint) {
                assert_eq!(rayon::current_num_threads(), 1);
                if pause {
                    assert!(
                        !tgrep_core::meta::IndexMeta::load(&index_dir)
                            .unwrap()
                            .complete
                    );
                    std::fs::write(index_dir.join("checkpoint-test-paused"), b"ready").unwrap();
                    loop {
                        thread::park();
                    }
                }
            }
        }));
    });
    run(
        &root,
        None,
        ServeOptions {
            no_watch: true,
            watch_mode: WatchMode::Auto,
            poll_interval: Duration::from_secs(120),
            watch_budget: 8192,
            exclude_dirs: &[],
            memory_cap_bytes: 512 * 1024 * 1024,
            index_threads: 1,
            no_ignore: false,
            no_require_git: true,
            max_file_size: tgrep_core::walker::DEFAULT_MAX_FILE_SIZE,
            auto_save_mutations: None,
            watcher_queue_cap: None,
        },
    )
    .unwrap();
}
