use super::tests::test_server_state;
use super::*;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Weak, mpsc};
use tempfile::TempDir;

struct Fixture {
    state: Arc<ServerState>,
    root: PathBuf,
    _temp: TempDir,
}

impl Fixture {
    fn new(mode: WatchMode, budget: usize) -> Self {
        Self::with_interval(mode, budget, Duration::from_secs(120))
    }

    fn with_interval(mode: WatchMode, budget: usize, interval: Duration) -> Self {
        let temp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let mut state = test_server_state(&root, &root.join(".tgrep"));
        let unique = Arc::get_mut(&mut state).unwrap();
        unique.refresh = RefreshControl::new(mode, interval, budget);
        unique.no_require_git = true;
        Self {
            state,
            root,
            _temp: temp,
        }
    }

    fn write(&self, relative: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn due(&self) {
        let mut status = self.state.refresh.status.lock().unwrap();
        status.finished = Instant::now()
            .checked_sub(self.state.refresh.poll_interval)
            .unwrap();
    }

    fn tick(&self) -> Option<bool> {
        scheduled_reconcile(&self.state, &self.root, &self.state.index_dir)
    }

    fn poll(&self) {
        self.due();
        assert_eq!(self.tick(), Some(true));
        self.assert_polling();
    }

    fn native_reconcile(&self) {
        assert!(native_watching(&self.state));
        self.state.refresh.status.lock().unwrap().finished = Instant::now() - RECONCILE_DEADLINE;
        assert_eq!(self.tick(), Some(true));
    }

    fn assert_polling(&self) {
        assert!(self.state.watch_enabled);
        assert!(self.state.refresh.polling.load(Ordering::SeqCst));
        assert!(!native_watching(&self.state));
        assert!(!self.state.watcher_active.load(Ordering::SeqCst));
        assert!(self.state.watch_registry.lock().unwrap().is_none());
    }

    fn assert_files(&self, expected: &[&str]) {
        let response = process_request(r#"{"jsonrpc":"2.0","method":"files","id":1}"#, &self.state);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        let actual: BTreeSet<_> = value["result"]["files"]
            .as_array()
            .unwrap_or_else(|| panic!("files RPC failed: {response}"))
            .iter()
            .map(|path| path.as_str().unwrap())
            .collect();
        assert_eq!(actual, expected.iter().copied().collect());
    }

    fn assert_hit(&self, pattern: &str, expected_path: &str) {
        let response = handle_search(None, &serde_json::json!({"pattern": pattern}), &self.state);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        let matches = value["result"]["matches"]
            .as_array()
            .unwrap_or_else(|| panic!("search RPC failed: {response}"));
        assert!(
            matches.iter().any(|hit| {
                hit["file"] == expected_path
                    && hit["content"]
                        .as_str()
                        .is_some_and(|text| text.contains(pattern))
            }),
            "missing {pattern} in {expected_path}: {response}"
        );
    }

    fn desired(&self) -> WatchableDirs {
        let matcher = self.state.gitignore.read().unwrap();
        watchable_dirs(
            &self.root,
            &self.root,
            &self.state.exclude_dirs,
            matcher.as_ref(),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Also break notify's callback -> state -> registry cycle on a panic.
        self.state.refresh.polling.store(true, Ordering::SeqCst);
        stop_native_watcher(&self.state);
    }
}

fn disk_snapshot(index_dir: &Path) -> BTreeMap<String, (Vec<u8>, SystemTime)> {
    std::fs::read_dir(index_dir)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_file())
        .map(|entry| {
            (
                entry.file_name().into_string().unwrap(),
                (
                    std::fs::read(entry.path()).unwrap(),
                    entry.metadata().unwrap().modified().unwrap(),
                ),
            )
        })
        .collect()
}

fn assert_binary_evidence(fixture: &Fixture, relative: &str) {
    let version = builder::file_version(&std::fs::metadata(fixture.root.join(relative)).unwrap());
    let evidence = fixture.state.file_evidence.read().unwrap();
    assert_eq!(evidence.version(relative), Some(&version), "{relative}");
    assert_eq!(evidence.content_id(relative), None, "{relative}");
    drop(evidence);
    assert!(
        !fixture
            .state
            .index
            .read()
            .unwrap()
            .has_active_path(relative),
        "binary classification must not add content postings for {relative}"
    );
    assert!(
        fixture
            .state
            .filename_extra_paths
            .read()
            .unwrap()
            .contains(relative)
    );
}

#[test]
fn verified_binary_reindex_preserves_evidence_without_rewriting_the_index() {
    for force in [false, true] {
        let fixture = Fixture::new(WatchMode::Poll, 4);
        fixture.write("seeded.rs", "fn seeded_marker() {}\n");
        fixture.poll();
        let binary = fixture.write("binary.rs", b"binary_marker\n\0");
        {
            let _gate = fixture.state.snapshot_gate.read().unwrap();
            reindex_file(&fixture.state, &binary, "binary.rs", force);
        }
        assert_binary_evidence(&fixture, "binary.rs");
        assert!(
            !fixture
                .state
                .index
                .read()
                .unwrap()
                .live
                .has_pending_changes()
        );
        assert!(persist_pending_index_changes(&fixture.state));

        let reads = Arc::new(AtomicUsize::new(0));
        let hook_reads = Arc::clone(&reads);
        *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::AfterConcreteRead) {
                hook_reads.fetch_add(1, Ordering::SeqCst);
            }
        }));
        let before = disk_snapshot(&fixture.state.index_dir);
        let evidence = fixture.state.file_evidence.read().unwrap().clone();
        for _ in 0..2 {
            {
                let _gate = fixture.state.snapshot_gate.read().unwrap();
                reindex_file(&fixture.state, &binary, "binary.rs", false);
            }
            fixture.poll();
        }
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert_eq!(disk_snapshot(&fixture.state.index_dir), before);
        assert_eq!(*fixture.state.file_evidence.read().unwrap(), evidence);
        fixture.assert_files(&["binary.rs", "seeded.rs"]);
    }
}

#[test]
fn verified_binary_reindex_preserves_text_binary_text_transitions() {
    for force in [false, true] {
        let fixture = Fixture::new(WatchMode::Poll, 4);
        let path = fixture.write("source.rs", "fn original_text_marker() {}\n");
        fixture.poll();
        fixture.assert_hit("original_text_marker", "source.rs");
        fixture.write("source.rs", b"binary_content\n\0");
        {
            let _gate = fixture.state.snapshot_gate.read().unwrap();
            reindex_file(&fixture.state, &path, "source.rs", force);
        }
        assert_binary_evidence(&fixture, "source.rs");
        assert!(
            fixture
                .state
                .cache
                .read()
                .unwrap()
                .peek("source.rs")
                .is_none()
        );
        assert!(persist_pending_index_changes(&fixture.state));
        let persisted = tgrep_core::meta::read_file_evidence(&fixture.state.index_dir).unwrap();
        assert_eq!(
            persisted,
            *fixture.state.file_evidence.read().unwrap(),
            "the content eviction must publish the verified binary classification"
        );
        let before = disk_snapshot(&fixture.state.index_dir);
        fixture.poll();
        assert_eq!(disk_snapshot(&fixture.state.index_dir), before);

        fixture.write("source.rs", "fn restored_text_marker() {}\n");
        {
            let _gate = fixture.state.snapshot_gate.read().unwrap();
            reindex_file(&fixture.state, &path, "source.rs", force);
        }
        fixture.assert_hit("restored_text_marker", "source.rs");
        assert!(
            !fixture
                .state
                .filename_extra_paths
                .read()
                .unwrap()
                .contains("source.rs")
        );
        assert!(
            fixture
                .state
                .file_evidence
                .read()
                .unwrap()
                .content_id("source.rs")
                .is_some()
        );
        fixture.assert_files(&["source.rs"]);
    }
}

