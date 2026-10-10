// Copyright (c) Microsoft Corporation. All rights reserved.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;
use tgrep_core::generations::{IndexingProfile, Repository, RetentionPolicy};
use tgrep_core::managed::{
    ErrorCategory, Namespace, OperationToken, OwnerGuard, Policy, Token, WorkPermit, WorkRequest,
};

fn git(root: &Path, arguments: &[&str]) -> String {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    let output = command
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Managed fixture",
            "-c",
            "user.email=managed@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
        ])
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn policy() -> Policy {
    serde_json::from_value(serde_json::json!({
        "schema":2, "storage":"managed",
        "retention":{"mode":"retain-all"}, "advancement":{"mode":"fixed"},
        "work":{
            "max_views":8, "max_leases":32, "workers":2, "queue_items":16,
            "staging_bytes":67108864, "private_work_bytes":268435456,
            "sort_buffer_bytes":1048576, "blob_bytes":1048576,
            "operation_timeout_ms":30000, "page_objects":16, "max_cursors":8,
            "cursor_lifetime_ms":30000, "max_receipts":1024, "metadata_bytes":16777216
        },
        "collection":{
            "schedule":{"mode":"disabled"}, "on_pressure":false,
            "checkpoint_grace_ms":0, "generation_grace_ms":0,
            "max_duration_ms":1000, "max_examined":64, "max_removed":16,
            "max_delete_bytes":1048576, "chunk_bytes":65536, "max_pages":4, "retry_ms":10
        }
    }))
    .unwrap()
}

struct Fixture {
    namespace: Arc<Namespace>,
    owner: OwnerGuard,
    repository: Repository,
    root: PathBuf,
    temp: TempDir,
    sequence: u64,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repository");
        let storage = temp.path().join("storage");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&storage).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(root.join("source.txt"), b"committed managed needle\n").unwrap();
        git(&root, &["add", "--all"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        let repository = Repository::discover(&root).unwrap();
        let namespace = Namespace::initialize(&repository, &storage, policy()).unwrap();
        let prepared = namespace.prepare_owner().unwrap();
        let owner = OwnerGuard::claim(prepared.claim).unwrap();
        namespace.register_owner(owner.registration()).unwrap();
        Self {
            namespace,
            owner,
            repository,
            root,
            temp,
            sequence: 0,
        }
    }

    fn permit(&mut self, staging_bytes: u64) -> Arc<WorkPermit> {
        self.sequence += 1;
        let operation = self
            .namespace
            .accept_operation(
                OperationToken {
                    scope: self.owner.registration().owner.clone(),
                    sequence: self.sequence,
                    token: Token::parse(format!("operation-{}", self.sequence)).unwrap(),
                },
                "ensure",
                serde_json::json!({"sequence":self.sequence}),
            )
            .unwrap();
        self.namespace
            .reserve(
                &operation.id,
                Some(&self.owner.registration().owner),
                WorkRequest {
                    allocation_version: self.namespace.allocation().unwrap().version,
                    staging_bytes,
                    private_bytes: 128 * 1024 * 1024,
                    slots: 1,
                },
            )
            .unwrap()
    }

    fn token(&mut self, label: &str) -> OperationToken {
        self.sequence += 1;
        OperationToken {
            scope: self.owner.registration().owner.clone(),
            sequence: self.sequence,
            token: Token::parse(format!("{label}-{}", self.sequence)).unwrap(),
        }
    }

    fn manager(&self) -> tgrep_core::managed::ViewManager {
        tgrep_core::managed::ViewManager::new(
            Arc::clone(&self.namespace),
            None,
            tgrep_core::worktrees::WorktreeOptions::default(),
            None,
        )
        .unwrap()
    }

    fn attach(
        &mut self,
        manager: &tgrep_core::managed::ViewManager,
    ) -> tgrep_core::managed::ViewRecord {
        let operation = manager
            .accept_attach(
                self.token("attach"),
                tgrep_core::managed::AttachRequest {
                    root: self.root.clone(),
                    revision: Some("HEAD".into()),
                    profile: IndexingProfile::default(),
                    lease: Token::parse("primary-lease").unwrap(),
                    owner: self.owner.registration().owner.clone(),
                    accept_current: None,
                    migratable: true,
                    allocation_version: self.namespace.allocation().unwrap().version,
                },
            )
            .unwrap();
        let completed = manager.execute(&operation.id).unwrap();
        assert_eq!(
            completed.state,
            tgrep_core::managed::OperationState::Completed,
            "{completed:?}"
        );
        serde_json::from_value(completed.result.unwrap()["current"].clone()).unwrap()
    }
}

#[test]
fn superseded_reconciliation_preserves_ready_queries_and_exact_checkpoint() {
    use tgrep_core::managed::{OperationState, ReconcileRequest, RefreshRequest};
    let mut fixture = Fixture::new();
    fixture.namespace.activate().unwrap();
    let manager = fixture.manager();
    let first = fixture.attach(&manager);
    let slot = manager.slot(&first.id).unwrap();
    slot.invalidate(&[], true).unwrap();
    let token = Token::parse("queued-before-explicit-refresh").unwrap();
    let request = ReconcileRequest {
        view: first.id.clone(),
        expected_version: first.version,
        allocation_version: 1,
    };
    let queued = manager
        .accept_reconciliation(token.clone(), request.clone())
        .unwrap();
    let refresh = manager
        .accept_refresh(
            fixture.token("explicit-refresh"),
            RefreshRequest {
                view: first.id.clone(),
                expected_version: first.version,
                owner: fixture.owner.registration().owner.clone(),
                allocation_version: 1,
            },
        )
        .unwrap();
    let refreshed = manager.execute(&refresh.id).unwrap();
    assert_eq!(refreshed.state, OperationState::Completed, "{refreshed:?}");
    let before = manager.recover(&first.id).unwrap();
    let query = slot.query(before.version).unwrap();
    let completed = manager.execute(&queued.id).unwrap();
    assert_eq!(completed.state, OperationState::Completed, "{completed:?}");
    assert_eq!(
        query
            .with_snapshot(|snapshot| snapshot.files("", false))
            .unwrap(),
        ["source.txt"]
    );
    query.complete().unwrap();
    let current = manager.recover(&first.id).unwrap();
    assert_eq!(current.current, before.current);
    assert_eq!(current.checkpoint, before.checkpoint);
    assert_eq!(current.reconciled_epoch, before.reconciled_epoch);
    assert_eq!(current.input_epoch, before.input_epoch);
    let result = completed.result.as_ref().unwrap();
    assert_eq!(result["coalesced"], true);
    assert_eq!(result["reconcile"]["files_read"], 0);
    assert_eq!(result["reconcile"]["bytes_read"], 0);
    assert_eq!(result["reconcile"]["files_extracted"], 0);
    let replay = || {
        let accepted = manager
            .accept_reconciliation(token.clone(), request.clone())
            .unwrap();
        assert_eq!(
            serde_json::to_value(&accepted).unwrap(),
            serde_json::to_value(&completed).unwrap()
        );
        assert_eq!(
            serde_json::to_value(manager.execute(&accepted.id).unwrap()).unwrap(),
            serde_json::to_value(&completed).unwrap()
        );
    };
    replay();

    let newer = manager
        .accept_reconciliation(
            Token::parse("queued-before-new-invalidation").unwrap(),
            ReconcileRequest {
                view: first.id.clone(),
                expected_version: first.version,
                allocation_version: 1,
            },
        )
        .unwrap();
    fs::write(
        fixture.root.join("source.txt"),
        "newly invalidated needle\n",
    )
    .unwrap();
    let input = slot.invalidate(&[], true).unwrap();
    replay();
    assert!(!manager.status(&first.id).unwrap().ready);
    let updated = manager.execute(&newer.id).unwrap();
    assert_eq!(updated.state, OperationState::Completed, "{updated:?}");
    assert!(updated.result.as_ref().unwrap().get("coalesced").is_none());
    assert!(
        updated.result.as_ref().unwrap()["reconcile"]["bytes_read"]
            .as_u64()
            .unwrap()
            > 0
    );
    let current = manager.recover(&first.id).unwrap();
    assert_eq!(current.current, before.current);
    assert_eq!(current.input_epoch, input);
    assert_ne!(current.checkpoint, before.checkpoint);
    assert!(manager.status(&first.id).unwrap().ready);
    let query = slot.query(first.version).unwrap();
    let mut candidate = query
        .with_snapshot(|snapshot| snapshot.open_candidate("source.txt"))
        .unwrap()
        .unwrap();
    let mut bytes = String::new();
    std::io::Read::read_to_string(&mut candidate, &mut bytes).unwrap();
    assert_eq!(bytes, "newly invalidated needle\n");
    query.complete().unwrap();
    replay();
}

#[test]
fn ready_reconciliation_preserves_forced_repairs_and_rejects_invalid_work() {
    use tgrep_core::managed::{CommitState, OperationState, ReconcileRequest, RefreshRequest};
    let mut fixture = Fixture::new();
    fixture.namespace.activate().unwrap();
    let manager = fixture.manager();
    let first = fixture.attach(&manager);
    let slot = manager.slot(&first.id).unwrap();
    let input = slot.input_epoch();
    fs::write(
        fixture.root.join("source.txt"),
        "missed notification needle\n",
    )
    .unwrap();
    let refresh = manager
        .accept_refresh(
            fixture.token("forced-repair"),
            RefreshRequest {
                view: first.id.clone(),
                expected_version: first.version,
                owner: fixture.owner.registration().owner.clone(),
                allocation_version: 1,
            },
        )
        .unwrap();
    let repaired = manager.execute(&refresh.id).unwrap();
    assert_eq!(repaired.state, OperationState::Completed, "{repaired:?}");
    assert_eq!(slot.input_epoch(), input);
    let stats = &repaired.result.as_ref().unwrap()["reconcile"];
    assert_eq!(stats["full"], true);
    assert!(stats["bytes_read"].as_u64().unwrap() > 0);
    assert!(repaired.result.as_ref().unwrap().get("coalesced").is_none());
    assert_ne!(
        manager.recover(&first.id).unwrap().checkpoint,
        first.checkpoint
    );

    for reason in ["periodic-repair", "watcher-uncertainty"] {
        let name = format!("{reason}.txt");
        fs::write(fixture.root.join(&name), "missed untracked needle\n").unwrap();
        let input = slot.invalidate(&[], true).unwrap();
        let operation = manager
            .accept_reconciliation(
                Token::parse(reason).unwrap(),
                ReconcileRequest {
                    view: first.id.clone(),
                    expected_version: first.version,
                    allocation_version: 1,
                },
            )
            .unwrap();
        let completed = manager.execute(&operation.id).unwrap();
        assert_eq!(completed.state, OperationState::Completed, "{completed:?}");
        let result = completed.result.as_ref().unwrap();
        assert!(result.get("coalesced").is_none());
        assert_eq!(result["reconcile"]["full"], true);
        assert!(result["reconcile"]["bytes_read"].as_u64().unwrap() > 0);
        assert_eq!(manager.recover(&first.id).unwrap().input_epoch, input);
        let query = slot.query(first.version).unwrap();
        assert!(
            query
                .with_snapshot(|snapshot| snapshot.files("", false))
                .unwrap()
                .contains(&name)
        );
        query.complete().unwrap();
    }

    let before = manager.recover(&first.id).unwrap();
    for (name, version, allocation, cancelled, category) in [
        ("stale-view", 2, 1, false, ErrorCategory::StaleVersion),
        ("stale-allocation", 1, 2, false, ErrorCategory::StaleVersion),
        ("cancelled", 1, 1, true, ErrorCategory::Busy),
    ] {
        let operation = manager
            .accept_reconciliation(
                Token::parse(name).unwrap(),
                ReconcileRequest {
                    view: first.id.clone(),
                    expected_version: version,
                    allocation_version: allocation,
                },
            )
            .unwrap();
        if cancelled {
            fixture.namespace.cancel_operation(&operation.id).unwrap();
        }
        let failed = manager.execute(&operation.id).unwrap();
        assert_eq!(failed.state, OperationState::Failed, "{failed:?}");
        assert_eq!(failed.cancelled, cancelled);
        assert_eq!(failed.committed_state, CommitState::NotCommitted);
        assert_eq!(
            failed.error.as_ref().unwrap()["category"],
            serde_json::to_value(category).unwrap()
        );
        if cancelled {
            assert_eq!(
                failed.error.as_ref().unwrap()["reason_code"],
                "operation-is-not-admissible"
            );
        }
        assert!(failed.result.is_none());
        let current = manager.recover(&first.id).unwrap();
        assert_eq!(current.current, before.current);
        assert_eq!(current.checkpoint, before.checkpoint);
        assert_eq!(current.input_epoch, before.input_epoch);
        assert!(manager.status(&first.id).unwrap().ready);
        assert_eq!(fixture.namespace.work_usage().unwrap().reservations, 0);
    }
}

