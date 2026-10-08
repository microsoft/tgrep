// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::{object_row, save_object, sql_integer, text};
use super::clock::{IdleEvidence, advance_idle};
use super::lifetime::{ActivityGuard, lock_error};
use super::storage::{Directory, allocated_bytes};
use super::{
    CatalogCursor, CommitState, Error, ErrorCategory, FileIdentity, FileRecord, Id, Measurement,
    Namespace, ObjectKind, ObjectRecord, ObjectState, OperationRecord, OperationState, Result,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceCount {
    pub kind: String,
    pub count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed::faults::{Action, Point, Specification};
    use crate::managed::{Token, WorkRequest};
    use std::io::Write;

    fn pass(namespace: &Arc<Namespace>, name: &str) -> CollectionProgress {
        let policy = namespace.policy().unwrap();
        let request = CollectionRequest {
            policy_version: policy.version,
            allocation_version: namespace.allocation().unwrap().version,
            bounds: CollectionBounds::from_policy(&policy.policy.collection),
            cursor: None,
        };
        let operation = namespace
            .accept_system_operation(
                Token::parse(name).unwrap(),
                "collection",
                serde_json::to_value(&request).unwrap(),
            )
            .unwrap();
        namespace.collect_pass(&operation.id, &request).unwrap()
    }

    #[test]
    fn single_page_passes_resume_member_deletion_without_spending_the_page_on_a_cached_id() {
        let temp = tempfile::tempdir().unwrap();
        let mut policy = crate::managed::policy::fixture_policy();
        policy.retention = crate::managed::policy::Retention::Bounded { target_bytes: 1 };
        policy.collection.chunk_bytes = 4096;
        let namespace =
            Namespace::initialize_identity(&"a".repeat(64), temp.path(), policy).unwrap();
        namespace.activate().unwrap();
        let build = namespace
            .accept_system_operation(
                Token::parse("single-page-input").unwrap(),
                "build",
                serde_json::json!({}),
            )
            .unwrap();
        let permit = namespace
            .reserve(
                &build.id,
                None,
                WorkRequest {
                    allocation_version: 1,
                    staging_bytes: 1024 * 1024,
                    private_bytes: 1024 * 1024,
                    slots: 1,
                },
            )
            .unwrap();
        let (object, pin) = namespace
            .create_object(ObjectKind::Generation, None, &permit)
            .unwrap();
        let mut writer = crate::managed::work::ChargedWriter::new(
            Arc::clone(&pin),
            "payload.bin",
            Arc::clone(&permit),
        )
        .unwrap();
        writer.write_all(&vec![b'x'; 10 * 1024]).unwrap();
        writer.sync_all().unwrap();
        drop(writer);
        namespace.publish_object(&object.id, None, None).unwrap();
        let bytes = namespace.object(&object.id).unwrap().logical_bytes;
        drop((pin, permit));
        let mut request = CollectionRequest {
            policy_version: 1,
            allocation_version: 1,
            bounds: CollectionBounds {
                max_duration_ms: 1000,
                max_examined: 1,
                max_removed: 1,
                max_delete_bytes: 4096,
                max_pages: 1,
            },
            cursor: None,
        };
        let mut reclaimed = 0;
        let mut removed = 0;
        for sequence in 0..32 {
            let operation = namespace
                .accept_system_operation(
                    Token::parse(format!("single-page-{sequence}")).unwrap(),
                    "collection",
                    serde_json::to_value(&request).unwrap(),
                )
                .unwrap();
            let progress = namespace.collect_pass(&operation.id, &request).unwrap();
            assert!(progress.pages <= 1 && progress.examined <= 1 && progress.removed <= 1);
            assert!(progress.logical_bytes_reclaimed <= 4096);
            assert_eq!(progress.errors, 0, "{progress:?}");
            reclaimed += progress.logical_bytes_reclaimed;
            removed += progress.removed;
            request.cursor = progress.next;
            if removed != 0 {
                break;
            }
        }
        assert_eq!(removed, 1, "a valid one-page budget must make progress");
        assert_eq!(reclaimed, bytes);
        assert_eq!(
            namespace.object(&object.id).unwrap().state,
            ObjectState::Removed
        );
        assert!(
            !namespace
                .path()
                .join("objects")
                .join(object.id.as_str())
                .exists()
        );
    }

    #[test]
    fn collection_requires_new_proven_grace_after_clock_rollback_restart_or_missing_age() {
        use crate::managed::clock::{Clock, Stamp};
        use std::sync::Mutex;
        struct TestClock(Mutex<Stamp>);
        impl Clock for TestClock {
            fn now(&self) -> Result<Stamp> {
                Ok(self.0.lock().unwrap().clone())
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let mut policy = crate::managed::policy::fixture_policy();
        policy.retention = crate::managed::policy::Retention::Bounded { target_bytes: 1 };
        policy.collection.generation_grace_ms = 1000;
        policy.collection.max_pages = 64;
        let mut namespace =
            Namespace::initialize_identity(&"a".repeat(64), temp.path(), policy).unwrap();
        let clock = Arc::new(TestClock(Mutex::new(Stamp {
            boot: "a".into(),
            millis: 0,
            wall_millis: 10000,
        })));
        Arc::get_mut(&mut namespace).unwrap().clock = clock.clone();
        namespace.activate().unwrap();
        let operation = namespace
            .accept_system_operation(
                Token::parse("generation").unwrap(),
                "build",
                serde_json::json!({}),
            )
            .unwrap();
        let permit = namespace
            .reserve(
                &operation.id,
                None,
                WorkRequest {
                    allocation_version: 1,
                    staging_bytes: 1048576,
                    private_bytes: 1048576,
                    slots: 1,
                },
            )
            .unwrap();
        let (object, pin) = namespace
            .create_object(ObjectKind::Generation, None, &permit)
            .unwrap();
        namespace.publish_object(&object.id, None, None).unwrap();
        drop((pin, permit));
        for (index, boot, millis, wall) in [
            (0, "a", 0, 10000),
            (1, "a", 999, 10999),
            (2, "a", 1000, 9000),
            (3, "a", 1999, 9999),
            (4, "b", 2000, 11000),
            (5, "b", 2999, 11999),
        ] {
            *clock.0.lock().unwrap() = Stamp {
                boot: boot.into(),
                millis,
                wall_millis: wall,
            };
            let progress = pass(&namespace, &format!("grace-{index}"));
            assert_eq!(progress.logical_bytes_reclaimed, 0, "{progress:?}");
            assert_eq!(
                namespace.object(&object.id).unwrap().state,
                ObjectState::Published
            );
        }
        namespace
            .transaction(|transaction| {
                let mut object = object_row(transaction, &object.id)?;
                object.idle_evidence = None;
                save_object(transaction, &mut object)
            })
            .unwrap();
        *clock.0.lock().unwrap() = Stamp {
            boot: "b".into(),
            millis: 5000,
            wall_millis: 14000,
        };
        assert_eq!(pass(&namespace, "missing-age").logical_bytes_reclaimed, 0);
        *clock.0.lock().unwrap() = Stamp {
            boot: "b".into(),
            millis: 6000,
            wall_millis: 15000,
        };
        let expired = pass(&namespace, "proved-grace");
        assert_eq!(expired.errors, 0, "{expired:?}");
        assert_eq!(
            namespace.object(&object.id).unwrap().state,
            ObjectState::Removed
        );
    }

    #[test]
    fn lru_traversal_excludes_new_and_moving_objects_without_losing_its_selected_order() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = Namespace::initialize_identity(
            &"a".repeat(64),
            temp.path(),
            crate::managed::policy::fixture_policy(),
        )
        .unwrap();
        namespace.activate().unwrap();
        let operation = namespace
            .accept_system_operation(
                Token::parse("stages").unwrap(),
                "build",
                serde_json::json!({}),
            )
            .unwrap();
        let permit = namespace
            .reserve(
                &operation.id,
                None,
                WorkRequest {
                    allocation_version: 1,
                    staging_bytes: 1024 * 1024,
                    private_bytes: 1024 * 1024,
                    slots: 1,
                },
            )
            .unwrap();
        let mut objects = Vec::new();
        for _ in 0..3 {
            let (object, pin) = namespace
                .create_object(ObjectKind::BuildStage, None, &permit)
                .unwrap();
            namespace.publish_object(&object.id, None, None).unwrap();
            objects.push((object, pin));
        }
        let revision = namespace
            .read(|connection| {
                Ok(connection.query_row(
                    "SELECT revision FROM state WHERE singleton=1",
                    [],
                    |row| super::super::catalog::unsigned(row, 0),
                )?)
            })
            .unwrap();
        let mut cursor = CollectionCursor {
            phase: 2,
            through_use: Some(revision),
            ..Default::default()
        };
        let first = namespace
            .next_collection_object(&mut cursor)
            .unwrap()
            .unwrap();
        assert_eq!(first, objects[0].0.id);
        let selected_use = cursor.current_use.unwrap();
        namespace
            .transaction(|transaction| {
                super::super::catalog::touch_object(transaction, &first)?;
                super::super::catalog::touch_object(transaction, &objects[1].0.id)
            })
            .unwrap();
        objects.push(
            namespace
                .create_object(ObjectKind::BuildStage, None, &permit)
                .unwrap(),
        );
        assert!(namespace.object(&first).unwrap().use_sequence > revision);
        cursor.after = Some(first);
        cursor.after_use = cursor.current_use.take().unwrap();
        assert_eq!(cursor.after_use, selected_use);
        let last = namespace
            .next_collection_object(&mut cursor)
            .unwrap()
            .unwrap();
        assert_eq!(last, objects[2].0.id);
        cursor.after = Some(last);
        cursor.after_use = cursor.current_use.take().unwrap();
        assert!(
            namespace
                .next_collection_object(&mut cursor)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pending_intent_preserves_unexpected_mutations_and_recovers_only_its_exact_truncation() {
        for changed_length in [8192, 8193, 4095, 0, 4096] {
            let temp = tempfile::tempdir().unwrap();
            let mut policy = crate::managed::policy::fixture_policy();
            policy.collection.chunk_bytes = 4096;
            policy.collection.max_pages = 64;
            let namespace =
                Namespace::initialize_identity(&"a".repeat(64), temp.path(), policy).unwrap();
            namespace.activate().unwrap();
            let operation = namespace
                .accept_system_operation(
                    Token::parse("stage").unwrap(),
                    "build",
                    serde_json::json!({}),
                )
                .unwrap();
            let permit = namespace
                .reserve(
                    &operation.id,
                    None,
                    WorkRequest {
                        allocation_version: 1,
                        staging_bytes: 1024 * 1024,
                        private_bytes: 1024 * 1024,
                        slots: 1,
                    },
                )
                .unwrap();
            let (object, pin) = namespace
                .create_object(ObjectKind::BuildStage, None, &permit)
                .unwrap();
            let mut writer = super::super::work::ChargedWriter::new(
                Arc::clone(&pin),
                "payload.bin",
                Arc::clone(&permit),
            )
            .unwrap();
            writer.write_all(&vec![b'a'; 8192]).unwrap();
            writer.sync_all().unwrap();
            drop((writer, permit, pin));
            namespace
                .transaction(|transaction| {
                    let mut object = object_row(transaction, &object.id)?;
                    object.state = ObjectState::Retired;
                    save_object(transaction, &mut object)
                })
                .unwrap();
            namespace
                .install_test_fault(Specification {
                    point: Point::MemberIntentSaved,
                    operation: None,
                    skip_hits: 0,
                    action: Action::Error {
                        category: ErrorCategory::Io,
                    },
                })
                .unwrap();
            let interrupted = pass(&namespace, "intent");
            assert_eq!(interrupted.errors, 1, "{interrupted:?}");
            assert_eq!(interrupted.logical_bytes_reclaimed, 0);
            let path = namespace
                .path()
                .join("objects")
                .join(object.id.as_str())
                .join("payload.bin");
            if changed_length == 4096 {
                let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                file.set_len(changed_length).unwrap();
                file.sync_all().unwrap();
            } else {
                std::fs::write(&path, vec![b'b'; changed_length as usize]).unwrap();
            }
            let resumed = pass(&namespace, "resume");
            if changed_length == 4096 {
                assert_eq!(resumed.errors, 0, "{resumed:?}");
                assert_eq!(resumed.recovered_logical_bytes, 4096);
                assert_eq!(
                    namespace.object(&object.id).unwrap().state,
                    ObjectState::Removed
                );
            } else {
                assert_eq!(
                    resumed.logical_bytes_reclaimed + resumed.recovered_logical_bytes,
                    0
                );
                assert_eq!(resumed.errors, 1, "{resumed:?}");
                assert_eq!(resumed.details[0].reasons, ["member-modified"]);
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    vec![b'b'; changed_length as usize]
                );
                assert_eq!(
                    namespace.object(&object.id).unwrap().state,
                    ObjectState::PendingDeletion
                );
            }
        }
    }

    #[test]
    fn collection_progress_is_not_published_from_a_rolled_back_transaction() {
        for point in [Point::CatalogBeforeCommit, Point::CatalogAfterCommit] {
            let temp = tempfile::tempdir().unwrap();
            let namespace = Namespace::initialize_identity(
                &"a".repeat(64),
                temp.path(),
                crate::managed::policy::fixture_policy(),
            )
            .unwrap();
            namespace.activate().unwrap();
            let operation = namespace
                .accept_system_operation(
                    crate::managed::Token::parse("journal-test").unwrap(),
                    "collection",
                    serde_json::json!({}),
                )
                .unwrap();
            let mut progress = CollectionProgress::default();
            namespace
                .install_test_fault(Specification {
                    point,
                    operation: None,
                    skip_hits: 0,
                    action: Action::Error {
                        category: ErrorCategory::Io,
                    },
                })
                .unwrap();
            let error = namespace
                .collection_transaction(&operation.id, &mut progress, |_, next| {
                    next.pages = 1;
                    Ok(())
                })
                .unwrap_err();
            assert_eq!(error.reason_code, "collection-journal-update");
            assert_eq!(
                progress.pages, 0,
                "failed return must not publish unconfirmed in-memory progress"
            );
            let receipt = namespace.operation(&operation.id).unwrap();
            if point == Point::CatalogBeforeCommit {
                assert_eq!(receipt.progress, serde_json::json!({}));
                assert_eq!(receipt.committed_state, CommitState::NotCommitted);
            } else {
                let saved: CollectionProgress = serde_json::from_value(receipt.progress).unwrap();
                assert_eq!(
                    saved.pages, 1,
                    "postcommit failure retains exactly the durable progress"
                );
                assert_eq!(receipt.committed_state, CommitState::Committed);
                assert_eq!(
                    saved.elapsed_nanos, None,
                    "an interrupted pass has no invented timing"
                );
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Eligibility {
    pub object: Id,
    pub state: ObjectState,
    pub eligible: bool,
    pub reasons: Vec<String>,
    pub references: Vec<ReferenceCount>,
    pub logical_bytes: u64,
    pub allocated_bytes: Measurement<u64>,
    pub idle_millis: Measurement<u64>,
}

#[derive(Debug, Serialize)]
pub struct CollectionPreview {
    pub policy_version: u64,
    pub allocation_version: u64,
    pub catalog_revision: u64,
    pub examined: usize,
    pub objects: Vec<Eligibility>,
    pub next: Option<CatalogCursor>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionBounds {
    pub max_duration_ms: u64,
    pub max_examined: u32,
    pub max_removed: u32,
    pub max_delete_bytes: u64,
    pub max_pages: u32,
}

impl CollectionBounds {
    pub fn from_policy(policy: &super::policy::CollectionPolicy) -> Self {
        Self {
            max_duration_ms: policy.max_duration_ms,
            max_examined: policy.max_examined,
            max_removed: policy.max_removed,
            max_delete_bytes: policy.max_delete_bytes,
            max_pages: policy.max_pages,
        }
    }
    fn validate(&self, policy: &super::policy::CollectionPolicy) -> Result<()> {
        for (requested, configured) in [
            (self.max_duration_ms, policy.max_duration_ms),
            (u64::from(self.max_examined), u64::from(policy.max_examined)),
            (u64::from(self.max_removed), u64::from(policy.max_removed)),
            (self.max_delete_bytes, policy.max_delete_bytes),
            (u64::from(self.max_pages), u64::from(policy.max_pages)),
        ] {
            if requested == 0 || requested > configured {
                return Err(Error::invalid(
                    "collection bounds must be positive and not exceed effective policy",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionCursor {
    pub phase: u8,
    pub after: Option<Id>,
    pub after_use: u64,
    pub current: Option<Id>,
    #[serde(default)]
    pub current_use: Option<u64>,
    #[serde(default)]
    pub through_use: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionRequest {
    pub policy_version: u64,
    pub allocation_version: u64,
    pub bounds: CollectionBounds,
    pub cursor: Option<CollectionCursor>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionSkip {
    pub object: Id,
    pub reasons: Vec<String>,
    pub error: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionProgress {
    pub examined: u32,
    pub retired: u32,
    pub removed: u32,
    pub recovered_objects: u32,
    pub pages: u32,
    pub logical_bytes_reclaimed: u64,
    pub allocated_bytes_reclaimed: u64,
    pub recovered_logical_bytes: u64,
    pub recovered_allocated_bytes: u64,
    pub allocation_measurement_unavailable: bool,
    pub elapsed_nanos: Option<u64>,
    pub elapsed_budget_exceeded: Option<bool>,
    pub skipped: u32,
    pub errors: u32,
    pub omitted_skip_details: u32,
    pub details: Vec<CollectionSkip>,
    pub next: Option<CollectionCursor>,
    pub cancelled: bool,
}

fn exclusive_guard(namespace: &Namespace, object: &ObjectRecord) -> Result<File> {
    let expected = object
        .guard_identity
        .as_ref()
        .ok_or_else(|| Error::corrupt("object has no sealed guard identity"))?;
    let guard = namespace
        .guards
        .open_file(&format!("{}.lock", object.id), true)?;
    if &FileIdentity::of(&guard)? != expected {
        return Err(Error::corrupt("object guard was replaced"));
    }
    fs2::FileExt::try_lock_exclusive(&guard)
        .map_err(|error| lock_error(error, "object-readers-active"))?;
    Ok(guard)
}

fn checked_directory(namespace: &Namespace, object: &ObjectRecord) -> Result<Arc<Directory>> {
    let expected = object
        .directory_identity
        .as_ref()
        .ok_or_else(|| Error::corrupt("object directory identity is unsealed"))?;
    let directory = namespace.objects.child(object.id.as_str())?;
    if &directory.identity()? != expected {
        return Err(Error::new(
            ErrorCategory::StaleIdentity,
            "object-directory-replaced",
            "refusing a different physical container",
        ));
    }
    Ok(directory)
}

impl Namespace {
    pub fn reference_counts(&self, id: &Id) -> Result<Vec<ReferenceCount>> {
        self.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT source_kind,count(*) FROM refs WHERE target=?1 GROUP BY source_kind",
            )?;
            let rows = statement.query_map([id.as_str()], |row| {
                Ok(ReferenceCount {
                    kind: row.get(0)?,
                    count: super::catalog::unsigned(row, 1)?,
                })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    fn eligibility_locked(&self, object: &ObjectRecord, record_age: bool) -> Result<Eligibility> {
        let policy = self.policy()?.policy;
        let references = self.reference_counts(&object.id)?;
        let mut result = Eligibility {
            object: object.id.clone(),
            state: object.state,
            eligible: false,
            reasons: Vec::new(),
            references,
            logical_bytes: object.logical_bytes,
            allocated_bytes: object.allocated_bytes.clone(),
            idle_millis: Measurement::Unavailable {
                reason: "not-observed-inactive".into(),
            },
        };
        if object.state == ObjectState::Removed {
            result.reasons.push("already-removed".into());
            return Ok(result);
        }
        if object.state == ObjectState::Quarantined {
            result.reasons.push("quarantined-identity".into());
        }
        if !result.references.is_empty() {
            result.reasons.push("catalog-references".into());
        }
        if let Some(operation) = &object.operation {
            let reserved: bool = self.read(|connection| {
                Ok(connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM reservations WHERE operation_id=?1)",
                    [operation.as_str()],
                    |row| row.get(0),
                )?)
            })?;
            if reserved {
                result.reasons.push("producer-reservation-active".into());
            }
        }
        let unsealed: bool = self.read(|connection| Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM member_creations WHERE object_id=?1)
             OR EXISTS(SELECT 1 FROM members WHERE object_id=?1 AND
               (json_extract(record,'$.producer_open')=1 OR json_extract(record,'$.seal.format') IS NOT ?2))",
            params![object.id.as_str(), super::authentication::FORMAT], |row| row.get(0),
        )?))?;
        if unsealed {
            result.reasons.push("producer-inventory-incomplete".into());
        }
        if object.state == ObjectState::Published
            && matches!(policy.retention, super::policy::Retention::RetainAll)
        {
            result.reasons.push("retain-all-policy".into());
        }
        if object.state == ObjectState::Preparing {
            match object
                .operation
                .as_ref()
                .map(|id| self.operation(id))
                .transpose()?
            {
                Some(operation)
                    if matches!(
                        operation.state,
                        OperationState::Completed
                            | OperationState::Cancelled
                            | OperationState::Failed
                    ) => {}
                _ => {
                    match object
                        .owner
                        .as_ref()
                        .map(|id| self.inspect_owner(id))
                        .transpose()?
                    {
                        Some(owner)
                            if owner.registered && owner.last_proof == super::OwnerProof::Ended => {
                        }
                        Some(owner) if owner.released => {}
                        _ => result
                            .reasons
                            .push("preparation-owner-not-proven-ended".into()),
                    }
                }
            }
        }
        if !result.reasons.is_empty() {
            return Ok(result);
        }
        if matches!(
            object.state,
            ObjectState::Retired | ObjectState::PendingDeletion
        ) {
            result.idle_millis = Measurement::Unavailable {
                reason: "already-withdrawn-age-not-required".into(),
            };
            result.eligible = true;
            return Ok(result);
        }
        let grace = if object.kind == ObjectKind::Checkpoint {
            policy.collection.checkpoint_grace_ms
        } else if object.kind == ObjectKind::Generation {
            policy.collection.generation_grace_ms
        } else {
            0
        };
        let previous: Option<IdleEvidence> = object
            .idle_evidence
            .clone()
            .map(serde_json::from_value)
            .transpose()?;
        let stamp = self.clock.now()?;
        let (idle, known) = advance_idle(previous.as_ref(), stamp, object.use_sequence);
        result.idle_millis = if known || grace == 0 {
            Measurement::Observed {
                value: idle.proven_millis,
            }
        } else {
            Measurement::Unavailable {
                reason: "age-not-yet-proven".into(),
            }
        };
        if record_age {
            self.transaction(|transaction| {
                let mut current = object_row(transaction, &object.id)?;
                if current.use_sequence != object.use_sequence || current.state != object.state {
                    return Err(Error::busy("eligibility-changed"));
                }
                current.idle_evidence = Some(serde_json::to_value(&idle)?);
                save_object(transaction, &mut current)
            })?;
        }
        if idle.proven_millis < grace || (!known && grace != 0) {
            result.reasons.push("minimum-idle-age".into());
        } else {
            result.eligible = true;
        }
        Ok(result)
    }

    pub fn eligibility(&self, id: &Id) -> Result<Eligibility> {
        let object = self.object(id)?;
        let mut eligibility = self.eligibility_locked(&object, false)?;
        if object.state != ObjectState::Removed {
            match exclusive_guard(self, &object) {
                Ok(_guard) => {
                    checked_directory(self, &object)?;
                }
                Err(error) if error.category == ErrorCategory::Busy => {
                    eligibility.eligible = false;
                    eligibility.reasons.push("object-readers-active".into());
                }
                Err(error) => return Err(error),
            }
        }
        Ok(eligibility)
    }

    pub fn preview_collection(&self, cursor: Option<CatalogCursor>) -> Result<CollectionPreview> {
        let policy_version = self.policy()?.version;
        let allocation_version = self.allocation()?.version;
        let page = self.page(cursor)?;
        let objects = page
            .objects
            .into_iter()
            .map(|object| self.eligibility(&object.id))
            .collect::<Result<Vec<_>>>()?;
        Ok(CollectionPreview {
            policy_version,
            allocation_version,
            catalog_revision: page.revision,
            examined: page.examined,
            objects,
            next: page.next,
        })
    }

    fn next_collection_object(&self, cursor: &mut CollectionCursor) -> Result<Option<Id>> {
        if cursor.phase > 3 {
            return Err(Error::invalid("invalid collection cursor phase"));
        }
        if let Some(current) = &cursor.current {
            return Ok(Some(current.clone()));
        }
        let condition = match cursor.phase {
            0 => "state IN ('retired','pending-deletion')",
            1 => "kind='checkpoint' AND state IN ('preparing','published')",
            2 => {
                "kind IN ('build-stage','migration-stage','checkpoint-stage') AND state IN ('preparing','published')"
            }
            3 => "kind='generation' AND state IN ('preparing','published')",
            _ => unreachable!(),
        };
        self.read(|connection| {
            let candidate: Option<(String, u64)> = connection.query_row(
                &format!("SELECT id,use_sequence FROM objects WHERE {condition} AND use_sequence<=?3
                  AND (use_sequence>?2 OR (use_sequence=?2 AND id>?1)) ORDER BY use_sequence,id LIMIT 1"),
                params![cursor.after.as_ref().map_or("", Id::as_str), sql_integer(cursor.after_use)?,
                    sql_integer(cursor.through_use.ok_or_else(|| Error::invalid("collection traversal has no high-water mark"))?)?],
                |row| Ok((row.get(0)?, super::catalog::unsigned(row, 1)?)),
            ).optional()?;
            candidate.map(|(id, sequence)| {
                cursor.current_use = Some(sequence);
                Id::parse(id)
            }).transpose()
        })
    }

    fn save_collection_progress(
        &self,
        operation: &Id,
        progress: &CollectionProgress,
        finished: bool,
    ) -> Result<()> {
        self.transaction(|transaction| {
            let encoded: String = transaction.query_row(
                "SELECT record FROM operations WHERE id=?1",
                [operation.as_str()],
                |row| row.get(0),
            )?;
            let mut record: OperationRecord = serde_json::from_str(&encoded)?;
            record.progress = serde_json::to_value(progress)?;
            if progress.retired > 0 || progress.removed > 0 || progress.logical_bytes_reclaimed > 0
            {
                record.committed_state = CommitState::Committed;
            }
            record.state = if progress.cancelled {
                OperationState::Cancelled
            } else if finished {
                OperationState::Completed
            } else {
                OperationState::Preparing
            };
            if finished {
                record.result = Some(serde_json::to_value(progress)?);
            }
            Self::save_operation(transaction, &record)
        })
    }

    /// Execute one bounded, resumable pass. Replaying the operation returns its saved result.
    pub fn collect_pass(
        self: &Arc<Self>,
        operation: &Id,
        request: &CollectionRequest,
    ) -> Result<CollectionProgress> {
        let prior = self.operation(operation)?;
        if prior.kind != "collection" || prior.request != serde_json::to_value(request)? {
            return Err(Error::invalid(
                "collection request differs from its accepted operation",
            ));
        }
        if let Some(result) = prior.result {
            return Ok(serde_json::from_value(result)?);
        }
        if let Some(error) = prior.error {
            return Err(serde_json::from_value(error)?);
        }
        let observation = self.observe_pass(super::diagnostics::Kind::Collection)?;
        let result = self
            .collect_pass_inner(operation, request)
            .map_err(|error| self.operation_error(operation, error));
        observation.collection(&result)?;
        result
    }

    fn collect_pass_inner(
        self: &Arc<Self>,
        operation: &Id,
        request: &CollectionRequest,
    ) -> Result<CollectionProgress> {
        let _receipt = self.hold_operation(operation)?;
        let _activity = ActivityGuard::acquire(&self.directory)?;
        let prior = self.operation(operation)?;
        if let Some(result) = prior.result {
            return Ok(serde_json::from_value(result)?);
        }
        if prior.kind != "collection" {
            return Err(Error::invalid("operation is not a collection pass"));
        }
        if prior.request != serde_json::to_value(request)? {
            return Err(Error::invalid(
                "collection request differs from its accepted operation",
            ));
        }
        let policy = self.policy()?;
        if policy.version != request.policy_version {
            return Err(Error::stale_version(policy.version));
        }
        let allocation = self.allocation()?;
        if allocation.version != request.allocation_version {
            return Err(Error::stale_version(allocation.version));
        }
        request.bounds.validate(&policy.policy.collection)?;
        let _permit = self.reserve(
            operation,
            None,
            super::WorkRequest {
                allocation_version: allocation.version,
                staging_bytes: 0,
                private_bytes: 2 * 1024 * 1024,
                slots: 1,
            },
        )?;
        let start = Instant::now();
        let deadline = start
            .checked_add(Duration::from_millis(request.bounds.max_duration_ms))
            .ok_or_else(|| Error::invalid("collection deadline overflow"))?;
        let mut cursor = request.cursor.clone().unwrap_or_default();
        let mut progress: CollectionProgress = if prior
            .progress
            .as_object()
            .is_some_and(|value| value.is_empty())
        {
            CollectionProgress::default()
        } else {
            serde_json::from_value(prior.progress)?
        };
        if let Some(next) = &progress.next {
            cursor = next.clone();
        }
        let revision =
            self.read(|connection| {
                Ok(connection.query_row(
                    "SELECT revision FROM state WHERE singleton=1",
                    [],
                    |row| super::catalog::unsigned(row, 0),
                )?)
            })?;
        if cursor.through_use.is_some_and(|through| through > revision) {
            return Err(Error::invalid(
                "collection high-water mark exceeds the catalog revision",
            ));
        }
        cursor.through_use.get_or_insert(revision);
        let mut complete = false;
        while Instant::now() < deadline
            && progress.examined < request.bounds.max_examined
            && progress.removed < request.bounds.max_removed
            && progress.pages < request.bounds.max_pages
            && progress.logical_bytes_reclaimed < request.bounds.max_delete_bytes
        {
            if self.operation(operation)?.cancelled {
                progress.cancelled = true;
                break;
            }
            let current_policy = self.policy()?;
            let current_allocation = self.allocation()?;
            if current_policy.version != request.policy_version
                || current_allocation.version != request.allocation_version
            {
                return Err(Error::stale_version(
                    if current_policy.version != request.policy_version {
                        current_policy.version
                    } else {
                        current_allocation.version
                    },
                )
                .committed(if progress.retired > 0 {
                    CommitState::Committed
                } else {
                    CommitState::NotCommitted
                })
                .operation(operation.to_string()));
            }
            if cursor.current.is_none() {
                progress.pages += 1;
            }
            let Some(id) = self.next_collection_object(&mut cursor)? else {
                if cursor.phase == 3 {
                    complete = true;
                    break;
                }
                cursor.phase += 1;
                cursor.after = None;
                cursor.after_use = 0;
                continue;
            };
            progress.examined += 1;
            cursor.current = Some(id.clone());
            match self.object(&id) {
                Ok(object) => {
                    cursor.current_use.get_or_insert(object.use_sequence);
                }
                Err(error) if error.category == ErrorCategory::CacheEvicted => {
                    cursor.current = None;
                    cursor.after_use = cursor.current_use.take().ok_or_else(|| Error::new(
                        ErrorCategory::RecoveryRequired, "collection-cursor-object-evicted",
                        "the old cursor has no ordering evidence; start a new collection traversal",
                    ))?;
                    cursor.after = Some(id);
                    continue;
                }
                Err(error) => return Err(error),
            }
            progress.next = Some(cursor.clone());
            self.save_collection_progress(operation, &progress, false)?;
            let result =
                self.collect_object(operation, &id, &request.bounds, &mut progress, deadline);
            match result {
                Ok(true) => {
                    cursor.current = None;
                    cursor.after_use = cursor
                        .current_use
                        .take()
                        .ok_or_else(|| Error::corrupt("collection ordering evidence missing"))?;
                    cursor.after = Some(id);
                }
                Ok(false) => break,
                Err(error) => {
                    if error.reason_code == "collection-journal-update"
                        || (matches!(error.category, ErrorCategory::CorruptMetadata)
                            && error.reason_code == "catalog-io")
                    {
                        return Err(error.operation(operation.to_string()));
                    }
                    progress.skipped += 1;
                    progress.errors += 1;
                    if progress.details.len() < policy.policy.work.page_objects as usize {
                        progress.details.push(CollectionSkip {
                            object: id.clone(),
                            reasons: vec![error.reason_code.clone()],
                            error: Some(serde_json::to_value(&error)?),
                        });
                    } else {
                        progress.omitted_skip_details += 1;
                    }
                    self.collection_transaction(
                        operation,
                        &mut progress,
                        |transaction, _progress| {
                            let mut object = object_row(transaction, &id)?;
                            object.error = Some(serde_json::to_value(&error)?);
                            save_object(transaction, &mut object)
                        },
                    )?;
                    cursor.current = None;
                    cursor.after_use = cursor
                        .current_use
                        .take()
                        .ok_or_else(|| Error::corrupt("collection ordering evidence missing"))?;
                    cursor.after = Some(id);
                }
            }
        }
        progress.next = if complete { None } else { Some(cursor) };
        progress.elapsed_nanos = Some(
            u64::try_from(start.elapsed().as_nanos())
                .map_err(|_| Error::corrupt("collection duration overflow"))?,
        );
        progress.elapsed_budget_exceeded =
            Some(start.elapsed() > Duration::from_millis(request.bounds.max_duration_ms));
        self.save_collection_progress(operation, &progress, true)?;
        Ok(progress)
    }

    fn collect_object(
        &self,
        operation: &Id,
        id: &Id,
        bounds: &CollectionBounds,
        progress: &mut CollectionProgress,
        deadline: Instant,
    ) -> Result<bool> {
        let object = self.object(id)?;
        if object.state == ObjectState::Removed {
            return Ok(true);
        }
        if object.state == ObjectState::PendingDeletion
            && self.finish_missing_container(operation, &object, progress)?
        {
            return Ok(true);
        }
        let _guard = match exclusive_guard(self, &object) {
            Ok(guard) => guard,
            Err(error)
                if error.category == ErrorCategory::Busy
                    && error.reason_code == "object-readers-active" =>
            {
                progress.skipped += 1;
                if progress.details.len() < self.policy()?.policy.work.page_objects as usize {
                    progress.details.push(CollectionSkip {
                        object: id.clone(),
                        reasons: vec![error.reason_code],
                        error: None,
                    });
                } else {
                    progress.omitted_skip_details += 1;
                }
                return Ok(true);
            }
            Err(error) => return Err(error),
        };
        let mut object = self.object(id)?;
        let eligibility = self.eligibility_locked(&object, true)?;
        if !eligibility.eligible {
            progress.skipped += 1;
            if progress.details.len() < self.policy()?.policy.work.page_objects as usize {
                progress.details.push(CollectionSkip {
                    object: id.clone(),
                    reasons: eligibility.reasons,
                    error: None,
                });
            } else {
                progress.omitted_skip_details += 1;
            }
            return Ok(true);
        }
        let directory = checked_directory(self, &object)?;
        if !matches!(
            object.state,
            ObjectState::Retired | ObjectState::PendingDeletion
        ) {
            self.collection_transaction(operation, progress, |transaction, progress| {
                let references: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM refs WHERE target=?1)",
                    [id.as_str()],
                    |row| row.get(0),
                )?;
                if references {
                    return Err(Error::busy("reference-acquired-before-retirement"));
                }
                object = object_row(transaction, id)?;
                object.state = ObjectState::Retired;
                save_object(transaction, &mut object)?;
                // Withdrawal precedes release of the checkpoint's generation reference.
                if object.kind == ObjectKind::Checkpoint {
                    transaction.execute(
                        "DELETE FROM refs WHERE source_kind='checkpoint' AND source_id=?1",
                        [id.as_str()],
                    )?;
                }
                progress.retired += 1;
                Ok(())
            })?;
            self.fault(super::faults::Point::ObjectWithdrawn, Some(operation))?;
        }
        if object.state == ObjectState::Retired {
            self.collection_transaction(operation, progress, |transaction, _progress| {
                object.state = ObjectState::PendingDeletion;
                save_object(transaction, &mut object)
            })?;
            self.fault(super::faults::Point::ObjectPendingDeletion, Some(operation))?;
        }
        loop {
            if Instant::now() >= deadline
                || progress.pages >= bounds.max_pages
                || progress.logical_bytes_reclaimed >= bounds.max_delete_bytes
            {
                return Ok(false);
            }
            if self.operation(operation)?.cancelled {
                progress.cancelled = true;
                return Ok(false);
            }
            progress.pages += 1;
            let member: Option<FileRecord> = self.read(|connection| {
                let record: Option<String> = connection.query_row(
                    "SELECT record FROM members WHERE object_id=?1 AND json_extract(record,'$.removed')=0
                     ORDER BY (name='object.json'),name LIMIT 1",
                    [id.as_str()], |row| row.get(0),
                ).optional()?;
                record.map(|record| Ok(serde_json::from_str(&record)?)).transpose()
            })?;
            let Some(member) = member else {
                break;
            };
            if !self.delete_member(operation, &object, &directory, member, bounds, progress)? {
                return Ok(false);
            }
        }
        let expected = directory.identity()?;
        drop(directory);
        self.fault(super::faults::Point::ObjectBeforeRemove, Some(operation))?;
        self.objects
            .remove_child(id.as_str(), &expected)
            .map_err(|error| {
                if error.source_io_kind() == Some(std::io::ErrorKind::DirectoryNotEmpty) {
                    Error::new(
                        ErrorCategory::Incompatible,
                        "unknown-object-entries",
                        "unregistered entries were preserved in the owned container",
                    )
                } else {
                    error
                }
            })?;
        self.fault(super::faults::Point::ObjectAfterRemove, Some(operation))?;
        // Catalog withdrawal already forbids new readers. Close our own handle
        // before confirming Windows has physically removed its lock file.
        drop(_guard);
        self.remove_owned_control("guards", &format!("{id}.lock"))?;
        self.fault(super::faults::Point::GuardAfterRemove, Some(operation))?;
        self.collection_transaction(operation, progress, |transaction, progress| {
            object.state = ObjectState::Removed;
            object.logical_bytes = 0;
            object.allocated_bytes = Measurement::Observed { value: 0 };
            object.error = None;
            save_object(transaction, &mut object)?;
            progress.removed += 1;
            Ok(())
        })?;
        self.fault(super::faults::Point::ObjectRemoved, Some(operation))?;
        Ok(true)
    }

    fn finish_missing_container(
        &self,
        operation: &Id,
        object: &ObjectRecord,
        progress: &mut CollectionProgress,
    ) -> Result<bool> {
        match self.objects.child(object.id.as_str()) {
            Ok(_) => return Ok(false),
            Err(error) if error.source_io_kind() == Some(std::io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error),
        }
        let outstanding = self.read(|connection| {
            Ok(connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM members WHERE object_id=?1 AND json_extract(record,'$.removed')=0)
                 OR EXISTS(SELECT 1 FROM refs WHERE target=?1)",
                [object.id.as_str()], |row| row.get::<_, bool>(0),
            )?)
        })?;
        if outstanding || object.logical_bytes != 0 {
            return Err(Error::corrupt(
                "missing pending-deletion container has unconfirmed members or references",
            ));
        }
        let name = format!("{}.lock", object.id);
        match self.guards.open_file(&name, true) {
            Ok(file) => {
                let expected = object
                    .guard_identity
                    .as_ref()
                    .ok_or_else(|| Error::corrupt("guard identity missing"))?;
                if &FileIdentity::of(&file)? != expected {
                    return Err(Error::corrupt("recovery guard identity differs"));
                }
                fs2::FileExt::try_lock_exclusive(&file)
                    .map_err(|error| lock_error(error, "recovery-reader-active"))?;
                drop(file);
                self.remove_owned_control("guards", &name)?;
            }
            Err(error) if error.source_io_kind() == Some(std::io::ErrorKind::NotFound) => {
                self.remove_owned_control("guards", &name)?;
            }
            Err(error) => return Err(error),
        }
        self.collection_transaction(operation, progress, |transaction, progress| {
            let mut object = object_row(transaction, &object.id)?;
            object.state = ObjectState::Removed;
            object.allocated_bytes = Measurement::Observed { value: 0 };
            object.error = None;
            save_object(transaction, &mut object)?;
            progress.recovered_objects += 1;
            progress.removed += 1;
            Ok(())
        })?;
        Ok(true)
    }

    fn progress_in_transaction(
        transaction: &rusqlite::Transaction<'_>,
        operation: &Id,
        progress: &CollectionProgress,
    ) -> Result<()> {
        let encoded: String = transaction.query_row(
            "SELECT record FROM operations WHERE id=?1",
            [operation.as_str()],
            |row| row.get(0),
        )?;
        let mut record: OperationRecord = serde_json::from_str(&encoded)?;
        record.progress = serde_json::to_value(progress)?;
        record.committed_state = CommitState::Committed;
        Self::save_operation(transaction, &record)
    }

    fn collection_transaction<T>(
        &self,
        operation: &Id,
        progress: &mut CollectionProgress,
        change: impl FnOnce(&rusqlite::Transaction<'_>, &mut CollectionProgress) -> Result<T>,
    ) -> Result<T> {
        let mut next = progress.clone();
        let result = self
            .transaction(|transaction| {
                let result = change(transaction, &mut next)?;
                Self::progress_in_transaction(transaction, operation, &next)?;
                Ok(result)
            })
            .map_err(|mut error| {
                error.detail = format!(
                    "collection catalog transition ({}): {}",
                    error.reason_code, error.detail
                );
                error.reason_code = "collection-journal-update".into();
                error.operation_id = Some(operation.to_string());
                error
            })?;
        *progress = next;
        Ok(result)
    }

    fn delete_member(
        &self,
        operation: &Id,
        object: &ObjectRecord,
        directory: &Directory,
        mut member: FileRecord,
        bounds: &CollectionBounds,
        progress: &mut CollectionProgress,
    ) -> Result<bool> {
        let file = match directory.open_file(&member.name, true) {
            Ok(file) => Some(file),
            Err(error)
                if member.pending_length == Some(0)
                    && error.source_io_kind() == Some(std::io::ErrorKind::NotFound) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        if let Some(file) = &file {
            if FileIdentity::of(file)? != member.identity {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "member-replaced",
                    "refusing to delete a different physical member",
                ));
            }
            let current = file.metadata()?.len();
            let expected_truncation = member.pending_length.is_some_and(|target| {
                target != 0 && target < member.logical_bytes && current == target
            });
            if !expected_truncation
                && (super::storage::file_change(file)? != member.change
                    || current != member.logical_bytes)
            {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "member-modified",
                    "the inventoried file was externally modified and has been preserved",
                ));
            }
        }
        let current = file
            .as_ref()
            .map(|file| file.metadata().map(|metadata| metadata.len()))
            .transpose()?
            .unwrap_or(0);
        let remaining = bounds.max_delete_bytes - progress.logical_bytes_reclaimed;
        let chunk = self.policy()?.policy.collection.chunk_bytes.min(remaining);
        if current > chunk && chunk < 4096 {
            return Ok(false);
        }
        let mut recovered = file.is_none();
        if let Some(target) = member.pending_length
            && current < member.logical_bytes
        {
            if current != target {
                return Err(Error::corrupt(
                    "deletion outcome differs from its exact journaled length",
                ));
            }
            recovered = true;
        }
        if !recovered && let Some(file) = file.as_ref() {
            // Observe and journal the exact identity/length before destructive I/O.
            let before_allocation = allocated_bytes(file)?;
            let old_accounted = member.logical_bytes;
            member.logical_bytes = current;
            member.allocated_bytes = before_allocation;
            member.pending_length = Some(current.saturating_sub(chunk));
            self.collection_transaction(operation, progress, |transaction, _progress| {
                let mut object = object_row(transaction, &object.id)?;
                object.logical_bytes = object
                    .logical_bytes
                    .checked_sub(old_accounted)
                    .and_then(|value| value.checked_add(current))
                    .ok_or_else(|| Error::corrupt("pre-deletion accounting overflow"))?;
                transaction.execute(
                    "UPDATE members SET record=?3 WHERE object_id=?1 AND name=?2",
                    params![object.id.as_str(), member.name, text(&member)?],
                )?;
                save_object(transaction, &mut object)
            })?;
            self.fault(super::faults::Point::MemberIntentSaved, Some(operation))?;
        }
        let before = member.logical_bytes;
        let target = member
            .pending_length
            .ok_or_else(|| Error::corrupt("deletion intent is absent"))?;
        let after;
        let after_allocation;
        let after_change;
        let removed;
        match (recovered, file) {
            (false, Some(file)) if target != 0 => {
                self.fault(super::faults::Point::MemberBeforeIo, Some(operation))?;
                file.set_len(target)?;
                file.sync_all()?;
                self.fault(super::faults::Point::MemberAfterIo, Some(operation))?;
                after = file.metadata()?.len();
                if after != target {
                    return Err(Error::corrupt(
                        "truncate did not reach its journaled length",
                    ));
                }
                removed = false;
                after_allocation = allocated_bytes(&file)?;
                after_change = super::storage::file_change(&file)?;
            }
            (false, Some(file)) => {
                drop(file);
                self.fault(super::faults::Point::MemberBeforeIo, Some(operation))?;
                directory.remove_file(&member.name, &member.identity)?;
                self.fault(super::faults::Point::MemberAfterIo, Some(operation))?;
                after = 0;
                removed = true;
                after_allocation = Measurement::Observed { value: 0 };
                after_change = member.change;
            }
            (_, file) => {
                after = current;
                removed = file.is_none();
                after_allocation = match &file {
                    Some(file) => allocated_bytes(file)?,
                    None => Measurement::Observed { value: 0 },
                };
                after_change = file
                    .as_ref()
                    .map(super::storage::file_change)
                    .transpose()?
                    .unwrap_or(member.change);
            }
        }
        let logical = before
            .checked_sub(after)
            .ok_or_else(|| Error::corrupt("file grew after withdrawal"))?;
        let allocated = match (&member.allocated_bytes, &after_allocation) {
            (Measurement::Observed { value: before }, Measurement::Observed { value: after })
                if before >= after =>
            {
                Some(before - after)
            }
            _ => None,
        };
        member.logical_bytes = after;
        member.change = after_change;
        member.allocated_bytes = after_allocation;
        member.pending_length = None;
        member.removed = removed;
        member.credited_logical_bytes = member
            .credited_logical_bytes
            .checked_add(logical)
            .ok_or_else(|| Error::corrupt("deletion credit overflow"))?;
        if let Some(allocated) = allocated {
            member.credited_allocated_bytes = member
                .credited_allocated_bytes
                .checked_add(allocated)
                .ok_or_else(|| Error::corrupt("allocation credit overflow"))?;
        }
        self.collection_transaction(operation, progress, |transaction, progress| {
            self.fault(super::faults::Point::MemberBeforeCredit, Some(operation))?;
            let mut object = object_row(transaction, &object.id)?;
            object.logical_bytes = object
                .logical_bytes
                .checked_sub(logical)
                .ok_or_else(|| Error::corrupt("object deletion accounting underflow"))?;
            match (&mut object.allocated_bytes, allocated) {
                (Measurement::Observed { value }, Some(amount)) if *value >= amount => {
                    *value -= amount
                }
                _ => {
                    object.allocated_bytes = Measurement::Unavailable {
                        reason: "deletion-allocation-not-fully-observed".into(),
                    }
                }
            }
            object.error = None;
            transaction.execute(
                "UPDATE members SET record=?3 WHERE object_id=?1 AND name=?2",
                params![object.id.as_str(), member.name, text(&member)?],
            )?;
            save_object(transaction, &mut object)?;
            if recovered {
                progress.recovered_logical_bytes = progress
                    .recovered_logical_bytes
                    .checked_add(logical)
                    .ok_or_else(|| Error::corrupt("recovery accounting overflow"))?;
                if let Some(allocated) = allocated {
                    progress.recovered_allocated_bytes = progress
                        .recovered_allocated_bytes
                        .checked_add(allocated)
                        .ok_or_else(|| Error::corrupt("recovery allocation accounting overflow"))?;
                } else {
                    progress.allocation_measurement_unavailable = true;
                }
            } else {
                progress.logical_bytes_reclaimed = progress
                    .logical_bytes_reclaimed
                    .checked_add(logical)
                    .ok_or_else(|| Error::corrupt("pass deletion accounting overflow"))?;
                if let Some(allocated) = allocated {
                    progress.allocated_bytes_reclaimed = progress
                        .allocated_bytes_reclaimed
                        .checked_add(allocated)
                        .ok_or_else(|| Error::corrupt("pass allocation accounting overflow"))?;
                } else {
                    progress.allocation_measurement_unavailable = true;
                }
            }
            Ok(())
        })?;
        self.fault(super::faults::Point::MemberAfterCredit, Some(operation))?;
        Ok(true)
    }
}