#[test]
fn verified_binary_reindex_rejects_raced_classification() {
    for force in [false, true] {
        let fixture = Fixture::new(WatchMode::Poll, 4);
        let path = fixture.write("source.rs", "fn original_text_marker() {}\n");
        fixture.poll();
        fixture.write("source.rs", b"binary_content\n\0");
        let hook_path = path.clone();
        *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::BeforeConcreteCommit) {
                std::fs::write(&hook_path, "fn latest_text_marker() {}\n").unwrap();
            }
        }));
        {
            let _gate = fixture.state.snapshot_gate.read().unwrap();
            reindex_file(&fixture.state, &path, "source.rs", force);
        }
        assert!(
            fixture
                .state
                .file_evidence
                .read()
                .unwrap()
                .version("source.rs")
                .is_none()
        );
        assert!(
            fixture
                .state
                .index
                .read()
                .unwrap()
                .has_active_path("source.rs")
        );
        assert!(
            !fixture
                .state
                .filename_extra_paths
                .read()
                .unwrap()
                .contains("source.rs")
        );
        *fixture.state.stale_refresh_hook.lock().unwrap() = None;
        fixture.poll();
        fixture.assert_hit("latest_text_marker", "source.rs");
    }
}

#[test]
fn resumed_build_preserves_binary_evidence_across_batches_and_flushes() {
    for force_flush in [false, true] {
        let mut fixture = Fixture::new(WatchMode::Poll, 4);
        if force_flush {
            Arc::get_mut(&mut fixture.state).unwrap().memory_cap_bytes = 0;
        }
        fixture.write("seeded.rs", "fn seeded_marker() {}\n");
        fixture.write("existing.rs", b"existing_binary\n\0");
        fixture.poll();
        let mut binaries = vec!["existing.rs".to_string()];
        for number in 0..501 {
            let name = format!("binary-{number:03}.rs");
            fixture.write(&name, b"batch_binary\n\0");
            binaries.push(name);
        }
        fixture.write("text.rs", "fn new_text_marker() {}\n");
        fixture.state.indexing.store(true, Ordering::SeqCst);
        background_index_build(&fixture.state, &fixture.root, &fixture.state.index_dir);
        assert!(!fixture.state.indexing.load(Ordering::SeqCst));
        assert!(!fixture.state.flushing.load(Ordering::SeqCst));
        assert_eq!(fixture.state.index.read().unwrap().num_files(), 2);
        fixture.assert_hit("new_text_marker", "text.rs");
        for name in &binaries {
            assert_binary_evidence(&fixture, name);
        }
        let persisted = tgrep_core::meta::read_file_evidence(&fixture.state.index_dir).unwrap();
        assert_eq!(persisted, *fixture.state.file_evidence.read().unwrap());
        assert_eq!(persisted.versions.len(), binaries.len() + 2);
        let before = disk_snapshot(&fixture.state.index_dir);
        fixture.poll();
        fixture.poll();
        assert_eq!(disk_snapshot(&fixture.state.index_dir), before);
        let mut expected: Vec<_> = binaries.iter().map(String::as_str).collect();
        expected.extend(["seeded.rs", "text.rs"]);
        fixture.assert_files(&expected);
    }
}

#[test]
fn resumed_build_binary_evidence_describes_the_read_not_the_final_walk() {
    let fixture = Fixture::new(WatchMode::Poll, 4);
    fixture.write("seeded.rs", "fn seeded_marker() {}\n");
    fixture.poll();
    let path = fixture.write("raced.rs", b"original_binary\n\0");
    let version = builder::file_version(&std::fs::metadata(&path).unwrap());
    let hook_root = fixture.root.clone();
    *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::AfterBuildBeforeStampPublish) {
            std::fs::write(&path, "fn after_classification_marker() {}\n").unwrap();
            std::fs::write(hook_root.join("late.rs"), "fn late_arrival_marker() {}\n").unwrap();
        }
    }));
    fixture.state.indexing.store(true, Ordering::SeqCst);
    background_index_build(&fixture.state, &fixture.root, &fixture.state.index_dir);
    *fixture.state.stale_refresh_hook.lock().unwrap() = None;
    assert_eq!(
        fixture
            .state
            .file_evidence
            .read()
            .unwrap()
            .version("raced.rs"),
        Some(&version)
    );
    assert!(
        fixture
            .state
            .file_evidence
            .read()
            .unwrap()
            .stamp("late.rs")
            .is_none()
    );
    assert!(
        !fixture
            .state
            .index
            .read()
            .unwrap()
            .has_active_path("raced.rs")
    );
    fixture.poll();
    fixture.assert_hit("after_classification_marker", "raced.rs");
    fixture.assert_hit("late_arrival_marker", "late.rs");
}

struct WatcherDropProbe {
    state: Weak<ServerState>,
    dropped: mpsc::Sender<bool>,
}

impl Drop for WatcherDropProbe {
    fn drop(&mut self) {
        let outside_registry_lock = self
            .state
            .upgrade()
            .is_some_and(|state| state.watch_registry.try_lock().is_ok());
        let _ = self.dropped.send(outside_registry_lock);
    }
}

fn install_registry(fixture: &Fixture, fail_after: Option<usize>) -> mpsc::Receiver<bool> {
    let (dropped, receive_drop) = mpsc::channel();
    let probe = WatcherDropProbe {
        state: Arc::downgrade(&fixture.state),
        dropped,
    };
    let mut watcher = notify::recommended_watcher(move |_event: notify::Result<Event>| {
        let _keep_probe_alive = &probe;
    })
    .unwrap();
    watcher
        .watch(&fixture.root, RecursiveMode::NonRecursive)
        .unwrap();
    *fixture.state.watch_registry.lock().unwrap() = Some(WatchRegistry {
        watcher,
        root: fixture.root.clone(),
        watched: HashSet::from([fixture.root.clone()]),
        budget: fixture.state.refresh.watch_budget,
        failure: None,
        fail_after,
        polling: Arc::clone(&fixture.state.refresh.polling),
    });
    fixture.state.watcher_active.store(true, Ordering::SeqCst);
    receive_drop
}

fn assert_retired(fixture: &Fixture, dropped: mpsc::Receiver<bool>, reason: &str) {
    fixture.assert_polling();
    assert!(
        dropped.recv_timeout(Duration::from_secs(5)).unwrap(),
        "notify and its callback must be destroyed outside watch_registry's mutex"
    );
    let status = fixture.state.refresh.status.lock().unwrap();
    assert!(
        status.catch_up,
        "fallback must request an immediate catch-up"
    );
    assert!(
        status.fallback_reason.as_deref().unwrap().contains(reason),
        "unexpected fallback: {:?}",
        status.fallback_reason
    );
}