#[cfg(feature = "managed-test-hooks")]
#[test]
fn coalesced_reconciliation_rechecks_cancellation_and_allocation_before_receipt() {
    use std::time::{Duration, Instant};
    use tgrep_core::managed::faults::{Action, Point, Specification, Stage};
    use tgrep_core::managed::{CommitState, OperationState, ReconcileRequest};

    for cancelled in [true, false] {
        let mut fixture = Fixture::new();
        fixture.namespace.activate().unwrap();
        let manager = fixture.manager();
        let first = fixture.attach(&manager);
        let slot = manager.slot(&first.id).unwrap();
        let query = slot.query(first.version).unwrap();
        let operation = manager
            .accept_reconciliation(
                Token::parse("coalesced-receipt-race").unwrap(),
                ReconcileRequest {
                    view: first.id.clone(),
                    expected_version: first.version,
                    allocation_version: 1,
                },
            )
            .unwrap();
        let barrier = fixture
            .namespace
            .install_test_fault(Specification {
                point: Point::ReconcileBeforeComplete,
                operation: Some(operation.id.clone()),
                skip_hits: 0,
                action: Action::Pause { timeout_ms: 30_000 },
            })
            .unwrap();
        let failed = std::thread::scope(|scope| {
            let worker = scope.spawn(|| manager.execute(&operation.id));
            let deadline = Instant::now() + Duration::from_secs(15);
            let reached = loop {
                let status = fixture.namespace.test_fault_status().unwrap().unwrap();
                if status.stage == Stage::Waiting {
                    break Some(status);
                }
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            let changed = if reached.is_some() {
                if cancelled {
                    fixture
                        .namespace
                        .cancel_operation(&operation.id)
                        .map(|_| ())
                } else {
                    fixture.namespace.allocation().and_then(|allocation| {
                        fixture
                            .namespace
                            .update_allocation(allocation.version, allocation)
                            .map(|_| ())
                    })
                }
            } else {
                Ok(())
            };
            let released = fixture.namespace.release_test_fault(&barrier.ticket);
            let result = worker.join().unwrap();
            let reached = reached.expect("coalescing did not reach its receipt boundary");
            assert_eq!(reached.reached_operation.as_ref(), Some(&operation.id));
            changed.unwrap();
            released.unwrap();
            result.unwrap()
        });
        assert_eq!(
            failed.state,
            if cancelled {
                OperationState::Cancelled
            } else {
                OperationState::Failed
            },
            "{failed:?}"
        );
        assert_eq!(failed.committed_state, CommitState::NotCommitted);
        assert_eq!(
            failed.error.as_ref().unwrap()["category"],
            serde_json::to_value(if cancelled {
                ErrorCategory::Cancelled
            } else {
                ErrorCategory::StaleVersion
            })
            .unwrap()
        );
        assert_eq!(
            query
                .with_snapshot(|snapshot| snapshot.files("", false))
                .unwrap(),
            ["source.txt"]
        );
        query.complete().unwrap();
        let current = manager.recover(&first.id).unwrap();
        assert_eq!(current.current, first.current);
        assert_eq!(current.checkpoint, first.checkpoint);
        assert_eq!(current.input_epoch, first.input_epoch);
        assert!(manager.status(&first.id).unwrap().ready);
        assert_eq!(fixture.namespace.work_usage().unwrap().reservations, 0);
        assert_eq!(
            serde_json::to_value(manager.execute(&operation.id).unwrap()).unwrap(),
            serde_json::to_value(failed).unwrap()
        );
    }
}

#[test]
fn metadata_walk_reserves_directory_queue_before_retaining_a_wide_tree() {
    use tgrep_core::worktrees::{WorktreeOptions, WorktreeView};
    let mut fixture = Fixture::new();
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let permit = fixture.permit(8 * 1024 * 1024);
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let view =
        WorktreeView::new(&fixture.root, built.generation, WorktreeOptions::default()).unwrap();
    drop(permit);
    for index in 0..2000 {
        fs::write(fixture.root.join(format!("private-{index:04}.txt")), b"").unwrap();
    }
    let token = fixture.token("bounded-walk");
    let operation = fixture
        .namespace
        .accept_operation(token, "refresh", serde_json::json!({}))
        .unwrap();
    let permit = fixture
        .namespace
        .reserve(
            &operation.id,
            Some(&fixture.owner.registration().owner),
            WorkRequest {
                allocation_version: 1,
                staging_bytes: 1024 * 1024,
                private_bytes: 2 * 1024 * 1024,
                slots: 1,
            },
        )
        .unwrap();
    let error: tgrep_core::managed::Error = view.refresh_controlled(&permit).unwrap_err().into();
    assert_eq!(error.category, ErrorCategory::ResourcePressure, "{error:?}");
    assert_eq!(error.reason_code, "reserved-private-memory-exhausted");
    assert!(permit.peak_private_bytes() <= permit.private_limit());
    assert!(!view.status().unwrap().ready);
    drop(permit);
    assert_eq!(fixture.namespace.work_usage().unwrap().reservations, 0);
}

#[test]
fn repeated_blob_extraction_uses_table_capacity_not_a_whole_blob_worst_case() {
    let mut fixture = Fixture::new();
    let contents = b"Bounded repeated source line\n".repeat(24_000);
    fs::write(fixture.root.join("source.txt"), contents).unwrap();
    git(
        &fixture.root,
        &["commit", "--quiet", "-am", "large repeated source"],
    );
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let token = fixture.token("bounded-extraction");
    let operation = fixture
        .namespace
        .accept_operation(token, "ensure", serde_json::json!({}))
        .unwrap();
    let permit = fixture
        .namespace
        .reserve(
            &operation.id,
            Some(&fixture.owner.registration().owner),
            WorkRequest {
                allocation_version: 1,
                staging_bytes: 8 * 1024 * 1024,
                private_bytes: 16 * 1024 * 1024,
                slots: 1,
            },
        )
        .unwrap();
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    assert_eq!(built.descriptor.stats.blobs_extracted, 1);
    assert!(permit.peak_private_bytes() < permit.private_limit());
    assert_eq!(built.generation.base().reader().all_paths(), ["source.txt"]);
}

#[test]
fn retained_memory_follows_escaped_readers_without_retaining_work_slots() {
    let mut fixture = Fixture::new();
    let permit = fixture.permit(8 * 1024 * 1024);
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let materialized = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let reader = Arc::clone(materialized.generation.base().reader());
    let active = fixture.namespace.work_usage().unwrap();
    assert_eq!(active.reservations, 1);
    assert_eq!(active.memory.retained_allocations, 1);
    assert!(active.memory.mapped_bytes > 0);
    assert!(active.memory.retained_private_estimate_bytes > 0);
    assert_eq!(active.memory.unreserved_retained_private_estimate_bytes, 0);
    drop(materialized);
    drop(permit);
    let escaped = fixture.namespace.work_usage().unwrap();
    assert_eq!(escaped.reservations, 0);
    assert_eq!(escaped.reserved_slots, 0);
    assert_eq!(escaped.memory.mapped_bytes, active.memory.mapped_bytes);
    assert_eq!(
        escaped.memory.unreserved_retained_private_estimate_bytes,
        active.memory.retained_private_estimate_bytes
    );

    let mut allocation = fixture.namespace.allocation().unwrap();
    allocation.private_work_bytes = escaped.memory.retained_private_estimate_bytes + 1;
    let allocation = fixture
        .namespace
        .update_allocation(allocation.version, allocation)
        .unwrap();
    let token = fixture.token("memory-pressure");
    let operation = fixture
        .namespace
        .accept_operation(token, "ensure", serde_json::json!({}))
        .unwrap();
    let pressure = fixture.namespace.reserve(
        &operation.id,
        Some(&fixture.owner.registration().owner),
        WorkRequest {
            allocation_version: allocation.version,
            staging_bytes: 1,
            private_bytes: 2,
            slots: 1,
        },
    );
    assert!(matches!(pressure, Err(error) if error.category == ErrorCategory::ResourcePressure));
    drop(reader);
    let released = fixture.namespace.work_usage().unwrap();
    assert_eq!(released.memory.retained_private_estimate_bytes, 0);
    assert_eq!(released.memory.mapped_bytes, 0);
    assert_eq!(released.memory.retained_allocations, 0);
    assert_eq!(
        released.memory.peak_mapped_bytes,
        active.memory.mapped_bytes
    );
}

#[test]
fn atomic_migration_preserves_dirty_overlay_old_queries_and_exact_replay() {
    use tgrep_core::managed::{MigrationRequest, OperationState, RefreshRequest};
    let mut fixture = Fixture::new();
    let manager = fixture.manager();
    let first = fixture.attach(&manager);
    let slot = manager.slot(&first.id).unwrap();
    fs::write(
        fixture.root.join("source.txt"),
        "new committed branch needle\n",
    )
    .unwrap();
    git(&fixture.root, &["commit", "--quiet", "-am", "successor"]);
    let successor = git(&fixture.root, &["rev-parse", "HEAD"]);
    fs::write(
        fixture.root.join("source.txt"),
        "dirty replacement needle\n",
    )
    .unwrap();
    fs::write(fixture.root.join("private.txt"), "untracked needle\n").unwrap();
    slot.invalidate(&[], true).unwrap();
    let refresh = manager
        .accept_refresh(
            fixture.token("refresh"),
            RefreshRequest {
                view: first.id.clone(),
                expected_version: 1,
                owner: fixture.owner.registration().owner.clone(),
                allocation_version: fixture.namespace.allocation().unwrap().version,
            },
        )
        .unwrap();
    let refreshed = manager.execute(&refresh.id).unwrap();
    assert_eq!(refreshed.state, OperationState::Completed, "{refreshed:?}");
    let old_query = slot.query(1).unwrap();
    let before_memory = fixture.namespace.work_usage().unwrap().memory;
    assert_eq!(before_memory.retained_allocations, 2);
    let token = fixture.token("migrate");
    let request = MigrationRequest {
        view: first.id.clone(),
        root: fixture.root.clone(),
        expected_version: 1,
        target_commit: successor.clone(),
        profile: IndexingProfile::default(),
        owner: fixture.owner.registration().owner.clone(),
        allocation_version: fixture.namespace.allocation().unwrap().version,
    };
    let migration = manager
        .accept_migration(token.clone(), request.clone())
        .unwrap();
    let completed = manager.execute(&migration.id).unwrap();
    assert_eq!(completed.state, OperationState::Completed, "{completed:?}");
    let current = manager.recover(&first.id).unwrap();
    assert_eq!(current.version, 2);
    assert_eq!(current.pin().unwrap().commit, successor);
    assert_ne!(
        current.pin().unwrap().incarnation,
        first.pin().unwrap().incarnation
    );
    let overlapping_memory = fixture.namespace.work_usage().unwrap();
    assert_eq!(overlapping_memory.reservations, 0);
    assert_eq!(overlapping_memory.memory.retained_allocations, 4);
    assert!(
        overlapping_memory.memory.retained_private_estimate_bytes
            > before_memory.retained_private_estimate_bytes
    );
    old_query.validate().unwrap();
    assert_eq!(
        old_query.published().record.pin().unwrap().commit,
        first.pin().unwrap().commit
    );
    let query = slot.query(2).unwrap();
    let mut paths = query
        .with_snapshot(|snapshot| snapshot.files("", false))
        .unwrap();
    paths.sort();
    assert_eq!(paths, ["private.txt", "source.txt"]);
    let mut candidate = query
        .with_snapshot(|snapshot| snapshot.open_candidate("source.txt"))
        .unwrap()
        .unwrap();
    let mut bytes = String::new();
    std::io::Read::read_to_string(&mut candidate, &mut bytes).unwrap();
    assert_eq!(bytes, "dirty replacement needle\n");
    let replay = manager.accept_migration(token, request).unwrap();
    assert_eq!(replay.id, migration.id);
    assert_eq!(
        manager.execute(&replay.id).unwrap().result,
        completed.result
    );
    assert_eq!(
        fixture
            .namespace
            .cancel_operation(&migration.id)
            .unwrap()
            .state,
        OperationState::Completed
    );
    assert!(slot.query(1).is_err());
    drop(query);
    let detached = manager
        .detach(
            &fixture.owner.registration().owner,
            &Token::parse("primary-lease").unwrap(),
            &first.id,
        )
        .unwrap();
    assert!(detached.lease_released);
    assert_eq!(detached.remaining_leases, 0);
    assert!(!detached.root_handles_released);
    drop(old_query);
    assert!(
        !manager
            .detach(
                &fixture.owner.registration().owner,
                &Token::parse("primary-lease").unwrap(),
                &first.id
            )
            .unwrap()
            .root_handles_released
    );
    drop(candidate);
    assert!(
        manager
            .detach(
                &fixture.owner.registration().owner,
                &Token::parse("primary-lease").unwrap(),
                &first.id
            )
            .unwrap()
            .root_handles_released
    );
}

#[test]
fn fixed_pin_participant_blocks_migration_until_its_independent_detach() {
    use tgrep_core::managed::{AttachRequest, MigrationRequest, OperationState, ViewVersion};
    let mut fixture = Fixture::new();
    let manager = fixture.manager();
    let first = fixture.attach(&manager);
    let claim = fixture.namespace.prepare_owner().unwrap().claim;
    let fixed_owner = OwnerGuard::claim(claim).unwrap();
    fixture
        .namespace
        .register_owner(fixed_owner.registration())
        .unwrap();
    let attached = manager
        .accept_attach(
            OperationToken {
                scope: fixed_owner.registration().owner.clone(),
                sequence: 1,
                token: Token::parse("fixed-attach").unwrap(),
            },
            AttachRequest {
                root: fixture.root.clone(),
                revision: None,
                profile: IndexingProfile::default(),
                lease: Token::parse("fixed-lease").unwrap(),
                owner: fixed_owner.registration().owner.clone(),
                accept_current: Some(ViewVersion {
                    view: first.id.clone(),
                    version: 1,
                }),
                migratable: false,
                allocation_version: fixture.namespace.allocation().unwrap().version,
            },
        )
        .unwrap();
    let attached = manager.execute(&attached.id).unwrap();
    assert_eq!(attached.state, OperationState::Completed, "{attached:?}");
    fs::write(fixture.root.join("source.txt"), "committed successor\n").unwrap();
    git(&fixture.root, &["commit", "--quiet", "-am", "successor"]);
    let request = MigrationRequest {
        view: first.id.clone(),
        root: fixture.root.clone(),
        expected_version: 1,
        target_commit: git(&fixture.root, &["rev-parse", "HEAD"]),
        profile: IndexingProfile::default(),
        owner: fixture.owner.registration().owner.clone(),
        allocation_version: fixture.namespace.allocation().unwrap().version,
    };
    let blocked = manager
        .accept_migration(fixture.token("blocked"), request.clone())
        .unwrap();
    let blocked = manager.execute(&blocked.id).unwrap();
    assert_eq!(blocked.state, OperationState::Failed);
    assert_eq!(
        blocked.error.unwrap()["reason_code"],
        "fixed-pin-participant"
    );
    let release = manager
        .detach(
            &fixed_owner.registration().owner,
            &Token::parse("fixed-lease").unwrap(),
            &first.id,
        )
        .unwrap();
    assert_eq!(release.remaining_leases, 1);
    assert!(!release.root_handles_released);
    let migration = manager
        .accept_migration(fixture.token("allowed"), request)
        .unwrap();
    let completed = manager.execute(&migration.id).unwrap();
    assert_eq!(completed.state, OperationState::Completed, "{completed:?}");
    assert_eq!(manager.recover(&first.id).unwrap().version, 2);
}

#[test]
fn pending_inputs_do_not_exhaust_adaptive_work_before_reconciliation() {
    use tgrep_core::managed::{AdaptiveRequest, OperationState, RefreshRequest};
    let mut fixture = Fixture::new();
    let mut configured = fixture.namespace.policy().unwrap();
    configured.policy.advancement = tgrep_core::managed::policy::Advancement::Adaptive {
        high_bytes: 1,
        low_bytes: 0,
        min_reduction_bytes: 1,
        min_reduction_percent: 1,
        cooldown_ms: 1,
        max_paths: 32,
        max_read_bytes: 1048576,
        max_attempts: 1,
    };
    fixture
        .namespace
        .update_policy(configured.version, configured.policy)
        .unwrap();
    let manager = fixture.manager();
    let view = fixture.attach(&manager);
    fs::write(
        fixture.root.join("source.txt"),
        "reconciled adaptive successor needle\n",
    )
    .unwrap();
    git(&fixture.root, &["commit", "--quiet", "-am", "successor"]);
    let target = git(&fixture.root, &["rev-parse", "HEAD"]);
    let slot = manager.slot(&view.id).unwrap();
    slot.invalidate(&[], true).unwrap();
    let request = AdaptiveRequest {
        view: view.id.clone(),
        owner: fixture.owner.registration().owner.clone(),
        expected_version: 1,
        allocation_version: 1,
    };
    let raced = manager
        .accept_adaptive(fixture.token("before-reconciliation"), request.clone())
        .unwrap();
    let raced = manager.execute(&raced.id).unwrap();
    assert_eq!(raced.state, OperationState::Failed, "{raced:?}");
    assert_eq!(
        raced.error.as_ref().unwrap()["reason_code"],
        "view-input-pending"
    );
    assert_eq!(
        raced.committed_state,
        tgrep_core::managed::CommitState::NotCommitted
    );
    assert!(!manager.status(&view.id).unwrap().ready);
    let refresh = manager
        .accept_refresh(
            fixture.token("refresh"),
            RefreshRequest {
                view: view.id.clone(),
                expected_version: 1,
                owner: request.owner.clone(),
                allocation_version: 1,
            },
        )
        .unwrap();
    assert_eq!(
        manager.execute(&refresh.id).unwrap().state,
        OperationState::Completed
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !manager
        .automatic_advancement_due(&view.id, &request.owner)
        .unwrap()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "reconciliation did not rearm work that never evaluated current input: {:?}",
            manager.last_adaptive_error(&view.id).unwrap()
        );
        std::thread::yield_now();
    }
    let next = manager
        .accept_adaptive(fixture.token("after-reconciliation"), request)
        .unwrap();
    let next = manager.execute(&next.id).unwrap();
    assert_eq!(next.state, OperationState::Completed, "{next:?}");
    assert_eq!(manager.recover(&view.id).unwrap().version, 2);
    assert_eq!(
        manager.recover(&view.id).unwrap().pin().unwrap().commit,
        target
    );
    assert!(manager.status(&view.id).unwrap().ready);
}

