use super::*;
use std::sync::atomic::AtomicUsize;
use tempfile::TempDir;

const OLD: &[u8] = b"fn old_recovery_marker() {}\n";
const NEW: &[u8] = b"fn new_recovery_marker() {}\n";
const LATEST: &[u8] = b"fn end_recovery_marker() {}\n";
const REL_PATH: &str = "source.rs";

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    path: PathBuf,
    state: Arc<ServerState>,
    modified: SystemTime,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let index_dir = root.join(".tgrep");
        let fixture = Self {
            path: root.join(REL_PATH),
            state: tests::test_server_state(&root, &index_dir),
            _dir: dir,
            root,
            modified: SystemTime::UNIX_EPOCH
                + Duration::from_secs(1_700_000_000)
                + Duration::from_millis(100),
        };
        fixture.write(OLD, fixture.modified);
        let outcome = builder::build_index_for_files(
            &fixture.root,
            &index_dir,
            std::slice::from_ref(&fixture.path),
            1024,
        )
        .unwrap();
        let version = outcome.versions[REL_PATH].clone();
        let mut evidence = tgrep_core::meta::FileEvidence::default();
        evidence.insert_verified(
            REL_PATH.into(),
            version.stamp().clone(),
            Some(outcome.content_ids[REL_PATH]),
            Some(version),
        );
        tgrep_core::meta::write_file_evidence(&evidence, &index_dir).unwrap();
        *fixture.state.index.write().unwrap() =
            HybridIndex::open(&index_dir, &fixture.root).unwrap();
        *fixture.state.file_evidence.write().unwrap() = evidence;
        fixture
            .state
            .gitignore_pending
            .store(false, Ordering::SeqCst);
        fixture.assert_matches("old_recovery_marker", 1);
        fixture
    }

    fn write(&self, bytes: &[u8], modified: SystemTime) {
        std::fs::write(&self.path, bytes).unwrap();
        File::options()
            .write(true)
            .open(&self.path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }

    fn recover(&self) {
        let _gate = self.state.snapshot_gate.read().unwrap();
        reindex_files_in(
            &self.state,
            &self.root,
            std::slice::from_ref(&self.root),
            SystemTime::now(),
        );
    }

    fn assert_matches(&self, pattern: &str, expected: u64) {
        let response = handle_search(None, &serde_json::json!({"pattern": pattern}), &self.state);
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["result"]["num_matches"], expected, "{response}");
    }

    fn assert_repaired(&self) {
        self.recover();
        self.assert_matches("new_recovery_marker", 1);
        self.assert_matches("old_recovery_marker", 0);
        let version = builder::file_version(&std::fs::metadata(&self.path).unwrap());
        assert_eq!(
            self.state.file_evidence.read().unwrap().version(REL_PATH),
            Some(&version),
            "recovery must retain the version validated around its indexing read"
        );
    }
}

#[cfg(unix)]
#[test]
fn subscription_recovery_detects_equal_size_writes_with_restored_mtime() {
    let fixture = Fixture::new();
    let before = fixture.state.file_evidence.read().unwrap().clone();
    fixture.write(NEW, fixture.modified);
    let version = builder::file_version(&std::fs::metadata(&fixture.path).unwrap());
    assert_eq!(before.stamp(REL_PATH), Some(version.stamp()));
    assert_ne!(before.version(REL_PATH), Some(&version));
    fixture.assert_repaired();
}

#[test]
fn subscription_recovery_detects_equal_size_same_second_writes() {
    let fixture = Fixture::new();
    let before = fixture.state.file_evidence.read().unwrap().clone();
    fixture.write(NEW, fixture.modified + Duration::from_millis(100));
    let version = builder::file_version(&std::fs::metadata(&fixture.path).unwrap());
    assert_eq!(before.stamp(REL_PATH), Some(version.stamp()));
    assert_ne!(before.version(REL_PATH), Some(&version));
    fixture.assert_repaired();
}

#[test]
fn subscription_recovery_verifies_legacy_evidence_once_without_overlay_churn() {
    let fixture = Fixture::new();
    fixture
        .state
        .file_evidence
        .write()
        .unwrap()
        .versions
        .remove(REL_PATH);
    let reads = Arc::new(AtomicUsize::new(0));
    let hook_reads = Arc::clone(&reads);
    *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::AfterConcreteRead) {
            hook_reads.fetch_add(1, Ordering::SeqCst);
        }
    }));

    fixture.recover();
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert!(
        fixture
            .state
            .file_evidence
            .read()
            .unwrap()
            .version(REL_PATH)
            .is_some()
    );
    fixture.recover();
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    let index = fixture.state.index.read().unwrap();
    assert!(!index.live.has_path(REL_PATH));
    assert_eq!(index.live.dirty_count(), 0);
}

#[test]
fn subscription_recovery_keeps_unchanged_versions_on_the_metadata_fast_path() {
    let fixture = Fixture::new();
    let before = fixture.state.file_evidence.read().unwrap().clone();
    let generation = fixture.state.cache_generation.load(Ordering::SeqCst);
    *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(|phase| {
        if matches!(phase, StaleRefreshPhase::AfterConcreteRead) {
            panic!("unchanged recovery must not re-read and verify file contents");
        }
    }));
    fixture.recover();
    assert_eq!(*fixture.state.file_evidence.read().unwrap(), before);
    assert_eq!(
        fixture.state.cache_generation.load(Ordering::SeqCst),
        generation
    );
    assert_eq!(fixture.state.index.read().unwrap().live.dirty_count(), 0);
}

#[test]
fn subscription_recovery_rejects_changes_after_read_or_before_commit() {
    for change_after_read in [true, false] {
        let fixture = Fixture::new();
        let modified = fixture.modified + Duration::from_secs(2);
        fixture.write(NEW, modified);
        // Coalesce the retry instead of starting a refresh that races these assertions.
        fixture
            .state
            .ignore_refresh_scheduled
            .store(true, Ordering::SeqCst);
        let path = fixture.path.clone();
        *fixture.state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if (change_after_read && matches!(phase, StaleRefreshPhase::AfterConcreteRead))
                || (!change_after_read && matches!(phase, StaleRefreshPhase::BeforeConcreteCommit))
            {
                std::fs::write(&path, LATEST).unwrap();
                File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(modified))
                    .unwrap();
            }
        }));

        fixture.recover();
        assert_eq!(
            fixture.state.index.read().unwrap().live.dirty_count(),
            0,
            "recovery must not publish bytes invalidated during verification"
        );
        assert!(fixture.state.ignore_rules_dirty.load(Ordering::SeqCst));
        let evidence = fixture.state.file_evidence.read().unwrap();
        assert_eq!(
            evidence.stamp(REL_PATH),
            Some(&tgrep_core::meta::FileStamp {
                mtime: u64::MAX,
                size: u64::MAX,
            })
        );
        assert!(evidence.version(REL_PATH).is_none());
        drop(evidence);

        *fixture.state.stale_refresh_hook.lock().unwrap() = None;
        fixture.recover();
        fixture.assert_matches("end_recovery_marker", 1);
        fixture.assert_matches("new_recovery_marker", 0);
    }
}