#[test]
fn a_small_eligible_tree_stays_native_and_ignored_trees_cost_no_budget() {
    let fixture = Fixture::new(WatchMode::Auto, 2);
    fixture.write("src/visible.rs", "fn visible_marker() {}\n");
    fixture.write("ignored/deep/hidden.rs", "fn ignored_marker() {}\n");
    fixture.write(".gitignore", "ignored/\n");
    assert!(background_refresh_stale(
        &fixture.state,
        &fixture.root,
        &fixture.state.index_dir,
        false,
    ));
    let desired = fixture.desired();
    assert_eq!(desired.completeness, TraversalCompleteness::Complete);
    assert_eq!(
        desired.dirs,
        HashSet::from([fixture.root.clone(), fixture.root.join("src")])
    );

    let dropped = install_registry(&fixture, None);
    let added = apply_watch_registrations(&fixture.state, &desired, Instant::now());
    assert_eq!(added, vec![fixture.root.join("src")]);
    assert!(native_watching(&fixture.state));
    assert!(fixture.state.watcher_active.load(Ordering::SeqCst));
    assert_eq!(
        fixture
            .state
            .watch_registry
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .watched,
        desired.dirs
    );
    assert!(matches!(dropped.try_recv(), Err(mpsc::TryRecvError::Empty)));
    fixture.assert_files(&["src/visible.rs"]);
    fixture.assert_hit("visible_marker", "src/visible.rs");
}

#[test]
fn budget_preflight_releases_the_entire_registry_and_never_restarts_it() {
    let fixture = Fixture::new(WatchMode::Auto, 2);
    fixture.write("src/old.rs", "fn available_marker() {}\n");
    assert!(background_refresh_stale(
        &fixture.state,
        &fixture.root,
        &fixture.state.index_dir,
        false,
    ));
    let dropped = install_registry(&fixture, None);
    assert_eq!(
        apply_watch_registrations(&fixture.state, &fixture.desired(), Instant::now()).len(),
        1
    );
    fixture.write("added/new.rs", "fn catchup_marker() {}\n");
    fixture
        .state
        .watch_registry
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .fail_after = Some(0);
    assert!(
        apply_watch_registrations(&fixture.state, &fixture.desired(), Instant::now()).is_empty()
    );
    assert_retired(&fixture, dropped, "budget");
    fixture.assert_hit("available_marker", "src/old.rs");
    assert_eq!(
        fixture.tick(),
        Some(true),
        "fallback catch-up must not wait 120 seconds"
    );
    fixture.assert_hit("catchup_marker", "added/new.rs");

    for _ in 0..2 {
        assert!(
            apply_watch_registrations(&fixture.state, &fixture.desired(), Instant::now())
                .is_empty()
        );
        assert!(!start_file_watcher(
            Arc::clone(&fixture.state),
            &fixture.root,
            4
        ));
        assert!(
            sync_watch_registrations(&fixture.state, &fixture.root)
                .0
                .is_empty()
        );
        fixture.assert_polling();
    }
}

#[test]
fn capacity_failure_before_or_after_one_registration_releases_all_watches() {
    for fail_after in [0, 1] {
        let fixture = Fixture::new(WatchMode::Auto, 4);
        fixture.write("first/a.rs", "fn first_marker() {}\n");
        fixture.write("second/b.rs", "fn second_marker() {}\n");
        let dropped = install_registry(&fixture, Some(fail_after));
        assert!(
            apply_watch_registrations(&fixture.state, &fixture.desired(), Instant::now())
                .is_empty()
        );
        assert_retired(&fixture, dropped, "capacity");
        assert_eq!(fixture.tick(), Some(true));
        fixture.assert_files(&["first/a.rs", "second/b.rs"]);
        fixture.assert_hit("second_marker", "second/b.rs");
        assert!(!start_file_watcher(
            Arc::clone(&fixture.state),
            &fixture.root,
            4
        ));
        assert!(
            apply_watch_registrations(&fixture.state, &fixture.desired(), Instant::now())
                .is_empty()
        );
        fixture.assert_polling();
    }
}

#[test]
fn registration_failure_is_sticky_even_if_the_injection_is_removed() {
    let fixture = Fixture::new(WatchMode::Auto, 4);
    for dir in ["first", "second", "third"] {
        std::fs::create_dir(fixture.root.join(dir)).unwrap();
    }
    let _dropped = install_registry(&fixture, Some(1));
    let mut guard = fixture.state.watch_registry.lock().unwrap();
    let registry = guard.as_mut().unwrap();
    let first = fixture.root.join("first");
    let second = fixture.root.join("second");
    let third = fixture.root.join("third");
    assert_eq!(registry.add_all([&first, &second]), vec![first.clone()]);
    assert!(registry.failure.is_some());
    assert_eq!(
        registry.watched,
        HashSet::from([fixture.root.clone(), first])
    );
    let before = registry.watched.clone();
    registry.fail_after = None;
    assert!(registry.add_all([&third]).is_empty());
    assert!(registry.resubscribe_all([&second]).is_empty());
    assert_eq!(registry.watched, before);
}

#[test]
fn a_callback_handoff_after_the_registration_entry_check_prevents_subscriptions() {
    let fixture = Fixture::new(WatchMode::Auto, 4);
    let first = fixture.root.join("first");
    let second = fixture.root.join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let dropped = install_registry(&fixture, Some(1));
    let mut guard = fixture.state.watch_registry.lock().unwrap();
    let registry = guard.as_mut().unwrap();
    assert!(Arc::ptr_eq(
        &registry.polling,
        &fixture.state.refresh.polling
    ));
    // Collection runs after subscribe's entry check but before its per-directory
    // loop, placing the callback handoff in that window without OS-event timing.
    let desired = [&first, &second].into_iter().inspect(|_| {
        request_polling(&fixture.state, "injected callback capacity failure".into());
    });
    assert!(registry.add_all(desired).is_empty());
    assert_eq!(
        registry.fail_after,
        Some(1),
        "no native watch call should be attempted"
    );
    assert_eq!(registry.watched, HashSet::from([fixture.root.clone()]));
    assert!(registry.failure.is_none());
    drop(guard);
    assert!(
        apply_watch_registrations(&fixture.state, &fixture.desired(), Instant::now()).is_empty()
    );
    assert_retired(&fixture, dropped, "callback");
}

#[test]
fn watcher_creation_capacity_failure_requests_polling_without_a_registry() {
    let fixture = Fixture::new(WatchMode::Auto, 4);
    fixture.write("source.rs", "fn creation_fallback_marker() {}\n");
    let attempted = AtomicBool::new(false);
    assert!(!start_file_watcher_using(
        Arc::clone(&fixture.state),
        &fixture.root,
        4,
        |_callback| {
            attempted.store(true, Ordering::SeqCst);
            Err(notify::Error::new(notify::ErrorKind::MaxFilesWatch))
        },
    ));
    assert!(attempted.load(Ordering::SeqCst));
    fixture.assert_polling();
    assert!(fixture.state.refresh.status.lock().unwrap().catch_up);
    assert_eq!(fixture.tick(), Some(true));
    fixture.assert_hit("creation_fallback_marker", "source.rs");
}

#[test]
fn root_subscription_failure_drops_the_created_watcher_and_requests_catch_up() {
    let fixture = Fixture::new(WatchMode::Auto, 4);
    fixture.write("source.rs", "fn root_fallback_marker() {}\n");
    let (dropped, receive_drop) = mpsc::channel();
    let probe = WatcherDropProbe {
        state: Arc::downgrade(&fixture.state),
        dropped,
    };
    assert!(!start_file_watcher_using(
        Arc::clone(&fixture.state),
        &fixture.root.join("missing-root"),
        4,
        |mut callback| {
            notify::recommended_watcher(move |event| {
                let _keep_probe_alive = &probe;
                callback(event);
            })
        },
    ));
    assert_retired(&fixture, receive_drop, "coverage");
    assert_eq!(fixture.tick(), Some(true));
    fixture.assert_hit("root_fallback_marker", "source.rs");
}