#[test]
fn deferred_adaptive_migration_retains_its_target_and_single_attempt_budget() {
    use tgrep_core::managed::{AdaptiveRequest, OperationState, RefreshRequest};
    let mut fixture = Fixture::new();
    let mut configured = fixture.namespace.policy().unwrap();
    configured.policy.advancement = tgrep_core::managed::policy::Advancement::Adaptive {
        high_bytes: 1,
        low_bytes: 0,
        min_reduction_bytes: 1,
        min_reduction_percent: 1,
        cooldown_ms: 60000,
        max_paths: 32,
        max_read_bytes: 1048576,
        max_attempts: 1,
    };
    fixture
        .namespace
        .update_policy(configured.version, configured.policy)
        .unwrap();
    let manager = fixture.manager();
    let view = fixture.attach(&manager);
    let owner = fixture.owner.registration().owner.clone();
    fs::write(fixture.root.join("source.txt"), "adaptive target needle\n").unwrap();
    git(
        &fixture.root,
        &["commit", "--quiet", "-am", "adaptive target"],
    );
    let target = git(&fixture.root, &["rev-parse", "HEAD"]);
    manager
        .invalidate(&view.id, &owner, view.version, &[], true)
        .unwrap();
    let refresh = manager
        .accept_refresh(
            fixture.token("adaptive-source-refresh"),
            RefreshRequest {
                view: view.id.clone(),
                owner: owner.clone(),
                expected_version: view.version,
                allocation_version: 1,
            },
        )
        .unwrap();
    assert_eq!(
        manager.execute(&refresh.id).unwrap().state,
        OperationState::Completed
    );
    let permit = fixture.permit(8 * 1024 * 1024);
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &target,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let incarnation = built.descriptor.incarnation.clone();
    drop((built, permit));
    let guard = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            fixture
                .namespace
                .path()
                .join("guards")
                .join(format!("{incarnation}.lock")),
        )
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&guard).unwrap();
    let operation = manager
        .accept_adaptive(
            fixture.token("contended-adaptive"),
            AdaptiveRequest {
                view: view.id.clone(),
                owner,
                expected_version: view.version,
                allocation_version: 1,
            },
        )
        .unwrap();
    let deferred = manager.execute(&operation.id).unwrap();
    assert!(matches!(
        deferred.state,
        OperationState::Accepted | OperationState::Preparing
    ));
    assert_eq!(deferred.progress["adaptive"]["target_commit"], target);
    assert_eq!(
        deferred.progress["waiting_reason"],
        "object-retirement-in-progress"
    );
    assert!(manager.status(&view.id).unwrap().ready);
    git(
        &fixture.root,
        &[
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "later exact HEAD",
        ],
    );
    assert_ne!(git(&fixture.root, &["rev-parse", "HEAD"]), target);
    fs2::FileExt::unlock(&guard).unwrap();
    drop(guard);
    let completed = manager.execute(&operation.id).unwrap();
    assert_eq!(completed.state, OperationState::Completed, "{completed:?}");
    let current = manager.recover(&view.id).unwrap();
    assert_eq!(current.version, view.version + 1, "{completed:?}");
    assert_eq!(current.pin().unwrap().commit, target);
    assert_eq!(
        current.pin().unwrap().incarnation.as_ref(),
        Some(&incarnation)
    );
    assert!(manager.status(&view.id).unwrap().ready);
    let cooldown = manager
        .accept_adaptive(
            fixture.token("adaptive-cooldown"),
            AdaptiveRequest {
                view: view.id.clone(),
                owner: fixture.owner.registration().owner.clone(),
                expected_version: current.version,
                allocation_version: 1,
            },
        )
        .unwrap();
    let cooldown = manager.execute(&cooldown.id).unwrap();
    assert_eq!(cooldown.state, OperationState::Completed);
    assert_eq!(cooldown.progress["adaptive"]["eligible"], false);
    assert_eq!(
        cooldown.progress["adaptive"]["reason_code"],
        "adaptive-cooldown"
    );
    assert_eq!(
        cooldown.progress["adaptive"],
        cooldown.result.as_ref().unwrap()["adaptive"]
    );
    assert_eq!(manager.recover(&view.id).unwrap().version, current.version);
}

#[test]
#[cfg(feature = "managed-test-hooks")]
fn changed_allocation_rearms_exhausted_adaptive_work_without_changing_the_target() {
    use tgrep_core::managed::faults::{Action, Point, Specification};
    use tgrep_core::managed::{AdaptiveRequest, OperationState, RefreshRequest};
    let mut fixture = Fixture::new();
    let mut configured = fixture.namespace.policy().unwrap();
    configured.policy.advancement = tgrep_core::managed::policy::Advancement::Adaptive {
        high_bytes: 1,
        low_bytes: 0,
        min_reduction_bytes: 1,
        min_reduction_percent: 1,
        cooldown_ms: 1,
        max_paths: 32,
        max_read_bytes: 1048576,
        max_attempts: 1,
    };
    fixture
        .namespace
        .update_policy(configured.version, configured.policy)
        .unwrap();
    let manager = fixture.manager();
    let first = fixture.attach(&manager);
    fs::write(
        fixture.root.join("source.txt"),
        "committed adaptive allocation successor\n",
    )
    .unwrap();
    git(&fixture.root, &["commit", "--quiet", "-am", "successor"]);
    let head = git(&fixture.root, &["rev-parse", "HEAD"]);
    manager
        .slot(&first.id)
        .unwrap()
        .invalidate(&[], true)
        .unwrap();
    let refresh = manager
        .accept_refresh(
            fixture.token("refresh"),
            RefreshRequest {
                view: first.id.clone(),
                expected_version: 1,
                owner: fixture.owner.registration().owner.clone(),
                allocation_version: 1,
            },
        )
        .unwrap();
    assert_eq!(
        manager.execute(&refresh.id).unwrap().state,
        OperationState::Completed
    );
    fixture
        .namespace
        .install_test_fault(Specification {
            point: Point::GenerationBuilt,
            operation: None,
            skip_hits: 0,
            action: Action::Error {
                category: ErrorCategory::ResourcePressure,
            },
        })
        .unwrap();
    let mut request = AdaptiveRequest {
        view: first.id.clone(),
        owner: fixture.owner.registration().owner.clone(),
        expected_version: 1,
        allocation_version: 1,
    };
    let failed = manager
        .accept_adaptive(fixture.token("pressure"), request.clone())
        .unwrap();
    let failed = manager.execute(&failed.id).unwrap();
    assert_eq!(failed.state, OperationState::Failed, "{failed:?}");
    assert_eq!(failed.error.unwrap()["category"], "resource-pressure");
    assert!(
        !manager
            .automatic_advancement_due(&first.id, &request.owner)
            .unwrap()
    );
    assert_eq!(manager.recover(&first.id).unwrap().version, 1);
    assert!(manager.status(&first.id).unwrap().ready);
    let allocation = fixture.namespace.allocation().unwrap();
    let allocation = fixture
        .namespace
        .update_allocation(allocation.version, allocation)
        .unwrap();
    request.allocation_version = allocation.version;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !manager
        .automatic_advancement_due(&first.id, &request.owner)
        .unwrap()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "allocation change did not rearm adaptive work"
        );
        std::thread::yield_now();
    }
    let retry = manager
        .accept_adaptive(fixture.token("new-allocation"), request)
        .unwrap();
    let done = manager.execute(&retry.id).unwrap();
    assert_eq!(done.state, OperationState::Completed, "{done:?}");
    let current = manager.recover(&first.id).unwrap();
    assert_eq!(current.version, 2);
    assert_eq!(current.pin().unwrap().commit, head);
}

#[test]
fn adaptive_advancement_uses_own_head_and_rejects_untracked_or_transformed_cost() {
    use tgrep_core::managed::{AdaptiveRequest, OperationState, RefreshRequest};
    let mut fixture = Fixture::new();
    let mut configured = fixture.namespace.policy().unwrap();
    configured.policy.advancement = tgrep_core::managed::policy::Advancement::Adaptive {
        high_bytes: 1,
        low_bytes: 0,
        min_reduction_bytes: 1,
        min_reduction_percent: 1,
        cooldown_ms: 1,
        max_paths: 32,
        max_read_bytes: 1048576,
        max_attempts: 3,
    };
    fixture
        .namespace
        .update_policy(configured.version, configured.policy)
        .unwrap();
    let manager = fixture.manager();
    let first = fixture.attach(&manager);
    let slot = manager.slot(&first.id).unwrap();
    fs::write(
        fixture.root.join("source.txt"),
        "adaptive committed successor needle\n",
    )
    .unwrap();
    git(&fixture.root, &["commit", "--quiet", "-am", "successor"]);
    let head = git(&fixture.root, &["rev-parse", "HEAD"]);
    slot.invalidate(&[], true).unwrap();
    let refresh = manager
        .accept_refresh(
            fixture.token("refresh"),
            RefreshRequest {
                view: first.id.clone(),
                expected_version: 1,
                owner: fixture.owner.registration().owner.clone(),
                allocation_version: fixture.namespace.allocation().unwrap().version,
            },
        )
        .unwrap();
    assert_eq!(
        manager.execute(&refresh.id).unwrap().state,
        OperationState::Completed
    );
    let advance = manager
        .accept_adaptive(
            fixture.token("adaptive"),
            AdaptiveRequest {
                view: first.id.clone(),
                owner: fixture.owner.registration().owner.clone(),
                expected_version: 1,
                allocation_version: fixture.namespace.allocation().unwrap().version,
            },
        )
        .unwrap();
    let result = manager.execute(&advance.id).unwrap();
    assert_eq!(result.state, OperationState::Completed, "{result:?}");
    assert_eq!(
        manager.recover(&first.id).unwrap().pin().unwrap().commit,
        head
    );
    assert_eq!(manager.recover(&first.id).unwrap().version, 2);
    assert_eq!(manager.status(&first.id).unwrap().overlay.unwrap().bytes, 0);
    fs::write(
        fixture.root.join("private.txt"),
        "large untracked private needle\n",
    )
    .unwrap();
    // The committed blob changes, but checkout transformation leaves different bytes.
    fs::write(fixture.root.join("source.txt"), "raw next blob\n").unwrap();
    git(
        &fixture.root,
        &["commit", "--quiet", "-am", "raw successor"],
    );
    fs::write(
        fixture.root.join("source.txt"),
        "smudged transformed bytes\n",
    )
    .unwrap();
    slot.invalidate(&[], true).unwrap();
    let refresh = manager
        .accept_refresh(
            fixture.token("refresh"),
            RefreshRequest {
                view: first.id.clone(),
                expected_version: 2,
                owner: fixture.owner.registration().owner.clone(),
                allocation_version: fixture.namespace.allocation().unwrap().version,
            },
        )
        .unwrap();
    assert_eq!(
        manager.execute(&refresh.id).unwrap().state,
        OperationState::Completed
    );
    let advance = manager
        .accept_adaptive(
            fixture.token("no-churn"),
            AdaptiveRequest {
                view: first.id.clone(),
                owner: fixture.owner.registration().owner.clone(),
                expected_version: 2,
                allocation_version: fixture.namespace.allocation().unwrap().version,
            },
        )
        .unwrap();
    let result = manager.execute(&advance.id).unwrap();
    assert_eq!(result.state, OperationState::Completed, "{result:?}");
    assert_eq!(result.result.unwrap()["adaptive"]["eligible"], false);
    assert_eq!(manager.recover(&first.id).unwrap().version, 2);
    let objects = fixture.namespace.page(None).unwrap();
    assert_eq!(
        objects
            .objects
            .iter()
            .filter(|object| object.kind == tgrep_core::managed::ObjectKind::Generation)
            .count(),
        2
    );
}

#[test]
fn managed_publication_reuses_one_guarded_generation() {
    let mut fixture = Fixture::new();
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let permit = fixture.permit(16 * 1024 * 1024);
    let first = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    assert_eq!(first.descriptor.stats.blobs_extracted, 1);
    assert!(first.descriptor.stats.published);
    assert_eq!(first.generation.retention(), RetentionPolicy::Managed);
    let second = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    assert_eq!(second.descriptor.stats.blobs_extracted, 0);
    assert!(second.descriptor.stats.reused_generation);
    assert!(Arc::ptr_eq(&first.generation, &second.generation));
    let external = tgrep_core::managed::open_generation(
        fixture.namespace.path(),
        &first.descriptor.incarnation,
    )
    .unwrap();
    assert!(Arc::ptr_eq(&first.generation, &external));
    assert!(permit.bytes_written() > 0);
    assert!(permit.peak_private_bytes() > 0);
}

#[test]
fn transient_retirement_locks_defer_attachment_without_changing_its_exact_intent() {
    use tgrep_core::managed::{AttachRequest, OperationState, ViewRecord};
    let mut fixture = Fixture::new();
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let permit = fixture.permit(8 * 1024 * 1024);
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let incarnation = built.descriptor.incarnation.clone();
    drop((built, permit));
    let guard = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            fixture
                .namespace
                .path()
                .join("guards")
                .join(format!("{incarnation}.lock")),
        )
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&guard).unwrap();
    let manager = fixture.manager();
    let operation = manager
        .accept_attach(
            fixture.token("contended-attach"),
            AttachRequest {
                root: fixture.root.clone(),
                revision: Some("HEAD".into()),
                profile: IndexingProfile::default(),
                lease: Token::parse("contended-lease").unwrap(),
                owner: fixture.owner.registration().owner.clone(),
                accept_current: None,
                migratable: true,
                allocation_version: 1,
            },
        )
        .unwrap();
    let deferred = manager.execute(&operation.id).unwrap();
    assert!(matches!(
        deferred.state,
        OperationState::Accepted | OperationState::Preparing
    ));
    assert_eq!(
        deferred.progress["waiting_reason"],
        "object-retirement-in-progress"
    );
    assert_eq!(deferred.progress["resolved_commit"], commit);
    fs::write(fixture.root.join("source.txt"), "a later committed head\n").unwrap();
    git(&fixture.root, &["commit", "--quiet", "-am", "later head"]);
    fs2::FileExt::unlock(&guard).unwrap();
    drop(guard);
    let completed = manager.execute(&operation.id).unwrap();
    assert_eq!(completed.state, OperationState::Completed, "{completed:?}");
    assert_eq!(completed.id, operation.id);
    let view: ViewRecord =
        serde_json::from_value(completed.result.unwrap()["current"].clone()).unwrap();
    assert_eq!(view.pin().unwrap().commit, commit);
    assert_eq!(view.pin().unwrap().incarnation.as_ref(), Some(&incarnation));
    let query = manager.slot(&view.id).unwrap().query(view.version).unwrap();
    let mut candidate = query
        .with_snapshot(|snapshot| snapshot.open_candidate("source.txt"))
        .unwrap()
        .unwrap();
    let mut bytes = String::new();
    std::io::Read::read_to_string(&mut candidate, &mut bytes).unwrap();
    assert_eq!(
        bytes,
        fs::read_to_string(fixture.root.join("source.txt")).unwrap()
    );
    query.validate().unwrap();
}

#[test]
fn unknown_missing_replaced_and_inaccessible_owner_proofs_never_release_leases() {
    use tgrep_core::managed::OwnerProof;
    let mut fixture = Fixture::new();
    let manager = fixture.manager();
    let view = fixture.attach(&manager);
    let owner = fixture.owner.registration().owner.clone();
    drop(fixture.owner);
    let proof = fixture
        .namespace
        .path()
        .join("owners")
        .join(format!("{owner}.lock"));
    let backup = proof.with_extension("held-original");
    let assert_protected = || {
        let inspected = fixture.namespace.inspect_owner(&owner).unwrap();
        assert_eq!(inspected.last_proof, OwnerProof::Unknown);
        assert!(!inspected.released);
        assert!(fixture.namespace.reap_owner(&owner).is_err());
        assert!(!fixture.namespace.owner(&owner).unwrap().released);
        assert_eq!(manager.status(&view.id).unwrap().leases, 1);
        manager
            .slot(&view.id)
            .unwrap()
            .query(1)
            .unwrap()
            .validate()
            .unwrap();
    };
    fs::rename(&proof, &backup).unwrap();
    assert_protected();
    assert!(
        fixture
            .namespace
            .inspect_owner(&owner)
            .unwrap()
            .last_error
            .is_some()
    );
    fs::write(&proof, []).unwrap();
    assert_protected();
    fs::remove_file(&proof).unwrap();
    fs::rename(&backup, &proof).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let denied = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&proof)
            .unwrap();
        assert_protected();
        assert!(
            fixture
                .namespace
                .inspect_owner(&owner)
                .unwrap()
                .last_error
                .is_some()
        );
        drop(denied);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::metadata(&proof).unwrap().permissions();
        fs::set_permissions(&proof, fs::Permissions::from_mode(0o0)).unwrap();
        let inspection = fixture.namespace.inspect_owner(&owner).unwrap();
        if unsafe { libc::geteuid() } == 0 {
            assert_eq!(inspection.last_proof, OwnerProof::Ended);
            assert!(
                inspection.last_error.is_none(),
                "privileged opens must report their actual evidence"
            );
        } else {
            assert_protected();
            assert_eq!(
                inspection.last_error.unwrap()["category"],
                serde_json::to_value(ErrorCategory::Permission).unwrap()
            );
        }
        fs::set_permissions(&proof, permissions).unwrap();
    }
    assert_eq!(
        fixture.namespace.inspect_owner(&owner).unwrap().last_proof,
        OwnerProof::Ended
    );
    assert_eq!(
        fixture
            .namespace
            .reap_owner(&owner)
            .unwrap()
            .leases_released,
        1
    );
    manager.drain_released().unwrap();
}

#[test]
fn managed_format_and_escaped_reader_cannot_bypass_ownership() {
    let mut fixture = Fixture::new();
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let permit = fixture.permit(16 * 1024 * 1024);
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let directory = built.generation.directory().to_path_buf();
    assert!(tgrep_core::reader::IndexReader::open(&directory).is_err());
    assert!(tgrep_core::shared::SharedBase::open(&directory).is_err());
    let copied = fixture.temp.path().join("renamed-sections");
    fs::create_dir(&copied).unwrap();
    for (managed, legacy) in [
        ("paths.tgm", "files.bin"),
        ("lookup.tgm", "lookup.bin"),
        ("postings.tgm", "index.bin"),
    ] {
        fs::copy(directory.join(managed), copied.join(legacy)).unwrap();
    }
    assert!(tgrep_core::reader::IndexReader::open(&copied).is_err());
    let raw = Arc::clone(built.generation.base().reader());
    let hybrid = built
        .generation
        .base()
        .create_worktree(&fixture.root)
        .unwrap();
    assert!(
        built
            .generation
            .base()
            .save_overlay(&hybrid, &fixture.temp.path().join("unmanaged.json"))
            .is_err()
    );
    let guard_path = fixture
        .namespace
        .path()
        .join("guards")
        .join(format!("{}.lock", built.descriptor.incarnation));
    let guard = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(guard_path)
        .unwrap();
    drop(hybrid);
    drop(built);
    assert!(
        fs2::FileExt::try_lock_exclusive(&guard).is_err(),
        "escaped raw reader must protect the physical object"
    );
    drop(raw);
    fs2::FileExt::try_lock_exclusive(&guard).unwrap();
}

#[test]
fn unrelated_store_named_directories_keep_ordinary_indexes_usable() {
    let fixture = Fixture::new();
    let unrelated = fixture
        .temp
        .path()
        .join("checkout")
        .join(tgrep_core::managed::STORE_DIRECTORY);
    let project = unrelated.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("note.txt"), b"ordinary needle\n").unwrap();
    let index = project.join(".tgrep");
    tgrep_core::builder::build_index(&project, Some(&index), true, false, &[]).unwrap();
    tgrep_core::reader::IndexReader::open(&index).unwrap();
    let meta = tgrep_core::meta::IndexMeta::load(&index).unwrap();
    meta.save(&index).unwrap();
    let legacy = unrelated.join("legacy-shared");
    fs::create_dir(&legacy).unwrap();
    tgrep_core::generations::GenerationManager::with_storage(fixture.repository.clone(), &legacy)
        .unwrap();

    let namespace = fixture.namespace.path();
    let store = namespace.parent().unwrap();
    for reserved in [
        store.to_path_buf(),
        namespace.to_path_buf(),
        namespace.join("objects"),
        namespace.join("objects").join("preparing"),
    ] {
        let error = meta.save(&reserved).unwrap_err().to_string();
        assert!(
            error.contains("protected managed reader"),
            "{reserved:?}: {error}"
        );
        assert!(!reserved.join("meta.json").exists(), "{reserved:?}");
        assert!(tgrep_core::reader::IndexReader::open(&reserved).is_err());
    }
    let error =
        tgrep_core::generations::GenerationManager::with_storage(fixture.repository.clone(), store)
            .err()
            .expect("legacy storage cannot be the managed store directory")
            .to_string();
    assert!(error.contains("protected managed reader"), "{error}");
}

#[test]
fn staging_exhaustion_is_typed_and_never_published() {
    let mut fixture = Fixture::new();
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let permit = fixture.permit(32);
    let error = match fixture.namespace.ensure_generation(
        &fixture.repository,
        &commit,
        IndexingProfile::default(),
        None,
        &permit,
    ) {
        Ok(_) => panic!("tiny reservation must not publish"),
        Err(error) => error,
    };
    assert_eq!(error.category, ErrorCategory::ResourcePressure);
    let (_, key) = fixture
        .namespace
        .generation_key(&fixture.repository, &commit, IndexingProfile::default())
        .unwrap();
    assert_eq!(fixture.namespace.find_generation(&key).unwrap(), None);
    assert!(permit.bytes_written() <= 32);
    let page = fixture.namespace.page(None).unwrap();
    let partial = page
        .objects
        .iter()
        .find(|object| object.operation.as_ref() == Some(permit.operation_id()))
        .unwrap();
    assert!(
        partial.logical_bytes > 0,
        "partial seals must retain their actual charged bytes"
    );
    assert_eq!(
        partial.logical_bytes,
        fs::metadata(
            fixture
                .namespace
                .path()
                .join("objects")
                .join(partial.id.as_str())
                .join("object.json"),
        )
        .unwrap()
        .len()
    );
}