#[test]
fn incomplete_registration_coverage_releases_watches_instead_of_claiming_native() {
    let fixture = Fixture::new(WatchMode::Auto, 4);
    fixture.write("src/source.rs", "fn incomplete_coverage_marker() {}\n");
    let dropped = install_registry(&fixture, None);
    let mut desired = fixture.desired();
    desired.completeness = TraversalCompleteness::Incomplete;
    assert!(apply_watch_registrations(&fixture.state, &desired, Instant::now()).is_empty());
    assert_retired(&fixture, dropped, "incomplete");
    assert_eq!(fixture.tick(), Some(true));
    fixture.assert_hit("incomplete_coverage_marker", "src/source.rs");
}

#[test]
fn queue_overflow_repairs_content_but_does_not_abandon_native_watching() {
    let fixture = Fixture::new(WatchMode::Auto, 4);
    fixture.write("source.rs", "fn before_overflow_marker() {}\n");
    assert!(background_refresh_stale(
        &fixture.state,
        &fixture.root,
        &fixture.state.index_dir,
        false,
    ));
    let _dropped = install_registry(&fixture, None);
    fixture.write("source.rs", "fn after_overflow_repaired_marker() {}\n");
    let overflowed = AtomicBool::new(true);
    fixture
        .state
        .watch_resubscribe
        .store(true, Ordering::SeqCst);
    recover_watcher_overflow(
        &fixture.state,
        &fixture.root,
        &fixture.state.index_dir,
        &overflowed,
        1,
    );
    assert!(!overflowed.load(Ordering::SeqCst));
    assert!(native_watching(&fixture.state));
    assert!(fixture.state.watch_registry.lock().unwrap().is_some());
    assert!(
        fixture
            .state
            .refresh
            .status
            .lock()
            .unwrap()
            .fallback_reason
            .is_none()
    );
    fixture.assert_hit("after_overflow_repaired_marker", "source.rs");
}

#[test]
fn explicit_poll_never_creates_a_watcher_or_starts_native_recovery() {
    let fixture = Fixture::new(WatchMode::Poll, 4);
    fixture.write("src/source.rs", "fn polled_marker() {}\n");
    let attempted = AtomicBool::new(false);
    assert!(!start_file_watcher_using(
        Arc::clone(&fixture.state),
        &fixture.root,
        4,
        |_callback| {
            attempted.store(true, Ordering::SeqCst);
            Err(notify::Error::new(notify::ErrorKind::MaxFilesWatch))
        },
    ));
    assert!(!attempted.load(Ordering::SeqCst));
    assert!(!start_file_watcher(
        Arc::clone(&fixture.state),
        &fixture.root,
        4
    ));
    assert_eq!(
        fixture.tick(),
        Some(true),
        "explicit poll starts with catch-up due"
    );
    fixture.assert_polling();
    fixture.assert_hit("polled_marker", "src/source.rs");

    let before = disk_snapshot(&fixture.state.index_dir);
    let evidence = fixture.state.file_evidence.read().unwrap().clone();
    let generation = fixture.state.cache_generation.load(Ordering::SeqCst);
    fixture.write("arrived/new.rs", "fn next_poll_marker() {}\n");
    watch_new_subtree(&fixture.state, &fixture.root, &fixture.root.join("arrived"));
    spawn_recovery_scan(
        &fixture.state,
        &fixture.root,
        vec![fixture.root.join("arrived")],
        SystemTime::UNIX_EPOCH,
    );
    let overflowed = AtomicBool::new(true);
    recover_watcher_overflow(
        &fixture.state,
        &fixture.root,
        &fixture.state.index_dir,
        &overflowed,
        1,
    );
    fixture
        .state
        .ignore_rules_dirty
        .store(true, Ordering::SeqCst);
    schedule_ignore_rules_refresh(Arc::clone(&fixture.state), fixture.root.clone());
    assert!(
        !fixture
            .state
            .ignore_refresh_scheduled
            .load(Ordering::SeqCst)
    );
    assert!(
        sync_watch_registrations(&fixture.state, &fixture.root)
            .0
            .is_empty()
    );
    assert!(
        apply_watch_registrations(&fixture.state, &fixture.desired(), Instant::now()).is_empty()
    );
    assert_eq!(fixture.tick(), None);
    assert_eq!(*fixture.state.file_evidence.read().unwrap(), evidence);
    assert_eq!(
        fixture.state.cache_generation.load(Ordering::SeqCst),
        generation
    );
    assert_eq!(disk_snapshot(&fixture.state.index_dir), before);
    fixture.assert_polling();
    fixture.poll();
    fixture.assert_hit("next_poll_marker", "arrived/new.rs");
}

#[test]
fn scheduled_polling_reconciles_add_modify_delete_rename_and_ignore_changes() {
    let fixture = Fixture::new(WatchMode::Poll, 4);
    fixture.write("source.rs", "fn initial_marker() {}\n");
    let deleted = fixture.write("delete.rs", "fn removed_marker() {}\n");
    let renamed = fixture.write("rename.rs", "fn renamed_marker() {}\n");
    fixture.write("ignored/hidden.rs", "fn initially_hidden_marker() {}\n");
    fixture.write(".gitignore", "ignored/\n");
    fixture.poll();
    fixture.assert_files(&["delete.rs", "rename.rs", "source.rs"]);
    fixture.assert_hit("initial_marker", "source.rs");

    fixture.write("source.rs", "fn modified_polling_marker() {}\n");
    fixture.write("added.rs", "fn added_marker() {}\n");
    fixture.write("asset.bin", [0, 1, 2, 3]);
    std::fs::remove_file(deleted).unwrap();
    std::fs::rename(renamed, fixture.root.join("renamed.rs")).unwrap();
    fixture.poll();
    fixture.assert_files(&["added.rs", "asset.bin", "renamed.rs", "source.rs"]);
    fixture.assert_hit("modified_polling_marker", "source.rs");
    fixture.assert_hit("renamed_marker", "renamed.rs");
    assert!(
        !fixture
            .state
            .index
            .read()
            .unwrap()
            .reader_has_path("delete.rs")
    );
    assert!(
        !fixture
            .state
            .index
            .read()
            .unwrap()
            .reader_has_path("rename.rs")
    );
    assert!(
        !fixture
            .state
            .file_evidence
            .read()
            .unwrap()
            .stamps
            .contains_key("delete.rs")
    );

    fixture.write(".gitignore", "source.rs\nasset.bin\n");
    fixture.poll();
    fixture.assert_files(&["added.rs", "ignored/hidden.rs", "renamed.rs"]);
    fixture.assert_hit("initially_hidden_marker", "ignored/hidden.rs");
    assert!(
        !fixture
            .state
            .index
            .read()
            .unwrap()
            .reader_has_path("source.rs")
    );
    std::fs::remove_file(fixture.root.join(".gitignore")).unwrap();
    fixture.poll();
    fixture.assert_files(&[
        "added.rs",
        "asset.bin",
        "ignored/hidden.rs",
        "renamed.rs",
        "source.rs",
    ]);
    fixture.assert_hit("modified_polling_marker", "source.rs");
}

#[test]
fn unchanged_scheduled_polls_preserve_evidence_index_bytes_mtimes_and_cache() {
    let fixture = Fixture::new(WatchMode::Poll, 4);
    fixture.write("source.rs", "fn unchanged_marker() {}\n");
    fixture.write("asset.bin", [0, 1, 2]);
    fixture.poll();
    fixture.assert_hit("unchanged_marker", "source.rs");
    let before = disk_snapshot(&fixture.state.index_dir);
    assert!(before.contains_key("filestamps.json"));
    assert!(before.contains_key("index.bin"));
    let evidence = fixture.state.file_evidence.read().unwrap().clone();
    assert_eq!(
        evidence,
        tgrep_core::meta::read_file_evidence(&fixture.state.index_dir).unwrap()
    );
    let reader = fixture.state.index.read().unwrap().reader_arc();
    let generation = fixture.state.cache_generation.load(Ordering::SeqCst);
    for _ in 0..3 {
        fixture.poll();
        assert_eq!(disk_snapshot(&fixture.state.index_dir), before);
        assert_eq!(*fixture.state.file_evidence.read().unwrap(), evidence);
        assert!(Arc::ptr_eq(
            &reader,
            &fixture.state.index.read().unwrap().reader_arc()
        ));
        assert_eq!(
            fixture.state.cache_generation.load(Ordering::SeqCst),
            generation
        );
        assert!(
            !fixture
                .state
                .index
                .read()
                .unwrap()
                .live
                .has_pending_changes()
        );
        assert_eq!(fixture.tick(), None);
    }
}

#[test]
fn indexing_and_flushing_preserve_catch_up_until_the_first_idle_tick() {
    for indexing in [true, false] {
        let fixture = Fixture::new(WatchMode::Poll, 4);
        fixture.write("source.rs", "fn deferred_marker() {}\n");
        let busy = if indexing {
            &fixture.state.indexing
        } else {
            &fixture.state.flushing
        };
        busy.store(true, Ordering::SeqCst);
        for _ in 0..2 {
            assert_eq!(fixture.tick(), None);
            let status = fixture.state.refresh.status.lock().unwrap();
            assert!(status.catch_up);
            assert!(!status.running);
            assert!(status.last_success.is_none());
        }
        busy.store(false, Ordering::SeqCst);
        assert_eq!(fixture.tick(), Some(true));
        fixture.assert_hit("deferred_marker", "source.rs");
        assert!(!fixture.state.refresh.status.lock().unwrap().catch_up);
    }
}

fn pause_first_refresh_before_lock(
    state: &Arc<ServerState>,
) -> (mpsc::Receiver<()>, mpsc::Sender<()>, Arc<AtomicUsize>) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let resume_rx = Mutex::new(resume_rx);
    let first = AtomicBool::new(true);
    let walks = Arc::new(AtomicUsize::new(0));
    let hook_walks = Arc::clone(&walks);
    *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| match phase {
        StaleRefreshPhase::BeforeRefreshLock if first.swap(false, Ordering::SeqCst) => {
            entered_tx.send(()).unwrap();
            resume_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
        StaleRefreshPhase::BeforeWalk => {
            hook_walks.fetch_add(1, Ordering::SeqCst);
        }
        _ => {}
    }));
    (entered_rx, resume_tx, walks)
}

fn assert_initial_poll_handoff(startup_waits: bool) {
    for mode in [WatchMode::Poll, WatchMode::Auto] {
        let fixture = Fixture::new(mode, 4);
        fixture.write("source.rs", "fn startup_handoff_marker() {}\n");
        if mode == WatchMode::Auto {
            request_polling(&fixture.state, "startup watch capacity failure".into());
        }
        let (entered, resume, walks) = pause_first_refresh_before_lock(&fixture.state);
        let waiting_state = Arc::clone(&fixture.state);
        let waiting = thread::spawn(move || {
            if startup_waits {
                Some(startup_refresh_stale(
                    &waiting_state,
                    &waiting_state.root,
                    &waiting_state.index_dir,
                ))
            } else {
                scheduled_reconcile(
                    &waiting_state,
                    &waiting_state.root,
                    &waiting_state.index_dir,
                )
            }
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let first_result = if startup_waits {
            fixture.tick()
        } else {
            Some(startup_refresh_stale(
                &fixture.state,
                &fixture.root,
                &fixture.state.index_dir,
            ))
        };
        let finished = fixture.state.refresh.status.lock().unwrap().finished;
        let disk = disk_snapshot(&fixture.state.index_dir);
        resume.send(()).unwrap();
        let waiting_result = waiting.join().unwrap();
        *fixture.state.stale_refresh_hook.lock().unwrap() = None;

        assert_eq!(first_result, Some(true));
        assert_eq!(
            walks.load(Ordering::SeqCst),
            1,
            "the initial poll must be shared by startup and the scheduler"
        );
        assert_eq!(
            waiting_result,
            if startup_waits { Some(true) } else { None }
        );
        let status = fixture.state.refresh.status.lock().unwrap();
        assert_eq!(status.finished, finished, "skipping must not reset cadence");
        assert!(!status.running);
        assert!(!status.catch_up);
        assert!(status.error.is_none());
        drop(status);
        assert_eq!(disk_snapshot(&fixture.state.index_dir), disk);
        fixture.assert_hit("startup_handoff_marker", "source.rs");
        assert_eq!(fixture.tick(), None);
    }
}

#[test]
fn polling_startup_skips_a_completed_scheduled_reconciliation() {
    assert_initial_poll_handoff(true);
}

#[test]
fn scheduled_poll_skips_a_completed_startup_reconciliation() {
    assert_initial_poll_handoff(false);
}

#[test]
fn scheduled_poll_rechecks_busy_state_before_claiming_catch_up() {
    for indexing in [true, false] {
        let fixture = Fixture::new(WatchMode::Poll, 4);
        fixture.write("source.rs", "fn busy_handoff_marker() {}\n");
        let (entered, resume, walks) = pause_first_refresh_before_lock(&fixture.state);
        let waiting_state = Arc::clone(&fixture.state);
        let waiting = thread::spawn(move || {
            scheduled_reconcile(
                &waiting_state,
                &waiting_state.root,
                &waiting_state.index_dir,
            )
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let busy = if indexing {
            &fixture.state.indexing
        } else {
            &fixture.state.flushing
        };
        busy.store(true, Ordering::SeqCst);
        resume.send(()).unwrap();
        let result = waiting.join().unwrap();
        *fixture.state.stale_refresh_hook.lock().unwrap() = None;
        busy.store(false, Ordering::SeqCst);

        assert_eq!(result, None);
        assert_eq!(walks.load(Ordering::SeqCst), 0);
        assert!(fixture.state.refresh.status.lock().unwrap().catch_up);
        assert_eq!(fixture.tick(), Some(true));
        fixture.assert_hit("busy_handoff_marker", "source.rs");
    }
}

#[test]
fn polling_startup_preserves_a_previous_failed_attempt_and_its_retry_cadence() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("missing");
    let index_dir = temp.path().join("index");
    let mut state = test_server_state(&root, &index_dir);
    Arc::get_mut(&mut state).unwrap().refresh =
        RefreshControl::new(WatchMode::Poll, Duration::from_secs(120), 4);
    let walks = Arc::new(AtomicUsize::new(0));
    let hook_walks = Arc::clone(&walks);
    *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::BeforeWalk) {
            hook_walks.fetch_add(1, Ordering::SeqCst);
        }
    }));
    assert_eq!(scheduled_reconcile(&state, &root, &index_dir), Some(false));
    let finished = state.refresh.status.lock().unwrap().finished;
    assert!(!startup_refresh_stale(&state, &root, &index_dir));
    assert_eq!(walks.load(Ordering::SeqCst), 1);
    let status = state.refresh.status.lock().unwrap();
    assert_eq!(status.finished, finished);
    assert!(status.last_success.is_none());
    assert!(status.error.is_some());
    drop(status);
    assert_eq!(scheduled_reconcile(&state, &root, &index_dir), None);
}