#[test]
fn inactive_roots_get_new_view_and_guard_incarnations_without_exhausting_slots() {
    let mut fixture = Fixture::new();
    let mut configured = fixture.namespace.policy().unwrap();
    configured.policy.work.max_views = 1;
    fixture
        .namespace
        .update_policy(configured.version, configured.policy)
        .unwrap();
    let manager = fixture.manager();
    let mut previous = None;
    for round in 0..3 {
        let lease = Token::parse(format!("incarnation-{round}")).unwrap();
        let operation = manager
            .accept_attach(
                fixture.token("attach"),
                tgrep_core::managed::AttachRequest {
                    root: fixture.root.clone(),
                    revision: Some("HEAD".into()),
                    profile: IndexingProfile::default(),
                    lease: lease.clone(),
                    owner: fixture.owner.registration().owner.clone(),
                    accept_current: None,
                    migratable: true,
                    allocation_version: fixture.namespace.allocation().unwrap().version,
                },
            )
            .unwrap();
        let result = manager.execute(&operation.id).unwrap();
        assert_eq!(
            result.state,
            tgrep_core::managed::OperationState::Completed,
            "{result:?}"
        );
        let record: tgrep_core::managed::ViewRecord =
            serde_json::from_value(result.result.unwrap()["current"].clone()).unwrap();
        assert!(manager.status(&record.id).unwrap().ready);
        if let Some((old_view, old_guard)) = previous {
            assert_ne!(record.id, old_view);
            assert_ne!(record.root_anchor.as_ref().unwrap().guard, old_guard);
        }
        let detached = manager
            .detach(&fixture.owner.registration().owner, &lease, &record.id)
            .unwrap();
        assert!(detached.root_handles_released);
        manager.drain_released().unwrap();
        assert!(!manager.status(&record.id).unwrap().ready);
        assert!(
            manager
                .detach(&fixture.owner.registration().owner, &lease, &record.id)
                .unwrap()
                .root_handles_released
        );
        previous = Some((record.id, record.root_anchor.unwrap().guard));
    }
}

#[test]
fn offline_recovery_preserves_live_owner_and_reader_proofs_after_git_deletion() {
    use tgrep_core::managed::{ExternalWork, OwnerProof};
    let mut fixture = Fixture::new();
    let permit = fixture.permit(16 * 1024 * 1024);
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let operation = permit.operation_id().clone();
    let path = fixture.namespace.path().to_path_buf();
    let old_instance = fixture.namespace.instance().clone();
    drop(permit);
    let Fixture {
        namespace,
        owner,
        repository,
        root,
        temp,
        ..
    } = fixture;
    drop(namespace);
    drop(repository);
    fs::remove_dir_all(&root).unwrap();
    let namespace = Namespace::open(&path).unwrap();
    namespace.activate().unwrap();
    assert_ne!(namespace.instance(), &old_instance);
    assert_eq!(
        namespace
            .inspect_owner(&owner.registration().owner)
            .unwrap()
            .last_proof,
        OwnerProof::Held
    );
    let mut cursor = None;
    for _ in 0..32 {
        let progress = namespace.recover_pass(cursor).unwrap();
        assert!(progress.issues.is_empty(), "{progress:?}");
        assert!(progress.examined <= namespace.policy().unwrap().policy.work.page_objects);
        cursor = progress.next;
        if cursor.is_none() {
            break;
        }
    }
    assert!(cursor.is_none());
    assert!(
        !namespace
            .owner(&owner.registration().owner)
            .unwrap()
            .released
    );
    let recovered_operation = namespace.operation(&operation).unwrap();
    assert_eq!(
        recovered_operation.state,
        tgrep_core::managed::OperationState::Cancelled
    );
    assert_eq!(
        recovered_operation.committed_state,
        tgrep_core::managed::CommitState::NotCommitted
    );
    assert_eq!(
        recovered_operation.error.unwrap()["reason_code"],
        "interrupted-operation"
    );
    assert!(
        !namespace
            .stop_if_idle(ExternalWork::default())
            .unwrap()
            .stopping
    );
    namespace.release_owner(owner.registration()).unwrap();
    let busy = namespace.stop_if_idle(ExternalWork::default()).unwrap();
    assert!(!busy.stopping && busy.namespace_readers_or_work);
    drop(owner);
    drop(built);
    assert!(
        namespace
            .stop_if_idle(ExternalWork::default())
            .unwrap()
            .stopping
    );
    assert!(namespace.activate().is_err());
    drop(namespace);
    drop(temp);
}

#[test]
fn idle_shutdown_distinguishes_lease_release_from_escaped_root_readers() {
    use tgrep_core::managed::ExternalWork;
    let mut fixture = Fixture::new();
    let manager = fixture.manager();
    let record = fixture.attach(&manager);
    let query = manager
        .slot(&record.id)
        .unwrap()
        .query(record.version)
        .unwrap();
    let candidate = query
        .with_snapshot(|snapshot| snapshot.open_candidate("source.txt"))
        .unwrap()
        .unwrap();
    drop(query);
    fixture
        .namespace
        .release_owner(fixture.owner.registration())
        .unwrap();
    manager.drain_released().unwrap();
    assert!(
        !fixture
            .namespace
            .stop_if_idle(ExternalWork::default())
            .unwrap()
            .stopping
    );
    drop(candidate);
    manager.drain_released().unwrap();
    assert!(
        fixture
            .namespace
            .stop_if_idle(ExternalWork::default())
            .unwrap()
            .stopping
    );
    assert!(fixture.namespace.prepare_owner().is_err());
}

#[test]
fn namespace_allocations_are_version_checked_and_independent() {
    let mut first = Fixture::new();
    let mut second = Fixture::new();
    let permit = first.permit(16 * 1024 * 1024);
    let before = first.namespace.allocation().unwrap();
    let mut lower = before.clone();
    lower.staging_bytes = 1024;
    let current = first
        .namespace
        .update_allocation(before.version, lower.clone())
        .unwrap();
    assert_eq!(current.version, before.version + 1);
    let error = first
        .namespace
        .update_allocation(before.version, lower)
        .unwrap_err();
    assert_eq!(error.category, ErrorCategory::StaleVersion);
    assert_eq!(
        first.namespace.work_usage().unwrap().reserved_staging_bytes,
        16 * 1024 * 1024
    );
    assert!(second.permit(16 * 1024 * 1024).staging_limit() > 1024);
    drop(permit);
    assert_eq!(first.namespace.work_usage().unwrap().reservations, 0);
}

#[test]
fn managed_checkpoint_is_delta_only_bound_and_restored_not_ready() {
    use tgrep_core::managed::{CurrentPin, Id};
    use tgrep_core::worktrees::{WorktreeOptions, WorktreeView};
    let mut fixture = Fixture::new();
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let permit = fixture.permit(16 * 1024 * 1024);
    let materialization = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let view = WorktreeView::new(
        &fixture.root,
        Arc::clone(&materialization.generation),
        WorktreeOptions::default(),
    )
    .unwrap();
    fs::write(
        fixture.root.join("source.txt"),
        "dirty private replacement\n",
    )
    .unwrap();
    view.refresh_controlled(&permit).unwrap();
    let checkpoint = fixture
        .namespace
        .save_checkpoint(
            &view,
            &Id::new().unwrap(),
            1,
            7,
            &CurrentPin::from_materialization(&materialization),
            &permit,
        )
        .unwrap();
    let restored = fixture
        .namespace
        .restore_checkpoint(
            &checkpoint.descriptor.incarnation,
            &checkpoint.descriptor.binding,
            &fixture.root,
            Arc::clone(&materialization.generation),
            WorktreeOptions::default(),
            &permit,
        )
        .unwrap();
    assert!(!restored.status().unwrap().ready);
    assert_eq!(
        restored
            .refresh_controlled(&permit)
            .unwrap()
            .files_extracted,
        0
    );
    assert_eq!(restored.overlay_cost().unwrap().files, 1);
    assert_eq!(
        fixture
            .namespace
            .reference_counts(&materialization.descriptor.incarnation)
            .unwrap()[0]
            .kind,
        "checkpoint"
    );
    let mut wrong = checkpoint.descriptor.binding.clone();
    wrong.view_version += 1;
    let error = match fixture.namespace.restore_checkpoint(
        &checkpoint.descriptor.incarnation,
        &wrong,
        &fixture.root,
        Arc::clone(&materialization.generation),
        WorktreeOptions::default(),
        &permit,
    ) {
        Ok(_) => panic!("stale checkpoint binding must fail"),
        Err(error) => error,
    };
    assert_eq!(error.category, ErrorCategory::StaleVersion);
}

#[test]
fn escaped_candidate_keeps_the_base_and_root_protected() {
    use tgrep_core::worktrees::{WorktreeOptions, WorktreeView};
    let mut fixture = Fixture::new();
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let permit = fixture.permit(16 * 1024 * 1024);
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let view = WorktreeView::new(
        &fixture.root,
        Arc::clone(&built.generation),
        WorktreeOptions::default(),
    )
    .unwrap();
    view.reconcile_full().unwrap();
    let candidate = view
        .with_snapshot(|snapshot| {
            assert!(snapshot.open_file("source.txt").is_err());
            snapshot.open_candidate("source.txt").unwrap()
        })
        .unwrap();
    let clone = candidate.try_clone().unwrap();
    let guard = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            fixture
                .namespace
                .path()
                .join("guards")
                .join(format!("{}.lock", built.descriptor.incarnation)),
        )
        .unwrap();
    drop(view);
    drop(built);
    drop(candidate);
    assert!(fs2::FileExt::try_lock_exclusive(&guard).is_err());
    #[cfg(windows)]
    assert!(fs::rename(&fixture.root, fixture.temp.path().join("moved-root")).is_err());
    drop(clone);
    fs2::FileExt::try_lock_exclusive(&guard).unwrap();
    #[cfg(windows)]
    fs::rename(&fixture.root, fixture.temp.path().join("moved-root")).unwrap();
}

fn collect(
    fixture: &mut Fixture,
    cursor: Option<tgrep_core::managed::CollectionCursor>,
) -> tgrep_core::managed::CollectionProgress {
    use tgrep_core::managed::{CollectionBounds, CollectionRequest};
    let policy = fixture.namespace.policy().unwrap();
    let request = CollectionRequest {
        policy_version: policy.version,
        allocation_version: fixture.namespace.allocation().unwrap().version,
        bounds: CollectionBounds::from_policy(&policy.policy.collection),
        cursor,
    };
    fixture.sequence += 1;
    let operation = fixture
        .namespace
        .accept_operation(
            OperationToken {
                scope: fixture.owner.registration().owner.clone(),
                sequence: fixture.sequence,
                token: Token::parse(format!("collection-{}", fixture.sequence)).unwrap(),
            },
            "collection",
            serde_json::to_value(&request).unwrap(),
        )
        .unwrap();
    let result = fixture
        .namespace
        .collect_pass(&operation.id, &request)
        .unwrap();
    let replay = fixture
        .namespace
        .collect_pass(&operation.id, &request)
        .unwrap();
    assert_eq!(
        result.logical_bytes_reclaimed,
        replay.logical_bytes_reclaimed
    );
    assert!(result.pages <= request.bounds.max_pages);
    assert!(result.examined <= request.bounds.max_examined);
    assert!(result.logical_bytes_reclaimed <= request.bounds.max_delete_bytes);
    result
}