#[test]
fn polling_startup_does_not_claim_deferred_work_as_successful() {
    for indexing in [true, false] {
        for previously_reconciled in [true, false] {
            let fixture = Fixture::new(WatchMode::Poll, 4);
            if previously_reconciled {
                fixture.poll();
                fixture.due();
            }
            fixture.write("source.rs", "fn deferred_startup_marker() {}\n");
            let busy = if indexing {
                &fixture.state.indexing
            } else {
                &fixture.state.flushing
            };
            busy.store(true, Ordering::SeqCst);
            let result =
                startup_refresh_stale(&fixture.state, &fixture.root, &fixture.state.index_dir);
            busy.store(false, Ordering::SeqCst);
            assert!(!result);
            assert_eq!(fixture.tick(), Some(true));
            fixture.assert_hit("deferred_startup_marker", "source.rs");
        }
    }
}

#[test]
fn native_and_unwatched_startup_refreshes_do_not_depend_on_the_poll_schedule() {
    for (mode, watch_enabled) in [
        (WatchMode::Auto, true),
        (WatchMode::Auto, false),
        (WatchMode::Poll, false),
    ] {
        let mut fixture = Fixture::new(mode, 4);
        Arc::get_mut(&mut fixture.state).unwrap().watch_enabled = watch_enabled;
        assert!(background_refresh_stale(
            &fixture.state,
            &fixture.root,
            &fixture.state.index_dir,
            false,
        ));
        fixture.write("source.rs", "fn mandatory_startup_marker() {}\n");
        assert!(startup_refresh_stale(
            &fixture.state,
            &fixture.root,
            &fixture.state.index_dir,
        ));
        fixture.assert_hit("mandatory_startup_marker", "source.rs");
    }
}

#[test]
fn polling_startup_runs_again_when_the_completion_interval_has_elapsed() {
    let fixture = Fixture::new(WatchMode::Poll, 4);
    fixture.poll();
    fixture.write("source.rs", "fn overdue_startup_marker() {}\n");
    fixture.due();
    assert!(startup_refresh_stale(
        &fixture.state,
        &fixture.root,
        &fixture.state.index_dir,
    ));
    fixture.assert_hit("overdue_startup_marker", "source.rs");
    assert_eq!(fixture.tick(), None);
}

#[test]
fn polling_startup_retains_catch_up_requested_during_a_previous_scan() {
    let fixture = Fixture::new(WatchMode::Auto, 4);
    fixture.write("source.rs", "fn initial_native_marker() {}\n");
    let hook_state = Arc::downgrade(&fixture.state);
    let late = fixture.root.join("late.rs");
    *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::AfterMatcherPublish) {
            let state = hook_state.upgrade().unwrap();
            std::fs::write(&late, "fn startup_catch_up_marker() {}\n").unwrap();
            request_polling(&state, "capacity failure during a startup scan".into());
        }
    }));
    assert!(background_refresh_stale(
        &fixture.state,
        &fixture.root,
        &fixture.state.index_dir,
        false,
    ));
    *fixture.state.stale_refresh_hook.lock().unwrap() = None;
    assert!(fixture.state.refresh.status.lock().unwrap().catch_up);
    fixture.assert_files(&["source.rs"]);
    assert!(startup_refresh_stale(
        &fixture.state,
        &fixture.root,
        &fixture.state.index_dir,
    ));
    fixture.assert_hit("startup_catch_up_marker", "late.rs");
    assert!(!fixture.state.refresh.status.lock().unwrap().catch_up);
    assert_eq!(fixture.tick(), None);
}

#[test]
fn explicit_refreshes_do_not_depend_on_the_poll_schedule() {
    let fixture = Fixture::new(WatchMode::Poll, 4);
    fixture.poll();
    for compare_index_membership in [false, true] {
        let path = format!("forced_{compare_index_membership}.rs");
        fixture.write(&path, "fn explicit_refresh_marker() {}\n");
        assert!(background_refresh_stale(
            &fixture.state,
            &fixture.root,
            &fixture.state.index_dir,
            compare_index_membership,
        ));
        fixture.assert_hit("explicit_refresh_marker", &path);
    }
}

#[test]
fn a_fallback_requested_during_a_scan_retains_exactly_one_immediate_catch_up() {
    let fixture = Fixture::new(WatchMode::Auto, 4);
    fixture.write("source.rs", "fn original_marker() {}\n");
    let hook_state = Arc::downgrade(&fixture.state);
    let late = fixture.root.join("late.rs");
    *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::AfterMatcherPublish) {
            let state = hook_state.upgrade().unwrap();
            assert!(state.refresh.status.lock().unwrap().running);
            std::fs::write(&late, "fn catch_up_after_walk_marker() {}\n").unwrap();
            request_polling(
                &state,
                "capacity failure during native reconciliation".into(),
            );
        }
    }));
    assert!(background_refresh_stale(
        &fixture.state,
        &fixture.root,
        &fixture.state.index_dir,
        false,
    ));
    *fixture.state.stale_refresh_hook.lock().unwrap() = None;
    fixture.assert_polling();
    assert!(fixture.state.refresh.status.lock().unwrap().catch_up);
    fixture.assert_files(&["source.rs"]);
    assert_eq!(
        fixture.tick(),
        Some(true),
        "catch-up must survive scan completion"
    );
    fixture.assert_hit("catch_up_after_walk_marker", "late.rs");
    assert!(!fixture.state.refresh.status.lock().unwrap().catch_up);
    assert_eq!(
        fixture.tick(),
        None,
        "one handoff must cause only one catch-up"
    );
}

#[test]
fn sustained_searches_do_not_defer_due_polls() {
    let fixture = Fixture::new(WatchMode::Poll, 4);
    fixture.write("source.rs", "fn searchable_marker() {}\n");
    fixture.poll();
    for iteration in 0..3 {
        fixture.assert_hit("searchable_marker", "source.rs");
        fixture.state.note_search();
        assert!(fixture.state.quiet_for() < RECONCILE_QUIET_PERIOD);
        let name = format!("new{iteration}.rs");
        fixture.write(&name, format!("fn arrival_{iteration}_marker() {{}}\n"));
        fixture.poll();
        fixture.assert_hit(&format!("arrival_{iteration}_marker"), &name);
    }
}

#[test]
fn a_running_production_refresh_rejects_a_second_tick_and_keeps_search_available() {
    let fixture = Fixture::new(WatchMode::Poll, 4);
    fixture.write("source.rs", "fn searchable_marker() {}\n");
    fixture.poll();
    let walks = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let resume_rx = Mutex::new(resume_rx);
    let hook_walks = Arc::clone(&walks);
    *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::BeforeWalk)
            && hook_walks.fetch_add(1, Ordering::SeqCst) == 0
        {
            entered_tx.send(()).unwrap();
            resume_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
    }));
    fixture.due();
    let first_state = Arc::clone(&fixture.state);
    let first = thread::spawn(move || {
        scheduled_reconcile(&first_state, &first_state.root, &first_state.index_dir)
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(fixture.state.refresh.status.lock().unwrap().running);
    fixture.assert_hit("searchable_marker", "source.rs");
    let (second_tx, second_rx) = mpsc::channel();
    let second_state = Arc::clone(&fixture.state);
    let second = thread::spawn(move || {
        second_tx
            .send(scheduled_reconcile(
                &second_state,
                &second_state.root,
                &second_state.index_dir,
            ))
            .unwrap();
    });
    let second_result = second_rx.recv_timeout(Duration::from_secs(2));
    resume_tx.send(()).unwrap();
    let first_result = first.join().unwrap();
    second.join().unwrap();
    *fixture.state.stale_refresh_hook.lock().unwrap() = None;
    assert_eq!(
        second_result.unwrap(),
        None,
        "a second tick must not queue another walk"
    );
    assert_eq!(first_result, Some(true));
    assert_eq!(walks.load(Ordering::SeqCst), 1);
    assert!(!fixture.state.refresh.status.lock().unwrap().running);
}

#[test]
fn a_scan_longer_than_the_interval_schedules_from_completion_not_start() {
    let interval = Duration::from_secs(1);
    let fixture = Fixture::with_interval(WatchMode::Poll, 4, interval);
    fixture.write("source.rs", "fn slow_scan_marker() {}\n");
    *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::BeforeWalk) {
            thread::sleep(interval + Duration::from_millis(20));
        }
    }));
    assert_eq!(fixture.tick(), Some(true));
    *fixture.state.stale_refresh_hook.lock().unwrap() = None;
    let status = fixture.state.refresh.status.lock().unwrap();
    assert!(status.duration_ms.unwrap() >= interval.as_millis() as u64);
    assert!(status.last_success.is_some());
    assert!(!status.catch_up);
    assert!(!status.running);
    assert!(status.error.is_none());
    drop(status);
    assert_eq!(
        fixture.tick(),
        None,
        "a long scan must not trigger a catch-up storm"
    );
    fixture.poll();
}

#[test]
fn a_failed_poll_preserves_last_success_and_retries_an_unchanged_failed_stamp() {
    let fixture = Fixture::new(WatchMode::Poll, 4);
    let path = fixture.write("source.rs", "fn original_marker() {}\n");
    fixture.poll();
    let last_success = fixture.state.refresh.status.lock().unwrap().last_success;
    fixture.write("source.rs", "fn recovered_after_failure_marker() {}\n");
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let failed_version = builder::file_version(&std::fs::metadata(&path).unwrap());
    let failed_stamp = failed_version.stamp().clone();
    let hook_path = path.clone();
    *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::AfterMatcherPublish) {
            std::fs::remove_file(&hook_path).unwrap();
        }
    }));
    fixture.due();
    assert_eq!(fixture.tick(), Some(false));
    *fixture.state.stale_refresh_hook.lock().unwrap() = None;
    assert!(
        fixture
            .state
            .index
            .read()
            .unwrap()
            .reader_has_path("source.rs")
    );
    assert_eq!(
        fixture.state.unreadable.read().unwrap().get("source.rs"),
        Some(&Some(failed_version))
    );
    let status = fixture.state.refresh.status.lock().unwrap();
    assert_eq!(status.last_success, last_success);
    assert!(status.error.is_some());
    assert!(status.duration_ms.is_some());
    assert!(!status.running);
    drop(status);
    assert_eq!(
        fixture.tick(),
        None,
        "failures also obey the completion-based retry interval"
    );

    fixture.write("source.rs", "fn recovered_after_failure_marker() {}\n");
    set_modified(&path, modified);
    assert_eq!(
        tgrep_core::meta::file_stamp(&std::fs::metadata(&path).unwrap()),
        failed_stamp
    );
    fixture.poll();
    assert!(fixture.state.unreadable.read().unwrap().is_empty());
    assert!(fixture.state.refresh.status.lock().unwrap().error.is_none());
    fixture.assert_hit("recovered_after_failure_marker", "source.rs");
}

fn assert_native_retry_after_failed_read(restore_mtime: bool) {
    for previously_indexed in [true, false] {
        let fixture = Fixture::new(WatchMode::Auto, 4);
        let modified = SystemTime::UNIX_EPOCH
            + Duration::from_secs(1_700_000_000)
            + Duration::from_millis(100);
        if previously_indexed {
            let path = fixture.write("source.rs", "fn old_memo_marker() {}\n");
            set_modified(&path, modified - Duration::from_millis(100));
            fixture.native_reconcile();
            fixture.assert_hit("old_memo_marker", "source.rs");
        }
        let path = fixture.write("source.rs", "fn bad_memo_marker() {}\n");
        set_modified(&path, modified);
        let failed_version = builder::file_version(&std::fs::metadata(&path).unwrap());
        let hook_path = path.clone();
        *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::AfterMatcherPublish) {
                std::fs::remove_file(&hook_path).unwrap();
            }
        }));
        fixture.native_reconcile();
        *fixture.state.stale_refresh_hook.lock().unwrap() = None;
        assert_eq!(
            fixture.state.unreadable.read().unwrap().get("source.rs"),
            Some(&Some(failed_version.clone())),
            "failure evidence must come from the scan, not the old indexed read or a later stat"
        );
        assert!(fixture.state.refresh.status.lock().unwrap().error.is_some());
        assert_eq!(
            fixture
                .state
                .index
                .read()
                .unwrap()
                .reader_has_path("source.rs"),
            previously_indexed
        );

        fixture.write("source.rs", "fn new_memo_marker() {}\n");
        set_modified(
            &path,
            if restore_mtime {
                modified
            } else {
                modified + Duration::from_millis(100)
            },
        );
        #[cfg(windows)]
        if restore_mtime {
            use std::os::windows::fs::FileTimesExt;
            // NTFS can reuse a deleted name's creation time. Give the replacement
            // distinct creation evidence while retaining the failed file's mtime.
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_created(modified))
                .unwrap();
        }
        let recovered_version = builder::file_version(&std::fs::metadata(&path).unwrap());
        assert_eq!(failed_version.stamp(), recovered_version.stamp());
        assert_ne!(failed_version, recovered_version);
        fixture.native_reconcile();
        fixture.assert_hit("new_memo_marker", "source.rs");
        assert!(fixture.state.unreadable.read().unwrap().is_empty());
        assert!(fixture.state.refresh.status.lock().unwrap().error.is_none());
        assert_eq!(
            fixture
                .state
                .file_evidence
                .read()
                .unwrap()
                .version("source.rs"),
            Some(&recovered_version)
        );
        let disk = disk_snapshot(&fixture.state.index_dir);
        fixture.native_reconcile();
        assert_eq!(disk_snapshot(&fixture.state.index_dir), disk);
    }
}

#[test]
fn native_reconciliation_retries_failed_files_after_same_second_writes() {
    assert_native_retry_after_failed_read(false);
}

#[test]
fn native_reconciliation_retries_failed_replacements_with_restored_mtime() {
    assert_native_retry_after_failed_read(true);
}

#[test]
fn memoized_read_failures_with_missing_versions_remain_retryable() {
    let fixture = Fixture::new(WatchMode::Auto, 4);
    let path = fixture.write("source.rs", "fn unknown_version_marker() {}\n");
    let version = builder::file_version(&std::fs::metadata(&path).unwrap());
    for (memo_known, scan_known) in [(true, false), (false, true), (false, false)] {
        let memo = std::collections::HashMap::from([(
            "source.rs".to_string(),
            memo_known.then_some(version.clone()),
        )]);
        let current = [tgrep_core::walker::FileMeta {
            relative_path: "source.rs".into(),
            mtime: version.stamp().mtime,
            size: version.stamp().size,
            version: scan_known.then_some(version.clone()),
        }];
        for was_indexed in [true, false] {
            let mut changed = Vec::new();
            let mut added = Vec::new();
            if was_indexed {
                changed.push("source.rs".to_string());
            } else {
                added.push("source.rs".to_string());
            }
            assert!(drop_memoized_failures(&memo, &current, &mut changed, &mut added).is_empty());
            assert_eq!(changed.len() + added.len(), 1);
        }
    }
}