#[test]
fn checkpoint_withdrawal_precedes_generation_collection_without_stopping_readers() {
    use tgrep_core::managed::{CurrentPin, Id, ObjectState};
    use tgrep_core::worktrees::{WorktreeOptions, WorktreeView};
    let mut fixture = Fixture::new();
    let current = fixture.namespace.policy().unwrap();
    let mut bounded = current.policy;
    bounded.retention = tgrep_core::managed::policy::Retention::Bounded { target_bytes: 1 };
    fixture
        .namespace
        .update_policy(current.version, bounded)
        .unwrap();
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let permit = fixture.permit(16 * 1024 * 1024);
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let view = WorktreeView::new(
        &fixture.root,
        Arc::clone(&built.generation),
        WorktreeOptions::default(),
    )
    .unwrap();
    view.reconcile_full().unwrap();
    let checkpoint = fixture
        .namespace
        .save_checkpoint(
            &view,
            &Id::new().unwrap(),
            1,
            1,
            &CurrentPin::from_materialization(&built),
            &permit,
        )
        .unwrap();
    let checkpoint_id = checkpoint.descriptor.incarnation.clone();
    let generation_id = built.descriptor.incarnation.clone();
    drop(checkpoint);
    drop(permit);
    let mut cursor = None;
    for _ in 0..16 {
        let progress = collect(&mut fixture, cursor);
        cursor = progress.next;
        if fixture.namespace.object(&checkpoint_id).unwrap().state == ObjectState::Removed {
            break;
        }
    }
    assert_eq!(
        fixture.namespace.object(&checkpoint_id).unwrap().state,
        ObjectState::Removed
    );
    assert!(
        fixture
            .namespace
            .reference_counts(&generation_id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fixture.namespace.object(&generation_id).unwrap().state,
        ObjectState::Published
    );
    assert!(
        !fixture
            .namespace
            .eligibility(&generation_id)
            .unwrap()
            .eligible
    );
    assert!(
        view.with_snapshot(|snapshot| snapshot.files("", false))
            .unwrap()
            .contains(&"source.txt".into())
    );
    drop(view);
    drop(built);
    for _ in 0..16 {
        let progress = collect(&mut fixture, cursor);
        cursor = progress.next;
        if fixture.namespace.object(&generation_id).unwrap().state == ObjectState::Removed {
            break;
        }
    }
    assert_eq!(
        fixture.namespace.object(&generation_id).unwrap().state,
        ObjectState::Removed
    );
}

#[test]
fn retained_checkpoints_release_independent_edges_and_stale_preview_cannot_authorize_deletion() {
    use tgrep_core::managed::{CurrentPin, Id, MetadataMutation, ObjectState, OperationState};
    use tgrep_core::worktrees::{WorktreeOptions, WorktreeView};
    let mut fixture = Fixture::new();
    let current = fixture.namespace.policy().unwrap();
    let mut bounded = current.policy;
    bounded.retention = tgrep_core::managed::policy::Retention::Bounded { target_bytes: 1 };
    bounded.collection.max_pages = 64;
    fixture
        .namespace
        .update_policy(current.version, bounded)
        .unwrap();
    let permit = fixture.permit(16 * 1024 * 1024);
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &git(&fixture.root, &["rev-parse", "HEAD"]),
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let view = WorktreeView::new(
        &fixture.root,
        Arc::clone(&built.generation),
        WorktreeOptions::default(),
    )
    .unwrap();
    view.refresh_controlled(&permit).unwrap();
    let view_id = Id::new().unwrap();
    let pin = CurrentPin::from_materialization(&built);
    let first = fixture
        .namespace
        .save_checkpoint(&view, &view_id, 1, 0, &pin, &permit)
        .unwrap();
    fs::write(
        fixture.root.join("private.txt"),
        "private checkpoint needle\n",
    )
    .unwrap();
    view.invalidate_all().unwrap();
    view.refresh_controlled(&permit).unwrap();
    let second = fixture
        .namespace
        .save_checkpoint(&view, &view_id, 2, 1, &pin, &permit)
        .unwrap();
    let first_id = first.descriptor.incarnation.clone();
    let second_id = second.descriptor.incarnation.clone();
    let generation = built.descriptor.incarnation.clone();
    let expected: u64 = [&first_id, &second_id, &generation]
        .into_iter()
        .map(|id| fixture.namespace.object(id).unwrap().logical_bytes)
        .sum();
    let mutate = |fixture: &mut Fixture, mutation: MetadataMutation| {
        let token = fixture.token("checkpoint-reference");
        let operation = fixture
            .namespace
            .accept_operation(token, "metadata", serde_json::to_value(mutation).unwrap())
            .unwrap();
        let completed = fixture
            .namespace
            .execute_metadata_mutation(&operation.id)
            .unwrap();
        assert_eq!(completed.state, OperationState::Completed, "{completed:?}");
        completed.result.unwrap()
    };
    let first_ref: Id = serde_json::from_value(
        mutate(
            &mut fixture,
            MetadataMutation::Retain {
                object: first_id.clone(),
            },
        )["reference"]
            .clone(),
    )
    .unwrap();
    let second_ref: Id = serde_json::from_value(
        mutate(
            &mut fixture,
            MetadataMutation::Retain {
                object: second_id.clone(),
            },
        )["reference"]
            .clone(),
    )
    .unwrap();
    drop((first, second, view, built, permit));
    let references = fixture.namespace.reference_counts(&generation).unwrap();
    assert_eq!(references.len(), 1);
    assert_eq!(references[0].kind, "checkpoint");
    assert_eq!(references[0].count, 2);
    assert_eq!(collect(&mut fixture, None).logical_bytes_reclaimed, 0);
    mutate(
        &mut fixture,
        MetadataMutation::Release {
            reference: first_ref,
        },
    );
    let mut reclaimed = 0;
    let mut cursor = None;
    for _ in 0..16 {
        let progress = collect(&mut fixture, cursor);
        assert_eq!(progress.errors, 0, "{progress:?}");
        reclaimed += progress.logical_bytes_reclaimed;
        cursor = progress.next;
        if fixture.namespace.object(&first_id).unwrap().state == ObjectState::Removed {
            break;
        }
    }
    assert_eq!(
        fixture.namespace.object(&first_id).unwrap().state,
        ObjectState::Removed
    );
    assert_eq!(
        fixture.namespace.object(&generation).unwrap().state,
        ObjectState::Published
    );
    assert_eq!(
        fixture.namespace.reference_counts(&generation).unwrap()[0].count,
        1
    );
    mutate(
        &mut fixture,
        MetadataMutation::Release {
            reference: second_ref,
        },
    );
    let preview = fixture.namespace.preview_collection(None).unwrap();
    assert!(preview.next.is_none());
    assert!(
        preview
            .objects
            .iter()
            .any(|object| object.object == second_id && object.eligible)
    );
    let protection: Id = serde_json::from_value(
        mutate(
            &mut fixture,
            MetadataMutation::Retain {
                object: second_id.clone(),
            },
        )["reference"]
            .clone(),
    )
    .unwrap();
    assert_eq!(collect(&mut fixture, None).logical_bytes_reclaimed, 0);
    assert_eq!(
        fixture.namespace.object(&second_id).unwrap().state,
        ObjectState::Published
    );
    assert_eq!(
        fixture.namespace.object(&generation).unwrap().state,
        ObjectState::Published
    );
    mutate(
        &mut fixture,
        MetadataMutation::Release {
            reference: protection,
        },
    );
    let mut cursor = None;
    for _ in 0..16 {
        let progress = collect(&mut fixture, cursor);
        assert_eq!(progress.errors, 0, "{progress:?}");
        reclaimed += progress.logical_bytes_reclaimed;
        cursor = progress.next;
        if fixture.namespace.object(&generation).unwrap().state == ObjectState::Removed {
            break;
        }
    }
    assert_eq!(
        fixture.namespace.object(&second_id).unwrap().state,
        ObjectState::Removed
    );
    assert_eq!(
        fixture.namespace.object(&generation).unwrap().state,
        ObjectState::Removed
    );
    assert_eq!(reclaimed, expected);
}

#[test]
#[cfg(windows)]
fn native_sharing_and_mapping_failures_stay_pending_without_false_reclaimed_bytes() {
    use std::os::windows::fs::OpenOptionsExt;
    use tgrep_core::managed::ObjectState;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    let mut fixture = Fixture::new();
    let policy = fixture.namespace.policy().unwrap();
    let mut bounded = policy.policy;
    bounded.retention = tgrep_core::managed::policy::Retention::Bounded { target_bytes: 1 };
    bounded.collection.max_pages = 64;
    fixture
        .namespace
        .update_policy(policy.version, bounded)
        .unwrap();
    let permit = fixture.permit(16 * 1024 * 1024);
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &git(&fixture.root, &["rev-parse", "HEAD"]),
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let id = built.descriptor.incarnation.clone();
    let record = fixture.namespace.object(&id).unwrap();
    let directory = fixture.namespace.path().join("objects").join(id.as_str());
    let members: Vec<_> = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let files: Vec<_> = members
        .iter()
        .map(|member| {
            fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(member)
                .unwrap()
        })
        .collect();
    let index = members
        .iter()
        .position(|member| member.file_name().unwrap() == "postings.tgm")
        .unwrap();
    // These native handles deny writes and deletion for the entire mapping lifetime.
    let mapping = unsafe { memmap2::MmapOptions::new().map(&files[index]).unwrap() };
    assert!(!mapping.is_empty());
    drop((built, permit));
    let blocked = collect(&mut fixture, None);
    assert_eq!(blocked.errors, 1, "{blocked:?}");
    assert_eq!(
        blocked.logical_bytes_reclaimed + blocked.recovered_logical_bytes,
        0
    );
    let error = blocked
        .details
        .iter()
        .find_map(|detail| detail.error.as_ref())
        .unwrap();
    assert_eq!(error["category"], "busy");
    assert_eq!(error["reason_code"], "windows-sharing-or-mapping");
    assert_eq!(
        fixture.namespace.object(&id).unwrap().state,
        ObjectState::PendingDeletion
    );
    drop(mapping);
    drop(files);
    let resumed = collect(&mut fixture, None);
    assert_eq!(resumed.errors, 0, "{resumed:?}");
    assert_eq!(
        fixture.namespace.object(&id).unwrap().state,
        ObjectState::Removed
    );
    assert_eq!(
        resumed.logical_bytes_reclaimed + resumed.recovered_logical_bytes,
        record.logical_bytes
    );
    assert!(!directory.exists());
}

#[test]
#[cfg(feature = "managed-test-hooks")]
fn disk_full_during_member_creation_preserves_the_prior_ready_pin_and_conservative_charges() {
    use tgrep_core::managed::faults::{Action, Point, Specification};
    use tgrep_core::managed::{MigrationRequest, OperationState, RefreshRequest};
    let mut fixture = Fixture::new();
    let manager = fixture.manager();
    let original = fixture.attach(&manager);
    fs::write(fixture.root.join("source.txt"), "new committed content\n").unwrap();
    git(&fixture.root, &["commit", "--quiet", "-am", "successor"]);
    let target = git(&fixture.root, &["rev-parse", "HEAD"]);
    manager
        .slot(&original.id)
        .unwrap()
        .invalidate(&[], true)
        .unwrap();
    let refresh = manager
        .accept_refresh(
            fixture.token("refresh"),
            RefreshRequest {
                view: original.id.clone(),
                expected_version: 1,
                owner: fixture.owner.registration().owner.clone(),
                allocation_version: 1,
            },
        )
        .unwrap();
    assert_eq!(
        manager.execute(&refresh.id).unwrap().state,
        OperationState::Completed
    );
    let old = manager.slot(&original.id).unwrap().query(1).unwrap();
    let before = fixture
        .namespace
        .work_usage()
        .unwrap()
        .storage
        .charged_overlap_logical_bytes;
    fixture
        .namespace
        .install_test_fault(Specification {
            point: Point::MemberCreated,
            operation: None,
            skip_hits: 0,
            action: Action::OsError {
                code: if cfg!(windows) { 112 } else { 28 },
            },
        })
        .unwrap();
    let migration = manager
        .accept_migration(
            fixture.token("disk-full"),
            MigrationRequest {
                root: fixture.root.clone(),
                view: original.id.clone(),
                expected_version: 1,
                target_commit: target,
                profile: IndexingProfile::default(),
                owner: fixture.owner.registration().owner.clone(),
                allocation_version: 1,
            },
        )
        .unwrap();
    let failed = manager.execute(&migration.id).unwrap();
    assert_eq!(failed.state, OperationState::Failed, "{failed:?}");
    let error = failed.error.unwrap();
    assert_eq!(error["category"], "resource-pressure");
    assert_eq!(error["reason_code"], "storage-full");
    assert_eq!(error["committed_state"], "not-committed");
    assert_eq!(
        manager.recover(&original.id).unwrap().current,
        original.current
    );
    assert!(manager.status(&original.id).unwrap().ready);
    old.validate().unwrap();
    assert!(
        fixture
            .namespace
            .work_usage()
            .unwrap()
            .storage
            .charged_overlap_logical_bytes
            > before
    );
}

#[test]
#[cfg(feature = "managed-test-hooks")]
fn catalog_errors_do_not_leave_a_busy_idle_stop_with_closed_admission() {
    use tgrep_core::managed::faults::{Action, Point, Specification};
    use tgrep_core::managed::{CommitState, ExternalWork};
    let mut fixture = Fixture::new();
    fixture.namespace.activate().unwrap();
    let manager = fixture.manager();
    let view = fixture.attach(&manager);
    let generation = view.pin().unwrap().incarnation.as_ref().unwrap();
    for skip_hits in 0..=2 {
        fixture
            .namespace
            .install_test_fault(Specification {
                point: Point::CatalogAfterCommit,
                operation: None,
                skip_hits,
                action: Action::Error {
                    category: ErrorCategory::Io,
                },
            })
            .unwrap();
        let token = OperationToken {
            scope: fixture.namespace.header().namespace.clone(),
            sequence: u64::from(skip_hits) + 1,
            token: Token::parse(format!("idle-catalog-error-{skip_hits}")).unwrap(),
        };
        let error = fixture
            .namespace
            .stop_if_idle_with_token(token.clone(), ExternalWork::default())
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::Io);
        assert_eq!(error.committed_state, CommitState::Committed);
        assert!(!fixture.namespace.stop_is_committed().unwrap());
        let reader = fixture.namespace.open_generation(generation).unwrap();
        assert_eq!(reader.base().reader().all_paths(), ["source.txt"]);
        let query = manager.slot(&view.id).unwrap().query(view.version).unwrap();
        query.validate().unwrap();
        match fixture
            .namespace
            .stop_if_idle_with_token(token, ExternalWork::default())
        {
            Ok(outcome) => assert!(!outcome.stopping),
            Err(error) => assert_eq!(error.category, ErrorCategory::Io),
        }
    }
    fixture
        .namespace
        .release_owner(fixture.owner.registration())
        .unwrap();
    manager.drain_released().unwrap();
    drop(fixture.owner);
    fixture
        .namespace
        .install_test_fault(Specification {
            point: Point::IdleCommitted,
            operation: None,
            skip_hits: 0,
            action: Action::Error {
                category: ErrorCategory::Io,
            },
        })
        .unwrap();
    let token = OperationToken {
        scope: fixture.namespace.header().namespace.clone(),
        sequence: 4,
        token: Token::parse("actual-idle-commit").unwrap(),
    };
    let error = fixture
        .namespace
        .stop_if_idle_with_token(token.clone(), ExternalWork::default())
        .unwrap_err();
    assert_eq!(error.committed_state, CommitState::Committed);
    assert!(fixture.namespace.stop_is_committed().unwrap());
    assert_eq!(
        fixture.namespace.prepare_owner().unwrap_err().category,
        ErrorCategory::Busy
    );
    assert!(
        fixture
            .namespace
            .stop_if_idle_with_token(token, ExternalWork::default())
            .unwrap()
            .stopping
    );
}