#[test]
fn auto_save_clears_a_recovered_unreadable_memo_entry() {
    let fixture = Fixture::new(WatchMode::Auto, 4);
    let path = fixture.write("source.rs", "fn before_auto_save_marker() {}\n");
    {
        let _gate = fixture.state.snapshot_gate.read().unwrap();
        reindex_file(&fixture.state, &path, "source.rs", true);
    }
    std::fs::remove_file(&path).unwrap();
    assert!(persist_pending_index_changes(&fixture.state));
    assert_eq!(
        fixture.state.unreadable.read().unwrap().get("source.rs"),
        Some(&None),
        "an auto-save has no preceding scan and must leave the failure retryable"
    );

    fixture.write("source.rs", "fn repaired_auto_save_marker() {}\n");
    {
        let _gate = fixture.state.snapshot_gate.read().unwrap();
        reindex_file(&fixture.state, &path, "source.rs", true);
    }
    assert!(persist_pending_index_changes(&fixture.state));
    assert!(fixture.state.unreadable.read().unwrap().is_empty());
    fixture.assert_hit("repaired_auto_save_marker", "source.rs");
}

#[test]
fn a_change_after_the_metadata_walk_is_reconciled_on_the_next_poll() {
    let fixture = Fixture::new(WatchMode::Poll, 4);
    let path = fixture.write("source.rs", "fn scanned_marker() {}\n");
    fixture.poll();
    let prior_evidence = fixture.state.file_evidence.read().unwrap().clone();
    let hook_root = fixture.root.clone();
    *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::AfterMatcherPublish) {
            std::fs::write(&path, "fn changed_after_walk_marker() {}\n").unwrap();
            std::fs::write(hook_root.join("late.rs"), "fn late_arrival_marker() {}\n").unwrap();
        }
    }));
    fixture.poll();
    *fixture.state.stale_refresh_hook.lock().unwrap() = None;
    assert_eq!(*fixture.state.file_evidence.read().unwrap(), prior_evidence);
    fixture.poll();
    fixture.assert_hit("changed_after_walk_marker", "source.rs");
    fixture.assert_hit("late_arrival_marker", "late.rs");
    fixture.assert_files(&["late.rs", "source.rs"]);
}

#[test]
fn poll_build_paths_repair_post_extraction_changes_without_native_watches() {
    enum Build {
        Bootstrap,
        Reload,
        Resume,
    }
    for build in [Build::Bootstrap, Build::Reload, Build::Resume] {
        let fixture = Fixture::new(WatchMode::Poll, 4);
        if !matches!(build, Build::Bootstrap) {
            fixture.write("seeded.rs", "fn seeded_marker() {}\n");
            fixture.poll();
        }
        let path = fixture.write("source.rs", "fn before_build_marker() {}\n");
        let writes = Arc::new(AtomicUsize::new(0));
        let hook_writes = Arc::clone(&writes);
        *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::AfterBuildBeforeStampPublish)
                && hook_writes.fetch_add(1, Ordering::SeqCst) == 0
            {
                std::fs::write(&path, "fn after_build_final_marker() {}\n").unwrap();
            }
        }));
        match build {
            Build::Bootstrap => {
                fixture.state.indexing.store(true, Ordering::SeqCst);
                assert!(bootstrap_index_build(
                    &fixture.state,
                    &fixture.root,
                    &fixture.state.index_dir
                ));
            }
            Build::Reload => {
                let response = handle_reload(None, &fixture.state);
                let value: serde_json::Value = serde_json::from_str(&response).unwrap();
                assert_eq!(value["result"]["status"], "reloaded", "{response}");
            }
            Build::Resume => {
                fixture.state.indexing.store(true, Ordering::SeqCst);
                background_index_build(&fixture.state, &fixture.root, &fixture.state.index_dir);
            }
        }
        *fixture.state.stale_refresh_hook.lock().unwrap() = None;
        assert!(writes.load(Ordering::SeqCst) >= 1);
        fixture.assert_polling();
        assert!(!fixture.state.indexing.load(Ordering::SeqCst));
        assert!(!fixture.state.gitignore_pending.load(Ordering::SeqCst));
        fixture.poll();
        fixture.assert_hit("after_build_final_marker", "source.rs");
    }
}

fn set_modified(path: &Path, modified: SystemTime) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
}

#[test]
fn legacy_evidence_without_metadata_versions_fails_open_and_converges() {
    let fixture = Fixture::new(WatchMode::Poll, 4);
    let path = fixture.write("source.rs", "fn old_legacy_marker() {}\n");
    fixture.poll();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let legacy = tgrep_core::meta::FileEvidence::from_stamps(
        fixture.state.file_evidence.read().unwrap().stamps.clone(),
    );
    tgrep_core::meta::write_file_evidence(&legacy, &fixture.state.index_dir).unwrap();
    *fixture.state.file_evidence.write().unwrap() = legacy.clone();
    fixture.write("source.rs", "fn new_legacy_marker() {}\n");
    set_modified(&path, modified);
    assert_eq!(
        tgrep_core::meta::file_stamp(&std::fs::metadata(&path).unwrap()),
        legacy.stamps["source.rs"]
    );
    fixture.poll();
    fixture.assert_hit("new_legacy_marker", "source.rs");
    let repaired = disk_snapshot(&fixture.state.index_dir);
    fixture.poll();
    assert_eq!(disk_snapshot(&fixture.state.index_dir), repaired);
}

#[cfg(unix)]
#[test]
fn equal_size_writes_with_restored_mtime_and_same_second_changes_are_polled() {
    for preserve_mtime in [true, false] {
        let fixture = Fixture::new(WatchMode::Poll, 4);
        let path = fixture.write("source.rs", "fn old_precise_marker() {}\n");
        let second = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let original_time = second + Duration::from_millis(100);
        set_modified(&path, original_time);
        fixture.poll();
        fixture.assert_hit("old_precise_marker", "source.rs");
        let old = fixture.state.file_evidence.read().unwrap().stamps["source.rs"].clone();
        fixture.write("source.rs", "fn new_precise_marker() {}\n");
        let new_time = if preserve_mtime {
            original_time
        } else {
            second + Duration::from_millis(900)
        };
        set_modified(&path, new_time);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            new_time
        );
        assert_eq!(
            tgrep_core::meta::file_stamp(&std::fs::metadata(&path).unwrap()),
            old
        );
        fixture.poll();
        fixture.assert_hit("new_precise_marker", "source.rs");
        let settled = disk_snapshot(&fixture.state.index_dir);
        fixture.poll();
        assert_eq!(disk_snapshot(&fixture.state.index_dir), settled);
    }
}

#[test]
#[ignore = "bounded local polling measurement; run with --ignored --nocapture"]
fn measure_polling_1000_files_and_20_file_change_batch() {
    let fixture = Fixture::new(WatchMode::Poll, 4);
    for file in 0..1000 {
        fixture.write(
            &format!("file{file:04}.rs"),
            format!("fn fixture_{file:04}_marker() {{}}\n"),
        );
    }
    fixture.poll();
    let before = disk_snapshot(&fixture.state.index_dir);
    let no_change_start = Instant::now();
    fixture.poll();
    let no_change = no_change_start.elapsed();
    assert_eq!(disk_snapshot(&fixture.state.index_dir), before);
    for file in 0..20 {
        fixture.write(
            &format!("file{file:04}.rs"),
            format!("fn changed_{file:04}_batch_marker() {{}}\n"),
        );
    }
    let changed_start = Instant::now();
    fixture.poll();
    let changed = changed_start.elapsed();
    fixture.assert_hit("changed_0019_batch_marker", "file0019.rs");
    assert_eq!(fixture.state.index.read().unwrap().num_files(), 1000);
    eprintln!("polling fixture: 1000 files; no-change {no_change:?}; 20-file batch {changed:?}");
}