#[test]
#[cfg(feature = "managed-test-hooks")]
fn atomic_idle_admission_rejects_new_owners_operations_and_independent_readers() {
    use tgrep_core::managed::faults::{Action, Point, Specification, Stage};
    use tgrep_core::managed::{ExternalWork, MetadataMutation};
    let mut fixture = Fixture::new();
    fixture.namespace.activate().unwrap();
    let manager = fixture.manager();
    let record = fixture.attach(&manager);
    let generation = record.pin().unwrap().incarnation.clone().unwrap();
    fixture
        .namespace
        .release_owner(fixture.owner.registration())
        .unwrap();
    manager.drain_released().unwrap();
    drop(fixture.owner);
    for external in [
        ExternalWork {
            queries: 1,
            ..Default::default()
        },
        ExternalWork {
            queued_jobs: 1,
            ..Default::default()
        },
        ExternalWork {
            requests: 1,
            ..Default::default()
        },
        ExternalWork {
            background_batches: 1,
            ..Default::default()
        },
    ] {
        let result = fixture.namespace.stop_if_idle(external).unwrap();
        assert!(!result.stopping, "{result:?}");
    }
    let keep = fixture
        .namespace
        .accept_system_operation(
            Token::parse("persistent-cache-only").unwrap(),
            "metadata",
            serde_json::to_value(MetadataMutation::Retain {
                object: generation.clone(),
            })
            .unwrap(),
        )
        .unwrap();
    fixture
        .namespace
        .execute_metadata_mutation(&keep.id)
        .unwrap();
    let hook = fixture
        .namespace
        .install_test_fault(Specification {
            point: Point::IdleAdmissionClosed,
            operation: None,
            skip_hits: 0,
            action: Action::Pause { timeout_ms: 30000 },
        })
        .unwrap();
    std::thread::scope(|scope| {
        let stop = scope.spawn(|| fixture.namespace.stop_if_idle(ExternalWork::default()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while fixture
            .namespace
            .test_fault_status()
            .unwrap()
            .unwrap()
            .stage
            != Stage::Waiting
        {
            assert!(
                std::time::Instant::now() < deadline,
                "idle admission barrier not reached"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            fixture.namespace.prepare_owner().unwrap_err().category,
            ErrorCategory::Busy
        );
        assert_eq!(
            fixture
                .namespace
                .accept_system_operation(
                    Token::parse("late-work").unwrap(),
                    "attach",
                    serde_json::json!({}),
                )
                .unwrap_err()
                .category,
            ErrorCategory::Busy
        );
        assert_eq!(
            tgrep_core::managed::open_generation(fixture.namespace.path(), &generation)
                .err()
                .unwrap()
                .category,
            ErrorCategory::Busy
        );
        fixture.namespace.release_test_fault(&hook.ticket).unwrap();
        let stopped = stop.join().unwrap().unwrap();
        assert!(
            stopped.stopping,
            "retained cache references are not live work: {stopped:?}"
        );
    });
    assert!(fixture.namespace.prepare_owner().is_err());
}

#[test]
fn controlled_external_sort_spills_and_preserves_shared_blob_postings() {
    let mut fixture = Fixture::new();
    let previous = fixture.namespace.policy().unwrap();
    let mut configured = previous.policy;
    configured.work.sort_buffer_bytes = 64 * 1024;
    fixture
        .namespace
        .update_policy(previous.version, configured)
        .unwrap();
    let mut seed = 0x5eed_u32;
    let mut bytes = b"controlled shared needle\n".to_vec();
    for index in 0..48_000 {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        bytes.push(if index % 73 == 0 {
            b'\n'
        } else {
            b'a' + (seed % 26) as u8
        });
    }
    for index in 0..5 {
        fs::write(fixture.root.join(format!("same-blob-{index}.txt")), &bytes).unwrap();
    }
    git(&fixture.root, &["add", "--all"]);
    git(
        &fixture.root,
        &["commit", "--quiet", "-m", "bounded shared blobs"],
    );
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let permit = fixture.permit(8 * 1024 * 1024);
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    assert_eq!(built.descriptor.stats.blobs_extracted, 2);
    assert_eq!(built.generation.base().reader().all_paths().len(), 6);
    assert!(
        fixture
            .namespace
            .page(None)
            .unwrap()
            .objects
            .iter()
            .any(|object| object.kind == tgrep_core::managed::ObjectKind::BuildStage)
    );
    let masks = tgrep_core::live::LiveIndex::compute_trigram_masks(&bytes);
    for trigram in masks.keys() {
        let paths: Vec<_> = built
            .generation
            .base()
            .reader()
            .lookup_trigram_with_masks(*trigram)
            .into_iter()
            .filter_map(|entry| built.generation.base().reader().file_path(entry.file_id))
            .filter(|path| path.starts_with("same-blob-"))
            .collect();
        assert_eq!(
            paths.len(),
            5,
            "shared blob trigram lost a file during controlled merge"
        );
    }
    assert!(permit.bytes_written() <= permit.staging_limit());
    assert!(permit.peak_private_bytes() <= permit.private_limit());
}

#[test]
#[cfg(feature = "managed-test-hooks")]
fn generation_retirement_races_rebuild_and_attach_without_aliasing_incarnations() {
    use tgrep_core::managed::faults::{Action, Point, Specification, Stage};
    use tgrep_core::managed::{CollectionBounds, CollectionRequest, ObjectState};
    let mut fixture = Fixture::new();
    let configured = fixture.namespace.policy().unwrap();
    let mut policy = configured.policy;
    policy.retention = tgrep_core::managed::policy::Retention::Bounded { target_bytes: 1 };
    policy.collection.max_pages = 64;
    let configured = fixture
        .namespace
        .update_policy(configured.version, policy)
        .unwrap();
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let permit = fixture.permit(16 * 1024 * 1024);
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let old = built.descriptor.incarnation.clone();
    let key = built.descriptor.key.clone();
    let directory = built.generation.directory().to_path_buf();
    drop((built, permit));
    let request = CollectionRequest {
        policy_version: configured.version,
        allocation_version: 1,
        cursor: None,
        bounds: CollectionBounds::from_policy(&configured.policy.collection),
    };
    let token = fixture.token("generation-retirement");
    let operation = fixture
        .namespace
        .accept_operation(token, "collection", serde_json::to_value(&request).unwrap())
        .unwrap();
    let hook = fixture
        .namespace
        .install_test_fault(Specification {
            point: Point::ObjectWithdrawn,
            operation: Some(operation.id.clone()),
            skip_hits: 0,
            action: Action::Pause { timeout_ms: 30000 },
        })
        .unwrap();
    let namespace = Arc::clone(&fixture.namespace);
    std::thread::scope(|scope| {
        let collector = scope.spawn(|| namespace.collect_pass(&operation.id, &request));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while namespace.test_fault_status().unwrap().unwrap().stage != Stage::Waiting {
            assert!(
                std::time::Instant::now() < deadline,
                "generation retirement barrier not reached"
            );
            std::thread::yield_now();
        }
        assert_eq!(namespace.object(&old).unwrap().state, ObjectState::Retired);
        assert!(matches!(
            namespace.open_generation(&old).err().unwrap().category,
            ErrorCategory::CacheEvicted | ErrorCategory::Busy
        ));
        let permit = fixture.permit(16 * 1024 * 1024);
        let replacement = namespace
            .ensure_generation(
                &fixture.repository,
                &commit,
                IndexingProfile::default(),
                None,
                &permit,
            )
            .unwrap();
        assert!(replacement.descriptor.stats.published);
        assert_eq!(replacement.descriptor.key, key);
        assert_ne!(replacement.descriptor.incarnation, old);
        let escaped = namespace
            .open_generation(&replacement.descriptor.incarnation)
            .unwrap();
        drop(permit);
        fs::write(
            fixture.root.join("source.txt"),
            "dirty while the old incarnation is retired\n",
        )
        .unwrap();
        let manager = fixture.manager();
        let view = fixture.attach(&manager);
        assert_eq!(
            view.pin().unwrap().incarnation.as_ref(),
            Some(&replacement.descriptor.incarnation)
        );
        let query = manager.slot(&view.id).unwrap().query(view.version).unwrap();
        let mut candidate = query
            .with_snapshot(|snapshot| snapshot.open_candidate("source.txt"))
            .unwrap()
            .unwrap();
        let mut bytes = String::new();
        std::io::Read::read_to_string(&mut candidate, &mut bytes).unwrap();
        assert_eq!(
            bytes,
            fs::read_to_string(fixture.root.join("source.txt")).unwrap()
        );
        namespace.release_test_fault(&hook.ticket).unwrap();
        let collected = collector.join().unwrap().unwrap();
        assert_eq!(collected.errors, 0, "{collected:?}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut cursor = collected.next;
        while namespace.object(&old).unwrap().state != ObjectState::Removed {
            assert!(
                std::time::Instant::now() < deadline,
                "retirement did not converge"
            );
            let request = CollectionRequest {
                cursor: cursor.take(),
                ..request.clone()
            };
            let token = fixture.token("generation-retirement-resume");
            let operation = namespace
                .accept_operation(token, "collection", serde_json::to_value(&request).unwrap())
                .unwrap();
            let resumed = namespace.collect_pass(&operation.id, &request).unwrap();
            assert_eq!(resumed.errors, 0, "{resumed:?}");
            cursor = resumed.next;
        }
        query.validate().unwrap();
        assert!(!directory.exists());
        assert!(replacement.generation.directory().exists());
        assert_eq!(
            namespace
                .object(&replacement.descriptor.incarnation)
                .unwrap()
                .state,
            ObjectState::Published
        );
        assert_eq!(escaped.base().reader().all_paths(), ["source.txt"]);
        assert_eq!(
            query
                .with_snapshot(|snapshot| snapshot.files("", false))
                .unwrap(),
            ["source.txt"]
        );
    });
}

#[test]
#[cfg(feature = "managed-test-hooks")]
fn checkpoint_retirement_races_restore_save_and_predecessor_build_without_partial_opens() {
    use tgrep_core::managed::faults::{Action, Point, Specification, Stage};
    use tgrep_core::managed::{CollectionBounds, CollectionRequest, CurrentPin, Id, ObjectState};
    use tgrep_core::worktrees::{WorktreeOptions, WorktreeView};
    let mut fixture = Fixture::new();
    let configured = fixture.namespace.policy().unwrap();
    let mut policy = configured.policy;
    policy.retention = tgrep_core::managed::policy::Retention::Bounded { target_bytes: 1 };
    policy.collection.max_pages = 64;
    let configured = fixture
        .namespace
        .update_policy(configured.version, policy)
        .unwrap();
    let permit = fixture.permit(16 * 1024 * 1024);
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &git(&fixture.root, &["rev-parse", "HEAD"]),
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let view = WorktreeView::new(
        &fixture.root,
        Arc::clone(&built.generation),
        WorktreeOptions::default(),
    )
    .unwrap();
    view.refresh_controlled(&permit).unwrap();
    let view_id = Id::new().unwrap();
    let checkpoint = fixture
        .namespace
        .save_checkpoint(
            &view,
            &view_id,
            1,
            0,
            &CurrentPin::from_materialization(&built),
            &permit,
        )
        .unwrap();
    let descriptor = checkpoint.descriptor.clone();
    drop((checkpoint, permit));
    let request = CollectionRequest {
        policy_version: configured.version,
        allocation_version: 1,
        cursor: None,
        bounds: CollectionBounds::from_policy(&configured.policy.collection),
    };
    let token = fixture.token("checkpoint-retirement");
    let operation = fixture
        .namespace
        .accept_operation(token, "collection", serde_json::to_value(&request).unwrap())
        .unwrap();
    let hook = fixture
        .namespace
        .install_test_fault(Specification {
            point: Point::ObjectWithdrawn,
            operation: Some(operation.id.clone()),
            skip_hits: 0,
            action: Action::Pause { timeout_ms: 30000 },
        })
        .unwrap();
    let namespace = Arc::clone(&fixture.namespace);
    std::thread::scope(|scope| {
        let collector = scope.spawn(|| namespace.collect_pass(&operation.id, &request));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while namespace.test_fault_status().unwrap().unwrap().stage != Stage::Waiting {
            assert!(
                std::time::Instant::now() < deadline,
                "retirement barrier not reached"
            );
            std::thread::yield_now();
        }
        let permit = fixture.permit(16 * 1024 * 1024);
        let restore = namespace.restore_checkpoint(
            &descriptor.incarnation,
            &descriptor.binding,
            &fixture.root,
            Arc::clone(&built.generation),
            WorktreeOptions::default(),
            &permit,
        );
        assert!(matches!(
            restore.err().unwrap().category,
            ErrorCategory::CacheEvicted | ErrorCategory::Busy
        ));
        let replacement = namespace
            .save_checkpoint(
                &view,
                &view_id,
                2,
                0,
                &CurrentPin::from_materialization(&built),
                &permit,
            )
            .unwrap();
        fs::write(
            fixture.root.join("source.txt"),
            "successor during checkpoint retirement\n",
        )
        .unwrap();
        git(&fixture.root, &["commit", "--quiet", "-am", "successor"]);
        let successor = namespace
            .ensure_generation(
                &fixture.repository,
                &git(&fixture.root, &["rev-parse", "HEAD"]),
                IndexingProfile::default(),
                Some(&built.generation),
                &permit,
            )
            .unwrap();
        assert!(successor.descriptor.stats.published);
        let escaped = namespace
            .open_generation(&built.descriptor.incarnation)
            .unwrap();
        namespace.release_test_fault(&hook.ticket).unwrap();
        let collected = collector.join().unwrap().unwrap();
        assert_eq!(collected.errors, 0, "{collected:?}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut cursor = collected.next;
        while namespace.object(&descriptor.incarnation).unwrap().state != ObjectState::Removed {
            assert!(
                std::time::Instant::now() < deadline,
                "retirement did not converge"
            );
            let request = CollectionRequest {
                cursor: cursor.take(),
                ..request.clone()
            };
            let token = fixture.token("retirement-resume");
            let operation = namespace
                .accept_operation(token, "collection", serde_json::to_value(&request).unwrap())
                .unwrap();
            let resumed = namespace.collect_pass(&operation.id, &request).unwrap();
            assert_eq!(resumed.errors, 0, "{resumed:?}");
            cursor = resumed.next;
        }
        assert_eq!(
            namespace
                .object(&built.descriptor.incarnation)
                .unwrap()
                .state,
            ObjectState::Published
        );
        assert_eq!(
            namespace
                .object(&replacement.descriptor.incarnation)
                .unwrap()
                .state,
            ObjectState::Published
        );
        assert!(
            escaped
                .base()
                .reader()
                .all_paths()
                .contains(&"source.txt".to_owned())
        );
    });
}

#[cfg(feature = "managed-test-hooks")]
#[test]
fn intermediate_publication_failures_do_not_commit_a_view_migration() {
    use tgrep_core::managed::faults::{Action, Point, Specification};
    use tgrep_core::managed::{CommitState, MigrationRequest, OperationState, RefreshRequest};
    for point in [Point::GenerationPublished, Point::CheckpointPublished] {
        let mut fixture = Fixture::new();
        let manager = fixture.manager();
        let first = fixture.attach(&manager);
        fs::write(
            fixture.root.join("source.txt"),
            "successor committed needle\n",
        )
        .unwrap();
        git(&fixture.root, &["commit", "--quiet", "-am", "successor"]);
        let target = git(&fixture.root, &["rev-parse", "HEAD"]);
        manager
            .slot(&first.id)
            .unwrap()
            .invalidate(&[], true)
            .unwrap();
        let refresh = manager
            .accept_refresh(
                fixture.token("refresh"),
                RefreshRequest {
                    view: first.id.clone(),
                    expected_version: 1,
                    owner: fixture.owner.registration().owner.clone(),
                    allocation_version: 1,
                },
            )
            .unwrap();
        assert_eq!(
            manager.execute(&refresh.id).unwrap().state,
            OperationState::Completed
        );
        let original = manager.recover(&first.id).unwrap();
        let request = MigrationRequest {
            view: first.id.clone(),
            root: fixture.root.clone(),
            expected_version: 1,
            target_commit: target,
            profile: IndexingProfile::default(),
            owner: fixture.owner.registration().owner.clone(),
            allocation_version: 1,
        };
        let operation = manager
            .accept_migration(fixture.token("migration"), request)
            .unwrap();
        fixture
            .namespace
            .install_test_fault(Specification {
                point,
                operation: Some(operation.id.clone()),
                skip_hits: 0,
                action: Action::Error {
                    category: ErrorCategory::Io,
                },
            })
            .unwrap();
        let failed = manager.execute(&operation.id).unwrap();
        assert_eq!(
            failed.state,
            OperationState::Failed,
            "{point:?}: {failed:?}"
        );
        assert_eq!(
            failed.committed_state,
            CommitState::NotCommitted,
            "{point:?}"
        );
        assert_eq!(failed.error.unwrap()["committed_state"], "not-committed");
        let current = manager.recover(&first.id).unwrap();
        assert_eq!(current.version, original.version);
        assert_eq!(current.current, original.current);
        assert!(manager.status(&first.id).unwrap().ready);
        assert_eq!(fixture.namespace.work_usage().unwrap().reservations, 0);
    }
}

#[cfg(feature = "managed-test-hooks")]
#[test]
fn view_commit_boundaries_preserve_exact_authority_and_replay() {
    use tgrep_core::managed::faults::{Action, Point, Specification};
    use tgrep_core::managed::{CommitState, MigrationRequest, OperationState, RefreshRequest};
    for point in [
        Point::ViewBeforeCommit,
        Point::ViewAfterCommit,
        Point::ViewAfterSwap,
    ] {
        let mut fixture = Fixture::new();
        let manager = fixture.manager();
        let first = fixture.attach(&manager);
        fs::write(
            fixture.root.join("source.txt"),
            "successor committed needle\n",
        )
        .unwrap();
        git(&fixture.root, &["commit", "--quiet", "-am", "successor"]);
        let target = git(&fixture.root, &["rev-parse", "HEAD"]);
        manager
            .slot(&first.id)
            .unwrap()
            .invalidate(&[], true)
            .unwrap();
        let refresh = manager
            .accept_refresh(
                fixture.token("refresh"),
                RefreshRequest {
                    view: first.id.clone(),
                    expected_version: 1,
                    owner: fixture.owner.registration().owner.clone(),
                    allocation_version: 1,
                },
            )
            .unwrap();
        assert_eq!(
            manager.execute(&refresh.id).unwrap().state,
            OperationState::Completed
        );
        let old_query = manager.slot(&first.id).unwrap().query(1).unwrap();
        let mut old_candidate = old_query
            .with_snapshot(|snapshot| snapshot.open_candidate("source.txt"))
            .unwrap()
            .unwrap();
        let token = fixture.token("migration");
        let request = MigrationRequest {
            view: first.id.clone(),
            root: fixture.root.clone(),
            expected_version: 1,
            target_commit: target.clone(),
            profile: IndexingProfile::default(),
            owner: fixture.owner.registration().owner.clone(),
            allocation_version: 1,
        };
        let operation = manager
            .accept_migration(token.clone(), request.clone())
            .unwrap();
        fixture
            .namespace
            .install_test_fault(Specification {
                point,
                operation: Some(operation.id.clone()),
                skip_hits: 0,
                action: Action::Error {
                    category: ErrorCategory::Io,
                },
            })
            .unwrap();
        let completed = manager.execute(&operation.id).unwrap();
        let committed = point != Point::ViewBeforeCommit;
        assert_eq!(
            completed.committed_state,
            if committed {
                CommitState::Committed
            } else {
                CommitState::NotCommitted
            }
        );
        let current = manager.recover(&first.id).unwrap();
        assert_eq!(current.version, 1 + u64::from(committed));
        assert_eq!(
            &current.pin().unwrap().commit,
            if committed {
                &target
            } else {
                &first.pin().unwrap().commit
            }
        );
        assert_eq!(
            manager.status(&first.id).unwrap().ready,
            point != Point::ViewAfterCommit
        );
        let replay = manager.accept_migration(token, request).unwrap();
        assert_eq!(replay.id, operation.id);
        assert_eq!(
            manager.execute(&replay.id).unwrap().result,
            completed.result
        );
        fs::write(
            fixture.root.join("after-failure.txt"),
            "private recovery input\n",
        )
        .unwrap();
        manager
            .invalidate(
                &first.id,
                &fixture.owner.registration().owner,
                current.version,
                &[PathBuf::from("after-failure.txt")],
                false,
            )
            .unwrap();
        let repair = manager
            .accept_refresh(
                fixture.token("repair-committed-publication"),
                RefreshRequest {
                    view: first.id.clone(),
                    expected_version: current.version,
                    owner: fixture.owner.registration().owner.clone(),
                    allocation_version: 1,
                },
            )
            .unwrap();
        let repaired = manager.execute(&repair.id).unwrap();
        assert_eq!(
            repaired.state,
            OperationState::Completed,
            "{point:?}: {repaired:?}"
        );
        assert_eq!(manager.recover(&first.id).unwrap().current, current.current);
        assert!(manager.status(&first.id).unwrap().ready);
        let query = manager
            .slot(&first.id)
            .unwrap()
            .query(current.version)
            .unwrap();
        drop(
            query
                .with_snapshot(|snapshot| snapshot.open_candidate("after-failure.txt"))
                .unwrap()
                .unwrap(),
        );
        query.complete().unwrap();
        let mut bytes = String::new();
        std::io::Read::read_to_string(&mut old_candidate, &mut bytes).unwrap();
        assert_eq!(bytes, "successor committed needle\n");
        drop((old_candidate, old_query));
        if committed {
            assert_eq!(completed.state, OperationState::Completed);
            assert_eq!(
                fixture
                    .namespace
                    .cancel_operation(&operation.id)
                    .unwrap()
                    .state,
                OperationState::Completed
            );
        }
    }
}

#[cfg(feature = "managed-test-hooks")]
#[test]
fn first_attach_and_same_version_refresh_recover_their_committed_publications() {
    use tgrep_core::managed::faults::{Action, Point, Specification};
    use tgrep_core::managed::{CommitState, OperationState, RefreshRequest};
    for initial_attach in [true, false] {
        let mut fixture = Fixture::new();
        let manager = fixture.manager();
        let install = |namespace: &Namespace| {
            namespace
                .install_test_fault(Specification {
                    point: Point::ViewAfterCommit,
                    operation: None,
                    skip_hits: 0,
                    action: Action::Error {
                        category: ErrorCategory::Io,
                    },
                })
                .unwrap()
        };
        if initial_attach {
            install(&fixture.namespace);
        }
        let first = fixture.attach(&manager);
        let owner = fixture.owner.registration().owner.clone();
        if !initial_attach {
            fs::write(
                fixture.root.join("source.txt"),
                "uncommitted refresh input\n",
            )
            .unwrap();
            manager
                .invalidate(&first.id, &owner, first.version, &[], true)
                .unwrap();
            install(&fixture.namespace);
            let failed = manager
                .accept_refresh(
                    fixture.token("same-version-commit"),
                    RefreshRequest {
                        view: first.id.clone(),
                        owner: owner.clone(),
                        expected_version: first.version,
                        allocation_version: 1,
                    },
                )
                .unwrap();
            let failed = manager.execute(&failed.id).unwrap();
            assert_eq!(failed.state, OperationState::Completed);
            assert_eq!(failed.committed_state, CommitState::Committed);
        }
        let durable = manager.recover(&first.id).unwrap();
        assert_eq!(durable.version, first.version);
        assert_eq!(durable.current, first.current);
        if !initial_attach {
            assert_ne!(durable.checkpoint, first.checkpoint);
        }
        assert!(!manager.status(&first.id).unwrap().ready);
        let repair = manager
            .accept_refresh(
                fixture.token("restore-committed-view"),
                RefreshRequest {
                    view: first.id.clone(),
                    owner,
                    expected_version: durable.version,
                    allocation_version: 1,
                },
            )
            .unwrap();
        let repaired = manager.execute(&repair.id).unwrap();
        assert_eq!(repaired.state, OperationState::Completed, "{repaired:?}");
        assert_eq!(manager.recover(&first.id).unwrap().current, durable.current);
        assert!(manager.status(&first.id).unwrap().ready);
        manager
            .slot(&first.id)
            .unwrap()
            .query(durable.version)
            .unwrap()
            .complete()
            .unwrap();
    }
}

#[cfg(feature = "managed-test-hooks")]
#[test]
fn unlink_before_credit_is_recovered_once_and_cannot_remove_a_new_incarnation() {
    use tgrep_core::managed::faults::{Action, Point, Specification};
    use tgrep_core::managed::{
        CollectionBounds, CollectionProgress, CollectionRequest, CommitState, ObjectState,
    };
    let mut fixture = Fixture::new();
    let previous = fixture.namespace.policy().unwrap();
    let mut configured = previous.policy;
    configured.retention = tgrep_core::managed::policy::Retention::Bounded { target_bytes: 1 };
    configured.collection.max_pages = 64;
    fixture
        .namespace
        .update_policy(previous.version, configured)
        .unwrap();
    let commit = git(&fixture.root, &["rev-parse", "HEAD"]);
    let permit = fixture.permit(8 * 1024 * 1024);
    let built = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    let old = built.descriptor.incarnation.clone();
    let key = built.descriptor.key.clone();
    let before = fixture.namespace.object(&old).unwrap().logical_bytes;
    let directory = built.generation.directory().to_path_buf();
    drop((built, permit));
    let policy = fixture.namespace.policy().unwrap();
    let request = CollectionRequest {
        policy_version: policy.version,
        allocation_version: 1,
        bounds: CollectionBounds::from_policy(&policy.policy.collection),
        cursor: None,
    };
    let collection_token = fixture.token("collection");
    let operation = fixture
        .namespace
        .accept_operation(
            collection_token,
            "collection",
            serde_json::to_value(&request).unwrap(),
        )
        .unwrap();
    fixture
        .namespace
        .install_test_fault(Specification {
            point: Point::MemberBeforeCredit,
            operation: Some(operation.id.clone()),
            skip_hits: 0,
            action: Action::Error {
                category: ErrorCategory::Io,
            },
        })
        .unwrap();
    let error = fixture
        .namespace
        .collect_pass(&operation.id, &request)
        .unwrap_err();
    assert_eq!(error.reason_code, "collection-journal-update");
    assert_eq!(error.committed_state, CommitState::Committed);
    assert!(!directory.join("generation.tgm").exists());
    let interrupted: CollectionProgress =
        serde_json::from_value(fixture.namespace.operation(&operation.id).unwrap().progress)
            .unwrap();
    assert_eq!(interrupted.logical_bytes_reclaimed, 0);
    assert_eq!(interrupted.recovered_logical_bytes, 0);
    assert_eq!(interrupted.elapsed_nanos, None);
    let completed = fixture
        .namespace
        .collect_pass(&operation.id, &request)
        .unwrap();
    assert!(completed.recovered_logical_bytes > 0);
    assert_eq!(
        completed.logical_bytes_reclaimed + completed.recovered_logical_bytes,
        before
    );
    assert_eq!(
        fixture.namespace.object(&old).unwrap().state,
        ObjectState::Removed
    );
    let replay = fixture
        .namespace
        .collect_pass(&operation.id, &request)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&completed).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    let permit = fixture.permit(8 * 1024 * 1024);
    let replacement = fixture
        .namespace
        .ensure_generation(
            &fixture.repository,
            &commit,
            IndexingProfile::default(),
            None,
            &permit,
        )
        .unwrap();
    assert_eq!(replacement.descriptor.key, key);
    assert_ne!(replacement.descriptor.incarnation, old);
    fixture
        .namespace
        .collect_pass(&operation.id, &request)
        .unwrap();
    assert!(replacement.generation.directory().exists());
    assert_eq!(
        replacement.generation.base().reader().all_paths(),
        ["source.txt"]
    );
}
