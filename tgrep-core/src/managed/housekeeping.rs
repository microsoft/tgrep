// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::{text, unsigned};
use super::lifetime::lock_error;
use super::storage::{Directory, allocated_bytes};
use super::{
    Error, ErrorCategory, FileIdentity, FileRecord, Id, Measurement, Namespace, ObjectState,
    OperationRecord, OperationState, OwnerRecord, Result,
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupCounts {
    pub history_rows: u32,
    pub member_rows: u32,
    pub proof_rows: u32,
    pub objects: u32,
    pub receipts: u32,
    pub leases: u32,
    pub views: u32,
    pub roots: u32,
    pub root_retirements: u32,
    pub owners: u32,
    pub bootstrap_receipts: u32,
    pub scopes: u32,
    pub control_logical_bytes_reclaimed: u64,
    pub control_recovered_logical_bytes: u64,
    pub control_files_removed: u32,
    pub control_verification_bytes: u64,
}

#[derive(Default)]
pub(super) struct ControlRemoval {
    pub removed: bool,
    pub logical_bytes: u64,
    pub recovered_logical_bytes: u64,
    pub verification_bytes: u64,
}

impl CleanupCounts {
    pub(super) fn control_removed(&mut self, removal: ControlRemoval) {
        self.control_files_removed += u32::from(removal.removed);
        self.control_logical_bytes_reclaimed += removal.logical_bytes;
        self.control_recovered_logical_bytes += removal.recovered_logical_bytes;
        self.control_verification_bytes += removal.verification_bytes;
    }

    fn root_retired(&mut self, retirement: (bool, ControlRemoval)) {
        self.root_retirements += u32::from(retirement.0);
        self.control_removed(retirement.1);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        CollectionBounds, CollectionCursor, CollectionRequest, ObjectKind, OperationToken,
        OwnerGuard, Token, WorkRequest,
    };
    use super::*;
    use std::sync::Arc;

    fn namespace() -> (tempfile::TempDir, Arc<Namespace>, OwnerGuard) {
        let temp = tempfile::tempdir().unwrap();
        let namespace = Namespace::initialize_identity(
            &"a".repeat(64),
            temp.path(),
            super::super::policy::fixture_policy(),
        )
        .unwrap();
        namespace.activate().unwrap();
        let owner = namespace
            .prepare_owner_with_token(&Token::parse("bootstrap").unwrap())
            .unwrap();
        let guard = OwnerGuard::claim(owner.claim).unwrap();
        namespace.register_owner(guard.registration()).unwrap();
        (temp, namespace, guard)
    }

    fn token(owner: &OwnerGuard, sequence: u64) -> OperationToken {
        OperationToken {
            scope: owner.registration().owner.clone(),
            sequence,
            token: Token::parse(format!("operation-{sequence}")).unwrap(),
        }
    }

    fn stage(
        namespace: &Arc<Namespace>,
        owner: &OwnerGuard,
        sequence: u64,
    ) -> (
        super::super::ObjectRecord,
        Arc<super::super::lifetime::ObjectGuard>,
    ) {
        let operation = namespace
            .accept_operation(token(owner, sequence), "build", serde_json::json!({}))
            .unwrap();
        let permit = namespace
            .reserve(
                &operation.id,
                Some(&owner.registration().owner),
                WorkRequest {
                    allocation_version: 1,
                    staging_bytes: 1024 * 1024,
                    private_bytes: 1024 * 1024,
                    slots: 1,
                },
            )
            .unwrap();
        let object = namespace
            .create_object(ObjectKind::CheckpointStage, None, &permit)
            .unwrap();
        namespace
            .record_operation_result(&operation.id, Ok(serde_json::json!({})), false)
            .unwrap();
        drop(permit);
        object
    }

    fn recover(namespace: &Namespace) -> CleanupCounts {
        let mut cursor = None;
        let mut all = CleanupCounts::default();
        for _ in 0..1024 {
            let progress = namespace.recover_pass(cursor).unwrap();
            assert!(progress.issues.is_empty(), "{progress:?}");
            assert!(progress.examined <= namespace.policy().unwrap().policy.work.page_objects);
            all.history_rows += progress.cleanup.history_rows;
            all.objects += progress.cleanup.objects;
            all.owners += progress.cleanup.owners;
            all.control_logical_bytes_reclaimed += progress.cleanup.control_logical_bytes_reclaimed;
            cursor = progress.next;
            if cursor.is_none() {
                return all;
            }
        }
        panic!("bounded recovery did not finish its traversal");
    }

    #[test]
    fn history_pruning_preserves_cursor_versions_then_compacts_collected_incarnations() {
        let (_temp, namespace, owner) = namespace();
        let mut policy = namespace.policy().unwrap();
        policy.policy.work.page_objects = 1;
        policy.policy.collection.max_pages = 16;
        namespace
            .update_policy(policy.version, policy.policy)
            .unwrap();
        let (first, first_pin) = stage(&namespace, &owner, 1);
        let (second, second_pin) = stage(&namespace, &owner, 2);
        let snapshot = namespace.page(None).unwrap().next.unwrap();
        namespace
            .transaction(|transaction| {
                let mut second = super::super::catalog::object_row(transaction, &second.id)?;
                second.error = Some(serde_json::json!({"reason":"later-version"}));
                super::super::catalog::save_object(transaction, &mut second)
            })
            .unwrap();
        assert!(recover(&namespace).history_rows > 0);
        let old = namespace.page(Some(snapshot)).unwrap();
        assert_eq!(old.objects[0].id, second.id);
        assert_eq!(old.objects[0].error, None);
        drop((first_pin, second_pin));
        let request = CollectionRequest {
            policy_version: namespace.policy().unwrap().version,
            allocation_version: 1,
            bounds: CollectionBounds::from_policy(&namespace.policy().unwrap().policy.collection),
            cursor: None,
        };
        let mut request = request;
        for sequence in 3..100 {
            let operation = namespace
                .accept_operation(
                    token(&owner, sequence),
                    "collection",
                    serde_json::to_value(&request).unwrap(),
                )
                .unwrap();
            request.cursor = namespace
                .collect_pass(&operation.id, &request)
                .unwrap()
                .next;
            if request.cursor.is_none() {
                break;
            }
        }
        assert!(request.cursor.is_none());
        assert_eq!(
            namespace.object(&first.id).unwrap().state,
            ObjectState::Removed
        );
        assert_eq!(recover(&namespace).objects, 2);
        assert_eq!(
            namespace.object(&first.id).unwrap_err().category,
            ErrorCategory::CacheEvicted
        );
        assert_eq!(
            namespace.object(&Id::new().unwrap()).unwrap_err().category,
            ErrorCategory::CacheMissing
        );
        assert!(namespace.page(None).unwrap().objects.is_empty());
        request.cursor = Some(CollectionCursor {
            current: Some(first.id.clone()),
            current_use: Some(first.use_sequence),
            ..Default::default()
        });
        let operation = namespace
            .accept_operation(
                token(&owner, 101),
                "collection",
                serde_json::to_value(&request).unwrap(),
            )
            .unwrap();
        assert!(
            namespace
                .collect_pass(&operation.id, &request)
                .unwrap()
                .next
                .is_none()
        );
        let (replacement, _pin) = stage(&namespace, &owner, 102);
        assert_ne!(replacement.id, first.id);
    }

    #[test]
    fn owner_control_cleanup_requires_ended_handles_and_never_replays_bootstrap_as_new() {
        let (_temp, namespace, owner) = namespace();
        let claim = owner.registration().clone();
        let bytes = namespace.work_usage().unwrap().control_logical_bytes;
        assert!(bytes > 0);
        namespace.release_owner(&claim).unwrap();
        assert_eq!(recover(&namespace).owners, 0);
        assert_eq!(namespace.work_usage().unwrap().control_logical_bytes, bytes);
        drop(owner);
        let cleaned = recover(&namespace);
        assert_eq!(cleaned.owners, 1);
        assert_eq!(cleaned.control_logical_bytes_reclaimed, bytes);
        assert_eq!(namespace.work_usage().unwrap().control_logical_bytes, 0);
        assert_eq!(
            namespace.owner(&claim.owner).unwrap_err().category,
            ErrorCategory::ReceiptExpired
        );
        assert_eq!(
            namespace
                .prepare_owner_with_token(&Token::parse("bootstrap").unwrap())
                .unwrap_err()
                .category,
            ErrorCategory::ReceiptExpired
        );
        assert_eq!(recover(&namespace).control_logical_bytes_reclaimed, 0);
    }

    #[test]
    fn metadata_pressure_blocks_new_work_but_not_reference_safe_cleanup() {
        let (_temp, namespace, owner) = namespace();
        let (_object, _pin) = stage(&namespace, &owner, 1);
        let mut policy = namespace.policy().unwrap();
        policy.policy.work.metadata_bytes = 1024 * 1024;
        namespace
            .update_policy(policy.version, policy.policy)
            .unwrap();
        namespace
            .directory
            .open_file("owner.lock", true)
            .unwrap()
            .set_len(2 * 1024 * 1024)
            .unwrap();
        assert_eq!(
            namespace
                .accept_operation(token(&owner, 2), "build", serde_json::json!({}))
                .unwrap_err()
                .category,
            ErrorCategory::ResourcePressure
        );
        assert!(recover(&namespace).history_rows > 0);
        assert!(namespace.catalog_file_bytes().unwrap() > 1024 * 1024);
    }

    #[test]
    fn collection_preserves_modified_members_and_external_hard_links() {
        for hard_link in [false, true] {
            let (temp, namespace, owner) = namespace();
            let (object, pin) = stage(&namespace, &owner, 1);
            let path = pin.directory.path().join("object.json");
            drop(pin);
            if hard_link {
                std::fs::hard_link(&path, temp.path().join("external-link")).unwrap();
            } else {
                let mut contents = std::fs::read(&path).unwrap();
                contents.push(b'\n');
                std::fs::write(&path, contents).unwrap();
            }
            let request = CollectionRequest {
                policy_version: 1,
                allocation_version: 1,
                bounds: CollectionBounds::from_policy(
                    &namespace.policy().unwrap().policy.collection,
                ),
                cursor: None,
            };
            let operation = namespace
                .accept_operation(
                    token(&owner, 2),
                    "collection",
                    serde_json::to_value(&request).unwrap(),
                )
                .unwrap();
            let progress = namespace.collect_pass(&operation.id, &request).unwrap();
            assert_eq!(progress.logical_bytes_reclaimed, 0, "{progress:?}");
            assert!(
                progress
                    .details
                    .iter()
                    .any(|detail| detail.object == object.id && detail.error.is_some()),
                "{progress:?}"
            );
            assert!(path.exists());
            if hard_link {
                assert!(temp.path().join("external-link").exists());
            }
        }
    }

    #[test]
    fn owner_control_unlink_intents_recover_once_at_every_boundary() {
        use super::super::faults::{Action, Point, Specification};
        for point in [
            Point::ControlIntentSaved,
            Point::ControlAfterRemove,
            Point::ControlBeforeCredit,
        ] {
            let (_temp, namespace, owner) = namespace();
            let name = format!("{}.json", owner.registration().owner);
            namespace.release_owner(owner.registration()).unwrap();
            drop(owner);
            let before = namespace
                .read(|connection| control_row(connection, "owners", &name))
                .unwrap();
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
                .remove_owned_control("owners", &name)
                .err()
                .expect("injected interruption");
            assert_eq!(error.committed_state, super::super::CommitState::Committed);
            let removed = namespace.remove_owned_control("owners", &name).unwrap();
            assert!(removed.removed);
            assert_eq!(
                removed.logical_bytes + removed.recovered_logical_bytes,
                before.file.logical_bytes
            );
            assert_eq!(
                removed.recovered_logical_bytes != 0,
                point != Point::ControlIntentSaved
            );
            let repeated = namespace.remove_owned_control("owners", &name).unwrap();
            assert!(!repeated.removed);
            assert_eq!(repeated.logical_bytes + repeated.recovered_logical_bytes, 0);
        }
    }

    #[test]
    fn completed_concurrent_control_removal_does_not_repeat_credit() {
        use super::super::faults::{Action, Point, Specification, Stage};
        let (_temp, namespace, owner) = namespace();
        let claim = owner.registration().clone();
        namespace.release_owner(&claim).unwrap();
        drop(owner);
        let name = format!("{}.json", claim.owner);
        let before = namespace.work_usage().unwrap().control_logical_bytes;
        let fault = namespace
            .install_test_fault(Specification {
                point: Point::ControlObserved,
                operation: None,
                skip_hits: 0,
                action: Action::Pause { timeout_ms: 10_000 },
            })
            .unwrap();
        let replayed = std::thread::scope(|scope| {
            let worker = scope.spawn(|| namespace.remove_owned_control("owners", &name));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let reached = loop {
                if namespace.test_fault_status().unwrap().unwrap().stage == Stage::Waiting {
                    break true;
                }
                if std::time::Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            };
            let removal = namespace.remove_owned_control("owners", &name);
            namespace.release_test_fault(&fault.ticket).unwrap();
            let result = worker.join().unwrap();
            assert!(reached, "control observation boundary was not exercised");
            let removal = removal.unwrap();
            assert!(removal.removed);
            assert_eq!(removal.logical_bytes, before);
            result.unwrap()
        });
        assert!(!replayed.removed);
        assert_eq!(replayed.logical_bytes + replayed.recovered_logical_bytes, 0);
        assert_eq!(namespace.work_usage().unwrap().control_logical_bytes, 0);
    }

    #[cfg(any(unix, windows))]
    fn change_control_ownership(path: &std::path::Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)?.permissions().mode();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode ^ 0o040))?;
        }
        #[cfg(windows)]
        {
            use std::os::windows::{ffi::OsStrExt, fs::MetadataExt};
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ATTRIBUTE_ARCHIVE, SetFileAttributesW,
            };
            let attributes = std::fs::metadata(path)?.file_attributes();
            let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: the owned fixture path is NUL-terminated and remains live.
            if unsafe { SetFileAttributesW(path.as_ptr(), attributes ^ FILE_ATTRIBUTE_ARCHIVE) }
                == 0
            {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn independent_control_permission_changes_are_preserved() {
        for changed in [false, true] {
            let (_temp, namespace, owner) = namespace();
            let name = format!("{}.json", owner.registration().owner);
            namespace.release_owner(owner.registration()).unwrap();
            drop(owner);
            let path = namespace.owners.path().join(&name);
            let contents = std::fs::read(&path).unwrap();
            let before = namespace
                .read(|connection| control_row(connection, "owners", &name))
                .unwrap();
            if changed {
                change_control_ownership(&path).unwrap();
                let file = std::fs::File::open(&path).unwrap();
                assert_ne!(
                    super::super::Ownership::capture(&file).unwrap(),
                    before.file.ownership
                );
            }
            let result = namespace.remove_owned_control("owners", &name);
            if changed {
                assert!(
                    result.is_err(),
                    "control cleanup deleted a file whose sealed ownership metadata changed"
                );
                assert!(path.exists());
                assert_eq!(std::fs::read(&path).unwrap(), contents);
                let after = namespace
                    .read(|connection| control_row(connection, "owners", &name))
                    .unwrap();
                assert!(!after.file.removed);
                assert!(after.file.pending_length.is_none());
                assert_eq!(after.file.credited_logical_bytes, 0);
            } else {
                let removed = result.unwrap();
                assert!(removed.removed);
                assert!(!path.exists());
                assert_eq!(removed.logical_bytes, before.file.logical_bytes);
            }
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn independent_control_ownership_change_after_intent_is_preserved() {
        use super::super::faults::{Action, Point, Specification, Stage};
        use std::time::Duration;

        let (_temp, namespace, owner) = namespace();
        let name = format!("{}.json", owner.registration().owner);
        namespace.release_owner(owner.registration()).unwrap();
        drop(owner);
        let path = namespace.owners.path().join(&name);
        let contents = std::fs::read(&path).unwrap();
        let before = namespace
            .read(|connection| control_row(connection, "owners", &name))
            .unwrap();
        let fault = namespace
            .install_test_fault(Specification {
                point: Point::ControlIntentSaved,
                operation: None,
                skip_hits: 0,
                action: Action::Pause { timeout_ms: 10_000 },
            })
            .unwrap();
        let result = std::thread::scope(|scope| {
            let worker = scope.spawn(|| namespace.remove_owned_control("owners", &name));
            let deadline = Instant::now() + Duration::from_secs(5);
            let reached = loop {
                let status = namespace.test_fault_status().unwrap().unwrap();
                if status.stage == Stage::Waiting {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            let mutation = reached.then(|| change_control_ownership(&path));
            let released = namespace.release_test_fault(&fault.ticket);
            let result = worker.join();
            assert!(reached, "the control unlink intent barrier was not reached");
            mutation.unwrap().unwrap();
            let released = released.unwrap();
            assert_eq!(released.stage, Stage::Released);
            assert_eq!(released.hits, 1);
            result.unwrap()
        });
        let error = result.err().expect(
            "control cleanup deleted a file whose ownership changed after intent commitment",
        );
        assert_eq!(error.committed_state, super::super::CommitState::Committed);
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), contents);
        let file = std::fs::File::open(&path).unwrap();
        assert_ne!(
            super::super::Ownership::capture(&file).unwrap(),
            before.file.ownership
        );
        let after = namespace
            .read(|connection| control_row(connection, "owners", &name))
            .unwrap();
        assert!(!after.file.removed);
        assert_eq!(after.file.pending_length, Some(before.file.logical_bytes));
        assert_eq!(after.file.credited_logical_bytes, 0);
    }

    #[test]
    fn missing_retired_root_guard_does_not_block_cleanup_or_alias_a_new_root_guard() {
        use super::super::faults::{Action, Point, Specification};
        let (temp, namespace, _owner) = namespace();
        let root_path = temp.path().join("worktree");
        std::fs::create_dir(&root_path).unwrap();
        let root = crate::rooted::RootedDir::open(&root_path).unwrap();
        let protection = super::super::roots::RootProtection::in_namespace(
            &namespace.directory,
            &root,
            &std::fs::canonicalize(&root_path).unwrap(),
        )
        .unwrap();
        let old = protection.anchor.clone();
        drop(protection);
        namespace
            .install_test_fault(Specification {
                point: Point::ControlAfterRemove,
                operation: None,
                skip_hits: 0,
                action: Action::Error {
                    category: ErrorCategory::Io,
                },
            })
            .unwrap();
        assert!(namespace.retire_root(&old).is_err());
        let replacement = super::super::roots::RootProtection::in_namespace(
            &namespace.directory,
            &root,
            &std::fs::canonicalize(&root_path).unwrap(),
        )
        .unwrap();
        assert_ne!(replacement.anchor.guard, old.guard);
        let (retired, removed) = namespace.retire_root(&old).unwrap();
        assert!(!retired);
        assert!(removed.removed);
        assert!(namespace.root_busy(&replacement.anchor).unwrap());
        assert!(!namespace.root_busy(&old).unwrap());
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlFile {
    file: FileRecord,
    checksum: [u8; 32],
}

pub(super) fn register_control(
    transaction: &Transaction<'_>,
    directory: &Directory,
    area: &str,
    name: &str,
    source: &str,
    producer: &std::fs::File,
    intended: &[u8],
) -> Result<()> {
    if !matches!(area, "guards" | "owners") {
        return Err(Error::invalid("unknown control-file area"));
    }
    let file = directory.open_file(name, false)?;
    let bytes = directory.read_bytes(name, 4096)?;
    let seal = super::MemberSeal::bounded(intended)?;
    if FileIdentity::of(&file)? != FileIdentity::of(producer)? || bytes != intended {
        return Err(Error::new(
            ErrorCategory::StaleIdentity,
            "control-file-modified",
            "control content or identity differs from its original producer",
        ));
    }
    let policy: String =
        transaction.query_row("SELECT policy FROM state WHERE singleton=1", [], |row| {
            row.get(0)
        })?;
    let policy: super::Policy = serde_json::from_str(&policy)?;
    super::catalog_io::admit_control(transaction, policy.work.metadata_bytes, bytes.len() as u64)?;
    if file.metadata()?.len() != bytes.len() as u64 {
        return Err(Error::corrupt(
            "control file changed while being inventoried",
        ));
    }
    let record = ControlFile {
        file: FileRecord {
            name: name.into(),
            identity: FileIdentity::of(&file)?,
            change: super::storage::file_change(&file)?,
            producer_open: false,
            seal: Some(seal),
            ownership: super::Ownership::capture(producer)?,
            logical_bytes: bytes.len() as u64,
            allocated_bytes: allocated_bytes(&file)?,
            pending_length: None,
            pending_manifest: None,
            removed: false,
            credited_logical_bytes: 0,
            credited_allocated_bytes: 0,
        },
        checksum: *blake3::hash(intended).as_bytes(),
    };
    transaction.execute(
        "INSERT INTO control_files VALUES(?1,?2,?3,?4)",
        params![area, name, source, text(&record)?],
    )?;
    Ok(())
}

fn control_row(connection: &rusqlite::Connection, area: &str, name: &str) -> Result<ControlFile> {
    let record: Option<String> = connection
        .query_row(
            "SELECT record FROM control_files WHERE area=?1 AND name=?2",
            params![area, name],
            |row| row.get(0),
        )
        .optional()?;
    Ok(serde_json::from_str(&record.ok_or_else(|| {
        Error::corrupt("control file has no authoritative ownership inventory")
    })?)?)
}

impl Namespace {
    /// An unlink intent remains until its once-only credit is durable. A missing
    /// file without that intent is not evidence that this collector reclaimed it.
    pub(super) fn remove_owned_control(&self, area: &str, name: &str) -> Result<ControlRemoval> {
        self.remove_owned_control_with_permit(area, name, None)
    }

    pub(super) fn remove_owned_control_with_permit(
        &self,
        area: &str,
        name: &str,
        permit: Option<&Arc<super::WorkPermit>>,
    ) -> Result<ControlRemoval> {
        if !matches!(area, "guards" | "owners") {
            return Err(Error::invalid("unknown control-file area"));
        }
        let directory = self.directory.child(area)?;
        let mut record = self.read(|connection| control_row(connection, area, name))?;
        if record.file.removed {
            return Ok(ControlRemoval::default());
        }
        self.fault(super::faults::Point::ControlObserved, None)?;
        let seal = record.file.seal.clone().ok_or_else(|| {
            Error::new(
                ErrorCategory::RecoveryRequired,
                "control-content-unsealed",
                "control deletion requires complete producer-intended content proof",
            )
        })?;
        seal.validate()?;
        if seal.length != record.file.logical_bytes
            || seal.length > 4096
            || record.file.pending_length.is_some() != record.file.pending_manifest.is_some()
            || record
                .file
                .pending_manifest
                .is_some_and(|manifest| manifest != seal.manifest)
        {
            return Err(Error::corrupt(
                "control deletion intent or original seal differs",
            ));
        }
        let policy = self.policy()?;
        let allocation = self.allocation()?;
        let _memory = match permit {
            Some(permit) => permit
                .memory(super::verification::MEMORY_BYTES)?
                .retain(0)?,
            None => self.verification_scratch(super::verification::MEMORY_BYTES)?,
        };
        let recheck = |native: &super::authentication_native::NativeFile| -> Result<()> {
            native.check()?;
            if FileIdentity::of(&native.file)? != record.file.identity
                || native.file.metadata()?.len() != record.file.logical_bytes
                || super::Ownership::capture(&native.file)? != record.file.ownership
                || directory.observe_file(name)?.identity != record.file.identity
            {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "control-file-modified",
                    "the control file identity, ownership or length changed and was preserved",
                ));
            }
            native.check()
        };
        let mut verification_bytes = 0;
        let recovered = match super::authentication_native::NativeFile::open(&directory, name) {
            Ok(mut native) => {
                recheck(&native)?;
                let mut bytes = vec![0; seal.length as usize];
                native.file.read_exact(&mut bytes)?;
                verification_bytes = bytes.len() as u64;
                if *blake3::hash(&bytes).as_bytes() != record.checksum
                    || super::MemberSeal::bounded(&bytes)? != seal
                {
                    return Err(Error::new(
                        ErrorCategory::StaleIdentity,
                        "control-file-modified",
                        "the inventoried control file was externally modified; it was preserved",
                    ));
                }
                recheck(&native)?;
                self.transaction(|transaction| {
                    let mut current = control_row(transaction, area, name)?;
                    if current.file.removed {
                        return Ok(());
                    }
                    current.file.pending_length = Some(current.file.logical_bytes);
                    current.file.pending_manifest = Some(seal.manifest);
                    transaction.execute(
                        "UPDATE control_files SET record=?3 WHERE area=?1 AND name=?2",
                        params![area, name, text(&current)?],
                    )?;
                    Ok(())
                })?;
                self.fault(super::faults::Point::ControlIntentSaved, None)
                    .map_err(|error| error.committed(super::CommitState::Committed))?;
                let committed = |error: Error| error.committed(super::CommitState::Committed);
                let uncertain = |error: Error| error.committed(super::CommitState::Unknown);
                if let Some(permit) = permit {
                    permit.check_now().map_err(committed)?;
                }
                let current_policy = self.policy().map_err(committed)?.version;
                let current_allocation = self.allocation().map_err(committed)?.version;
                if current_policy != policy.version || current_allocation != allocation.version {
                    return Err(Error::stale_version(if current_policy != policy.version {
                        current_policy
                    } else {
                        current_allocation
                    })
                    .committed(super::CommitState::Committed));
                }
                recheck(&native).map_err(committed)?;
                directory
                    .unlink_verified_file(name, &record.file.identity, &native.file)
                    .map_err(|error| error.committed(super::CommitState::Committed))?;
                native
                    .after_unlink()
                    .map_err(|error| error.committed(super::CommitState::Unknown))?;
                native
                    .check()
                    .map_err(|error| error.committed(super::CommitState::Unknown))?;
                if FileIdentity::of(&native.file).map_err(uncertain)? != record.file.identity
                    || super::Ownership::after_unlink(&native.file).map_err(uncertain)?
                        != record.file.ownership
                {
                    return Err(Error::new(
                        ErrorCategory::StaleIdentity,
                        "control-file-modified",
                        "control identity or ownership changed during unlink",
                    )
                    .committed(super::CommitState::Unknown));
                }
                drop(native);
                directory.confirm_unlinked_file(name).map_err(uncertain)?;
                self.fault(super::faults::Point::ControlAfterRemove, None)
                    .map_err(|error| error.committed(super::CommitState::Committed))?;
                false
            }
            Err(error) if error.source_io_kind() == Some(std::io::ErrorKind::NotFound) => {
                let current = self.read(|connection| control_row(connection, area, name))?;
                if current.file.identity != record.file.identity
                    || current.file.ownership != record.file.ownership
                    || current.file.seal != record.file.seal
                    || current.checksum != record.checksum
                {
                    return Err(Error::new(
                        ErrorCategory::StaleIdentity,
                        "control-file-replaced",
                        "missing control entry has different incarnation or producer evidence",
                    ));
                }
                if current.file.removed {
                    return Ok(ControlRemoval::default());
                }
                if current.file.pending_length != Some(current.file.logical_bytes)
                    || current.file.pending_manifest != Some(seal.manifest)
                {
                    return Err(error);
                }
                record = current;
                true
            }
            Err(error) => return Err(error),
        };
        self.fault(super::faults::Point::ControlBeforeCredit, None)
            .map_err(|error| error.committed(super::CommitState::Committed))?;
        self.transaction(|transaction| {
            record = control_row(transaction, area, name)?;
            if record.file.removed {
                return Ok(ControlRemoval::default());
            }
            let bytes = record
                .file
                .pending_length
                .ok_or_else(|| Error::corrupt("control unlink lost its intent"))?;
            record.file.removed = true;
            record.file.pending_length = None;
            record.file.pending_manifest = None;
            record.file.credited_logical_bytes = bytes;
            record.file.credited_allocated_bytes = match record.file.allocated_bytes {
                Measurement::Observed { value } => value,
                Measurement::Unavailable { .. } => 0,
            };
            record.file.logical_bytes = 0;
            record.file.allocated_bytes = Measurement::Observed { value: 0 };
            transaction.execute(
                "UPDATE control_files SET record=?3 WHERE area=?1 AND name=?2",
                params![area, name, text(&record)?],
            )?;
            let previous: Option<String> = transaction
                .query_row(
                    "SELECT record FROM records WHERE kind='control-credit' AND id='total'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            let (logical, recovered_logical): (u64, u64) = previous
                .map(|value| serde_json::from_str(&value))
                .transpose()?
                .unwrap_or_default();
            let total = logical
                .checked_add(bytes)
                .ok_or_else(|| Error::corrupt("control byte credit overflow"))?;
            let recovered_total = recovered_logical
                .checked_add(if recovered { bytes } else { 0 })
                .ok_or_else(|| Error::corrupt("control recovered-byte credit overflow"))?;
            transaction.execute(
                "INSERT INTO records VALUES('control-credit','total',1,?1)
                 ON CONFLICT(kind,id) DO UPDATE SET version=version+1,record=excluded.record",
                [text(&(total, recovered_total))?],
            )?;
            Ok(ControlRemoval {
                removed: true,
                logical_bytes: if recovered { 0 } else { bytes },
                recovered_logical_bytes: if recovered { bytes } else { 0 },
                verification_bytes,
            })
        })
        .map_err(|error| error.committed(super::CommitState::Committed))
    }

    pub(super) fn cleanup_next(&self, phase: u8, after: &str) -> Result<Option<(String, String)>> {
        let (table, condition, numeric) = match phase {
            4 => ("object_history", "", true),
            5 => ("members", "", true),
            6 => ("objects", "", false),
            7 => ("operations", "", false),
            8 => ("records", "AND kind='lease'", false),
            9 => ("records", "AND kind='view'", false),
            10 => ("records", "AND kind='root'", false),
            11 => ("owners", "", false),
            12 => ("records", "AND kind='owner-bootstrap'", false),
            13 => ("scopes", "", false),
            _ => return Err(Error::invalid("invalid metadata cleanup phase")),
        };
        let key = if numeric { "rowid" } else { "id" };
        let bound = if numeric { "CAST(?1 AS INTEGER)" } else { "?1" };
        let record = if phase == 13 {
            "json_object('closed',closed)"
        } else {
            "record"
        };
        self.read(|connection| Ok(connection.query_row(
            &format!("SELECT CAST({key} AS TEXT),{record} FROM {table} WHERE {key}>{bound} {condition} ORDER BY {key} LIMIT 1"),
            [after], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?))
    }

    fn scope_can_retire(&self, scope: &Id) -> Result<bool> {
        if scope == self.instance() || scope == &self.header().namespace {
            return Ok(false);
        }
        let (closed, owner) = self.read(|connection| {
            let closed: Option<bool> = connection
                .query_row(
                    "SELECT closed FROM scopes WHERE id=?1",
                    [scope.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            let owner: Option<String> = connection
                .query_row(
                    "SELECT record FROM owners WHERE id=?1",
                    [scope.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            Ok((closed.unwrap_or(true), owner))
        })?;
        if !closed {
            return Ok(false);
        }
        let Some(owner) = owner else {
            return Ok(true);
        };
        let owner: OwnerRecord = serde_json::from_str(&owner)?;
        if !owner.released {
            return Ok(false);
        }
        let control =
            self.read(|connection| control_row(connection, "owners", &format!("{scope}.lock")))?;
        if control.file.removed {
            return Ok(true);
        }
        if control.file.pending_length.is_some() {
            match self.owners.open_file(&format!("{scope}.lock"), false) {
                Err(error) if error.source_io_kind() == Some(std::io::ErrorKind::NotFound) => {
                    return Ok(true);
                }
                Err(error) => return Err(error),
                Ok(_) => {}
            }
        }
        Ok(self.inspect_owner(scope)?.last_proof == super::OwnerProof::Ended)
    }

    pub(super) fn cleanup_row(
        &self,
        phase: u8,
        id: &str,
        encoded: &str,
        counts: &mut CleanupCounts,
    ) -> Result<()> {
        match phase {
            4 => {
                let mut cursors = self
                    .cursors
                    .lock()
                    .map_err(|_| Error::corrupt("cursor registry poisoned"))?;
                cursors.retain(|_, cursor| cursor.expires > Instant::now());
                let floor = cursors
                    .values()
                    .map(|cursor| cursor.revision)
                    .min()
                    .unwrap_or(i64::MAX as u64);
                counts.history_rows += self.transaction(|transaction| Ok(transaction.execute(
                    "DELETE FROM object_history WHERE rowid=CAST(?1 AS INTEGER) AND revision<?2
                     AND EXISTS(SELECT 1 FROM object_history newer WHERE newer.object_id=object_history.object_id
                         AND newer.revision>object_history.revision AND newer.revision<=?2)",
                    params![id, super::catalog::sql_integer(floor)?],
                )? as u32))?;
            }
            5 => {
                counts.proof_rows += self.transaction(|transaction| Ok(transaction.execute(
                    "DELETE FROM member_seals WHERE rowid IN (
                      SELECT s.rowid FROM member_seals s
                      JOIN members m ON m.object_id=s.object_id AND m.name=s.name
                      JOIN objects o ON o.id=m.object_id
                      WHERE m.rowid=CAST(?1 AS INTEGER) AND
                        (o.state='removed' OR (o.state='quarantined' AND json_extract(m.record,'$.seal') IS NULL))
                      ORDER BY s.block_index LIMIT ?2)",
                    params![id, super::authentication::PROOF_BATCH as u32],
                )? as u32))?;
                counts.member_rows += self.transaction(|transaction| Ok(transaction.execute(
                    "DELETE FROM members WHERE rowid=CAST(?1 AS INTEGER)
                     AND EXISTS(SELECT 1 FROM objects WHERE id=members.object_id AND state='removed')
                     AND NOT EXISTS(SELECT 1 FROM member_seals WHERE object_id=members.object_id AND name=members.name)", [id],
                )? as u32))?;
            }
            6 => {
                let object: super::ObjectRecord = serde_json::from_str(encoded)?;
                if object.state != ObjectState::Removed {
                    return Ok(());
                }
                let mut cursors = self
                    .cursors
                    .lock()
                    .map_err(|_| Error::corrupt("cursor registry poisoned"))?;
                cursors.retain(|_, cursor| cursor.expires > Instant::now());
                if cursors
                    .values()
                    .any(|cursor| cursor.revision < object.revision)
                {
                    return Ok(());
                }
                counts.objects += self.transaction(|transaction| {
                    let blocked: bool = transaction.query_row(
                        "SELECT EXISTS(SELECT 1 FROM members WHERE object_id=?1)
                         OR EXISTS(SELECT 1 FROM member_creations WHERE object_id=?1)
                         OR EXISTS(SELECT 1 FROM refs WHERE target=?1)
                         OR EXISTS(SELECT 1 FROM control_files WHERE source=?1 AND json_extract(record,'$.file.removed')=0)",
                        [id], |row| row.get(0),
                    )?;
                    if blocked { return Ok(0); }
                    transaction.execute("DELETE FROM object_history WHERE object_id=?1", [id])?;
                    transaction.execute("DELETE FROM control_files WHERE source=?1", [id])?;
                    Ok(transaction.execute("DELETE FROM objects WHERE id=?1 AND state='removed'", [id])? as u32)
                })?;
            }
            7 => {
                let operation: OperationRecord = serde_json::from_str(encoded)?;
                if !matches!(
                    operation.state,
                    OperationState::Completed | OperationState::Failed | OperationState::Cancelled
                ) || !self.scope_can_retire(&operation.token.scope)?
                {
                    return Ok(());
                }
                let readers = self
                    .operation_readers
                    .lock()
                    .map_err(|_| Error::corrupt("receipt reader registry poisoned"))?;
                if readers.contains_key(&operation.id) {
                    return Ok(());
                }
                counts.receipts += self.transaction(|transaction| {
                    let blocked: bool = transaction.query_row(
                        "SELECT EXISTS(SELECT 1 FROM reservations WHERE operation_id=?1)
                         OR EXISTS(SELECT 1 FROM objects WHERE state='preparing' AND json_extract(record,'$.operation')=?1)
                         OR EXISTS(SELECT 1 FROM refs WHERE source_id=?1)",
                        [id], |row| row.get(0),
                    )?;
                    if blocked { return Ok(0); }
                    Ok(transaction.execute("DELETE FROM operations WHERE id=?1", [id])? as u32)
                })?;
            }
            8 => {
                let lease: super::LeaseRecord = serde_json::from_str(encoded)?;
                if !lease.released {
                    return Ok(());
                }
                let retired = match &lease.owner {
                    Some(owner) => self.scope_can_retire(owner)?,
                    None => lease.instance != *self.instance(),
                };
                if retired {
                    counts.leases += self.transaction(|transaction| Ok(transaction.execute(
                        "DELETE FROM records WHERE kind='lease' AND id=?1 AND json_extract(record,'$.released')=1", [id],
                    )? as u32))?;
                }
            }
            9 => {
                let live = self
                    .live_views
                    .lock()
                    .map_err(|_| Error::corrupt("view metadata registry poisoned"))?;
                if live
                    .get(&Id::parse(id)?)
                    .is_some_and(|views| views.iter().any(|view| view.strong_count() != 0))
                {
                    return Ok(());
                }
                let view: super::ViewRecord = serde_json::from_str(encoded)?;
                if view.active {
                    return Ok(());
                }
                let blocked = self.read(|connection| Ok(connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM records WHERE kind='lease' AND json_extract(record,'$.view')=?1)
                     OR EXISTS(SELECT 1 FROM refs WHERE source_kind='view' AND source_id=?1)
                     OR EXISTS(SELECT 1 FROM operations WHERE state IN ('accepted','preparing','cancelling')
                        AND json_extract(record,'$.request.view')=?1)",
                    [id], |row| row.get::<_, bool>(0),
                )?))?;
                if blocked {
                    return Ok(());
                }
                if let Some(anchor) = &view.root_anchor {
                    if self.root_busy(anchor)? {
                        return Ok(());
                    }
                    counts.root_retired(self.retire_root(anchor)?);
                } else if self.root_identity_busy(&view.root, &view.root_identity)? {
                    return Ok(());
                }
                counts.views += self.transaction(|transaction| {
                    transaction.execute(
                        "DELETE FROM records WHERE kind='view-root' AND record=?1",
                        [text(&view.id)?],
                    )?;
                    transaction
                        .execute("DELETE FROM records WHERE kind='adaptive' AND id=?1", [id])?;
                    Ok(transaction.execute(
                        "DELETE FROM records WHERE kind='view' AND id=?1
                        AND json_extract(record,'$.active')=0",
                        [id],
                    )? as u32)
                })?;
            }
            10 => {
                let anchor: super::RootAnchor = serde_json::from_str(encoded)?;
                if !anchor.retired {
                    return Ok(());
                }
                counts.root_retired(self.retire_root(&anchor)?);
                counts.roots += self.transaction(|transaction| {
                    let blocked: bool = transaction.query_row(
                        "SELECT EXISTS(SELECT 1 FROM records WHERE kind='view' AND json_extract(record,'$.root_anchor.guard')=?1)
                         OR EXISTS(SELECT 1 FROM control_files WHERE source=?1 AND json_extract(record,'$.file.removed')=0)",
                        [id], |row| row.get(0),
                    )?;
                    if blocked { return Ok(0); }
                    transaction.execute("DELETE FROM control_files WHERE source=?1", [id])?;
                    transaction.execute("DELETE FROM records WHERE kind='root-current' AND record=?1", [text(&anchor.guard)?])?;
                    Ok(transaction.execute("DELETE FROM records WHERE kind='root' AND id=?1", [id])? as u32)
                })?;
            }
            11 => {
                let owner: OwnerRecord = serde_json::from_str(encoded)?;
                if !owner.released {
                    return Ok(());
                }
                let blocked = self.read(|connection| Ok(connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM records WHERE kind='lease' AND json_extract(record,'$.owner')=?1)
                     OR EXISTS(SELECT 1 FROM operations WHERE scope=?1)
                     OR EXISTS(SELECT 1 FROM reservations WHERE owner=?1)
                     OR EXISTS(SELECT 1 FROM refs WHERE owner=?1)",
                    [id], |row| row.get::<_, bool>(0),
                )?))?;
                if blocked {
                    return Ok(());
                }
                let name = format!("{id}.lock");
                let guard_record =
                    self.read(|connection| control_row(connection, "owners", &name))?;
                let guard = if guard_record.file.removed {
                    None
                } else {
                    let guard = match self.owners.open_file(&name, true) {
                        Ok(guard) => Some(guard),
                        Err(error)
                            if error.source_io_kind() == Some(std::io::ErrorKind::NotFound)
                                && guard_record.file.pending_length.is_some() =>
                        {
                            None
                        }
                        Err(error) => return Err(error),
                    };
                    if let Some(guard) = guard {
                        if FileIdentity::of(&guard)? != owner.claim.guard_identity {
                            return Err(Error::corrupt(
                                "retired owner guard was externally replaced",
                            ));
                        }
                        match fs2::FileExt::try_lock_exclusive(&guard) {
                            Ok(()) => Some(guard),
                            Err(error) => {
                                let error = lock_error(error, "owner-lifetime-held");
                                if error.category == ErrorCategory::Busy {
                                    return Ok(());
                                }
                                return Err(error);
                            }
                        }
                    } else {
                        None
                    }
                };
                counts.control_removed(self.remove_owned_control("owners", &format!("{id}.json"))?);
                drop(guard);
                counts.control_removed(self.remove_owned_control("owners", &name)?);
                counts.owners += self.transaction(|transaction| {
                    transaction.execute("DELETE FROM control_files WHERE source=?1", [id])?;
                    Ok(transaction.execute("DELETE FROM owners WHERE id=?1", [id])? as u32)
                })?;
            }
            12 => {
                if !id.starts_with(&format!("{}:", self.instance())) {
                    counts.bootstrap_receipts += self.transaction(|transaction| {
                        Ok(transaction.execute(
                            "DELETE FROM records WHERE kind='owner-bootstrap' AND id=?1",
                            [id],
                        )? as u32)
                    })?;
                }
            }
            13 => {
                let scope = Id::parse(id)?;
                if scope == self.header().namespace || scope == *self.instance() {
                    return Ok(());
                }
                counts.scopes += self.transaction(|transaction| {
                    Ok(transaction.execute(
                        "DELETE FROM scopes WHERE id=?1 AND closed=1
                     AND NOT EXISTS(SELECT 1 FROM operations WHERE scope=?1)
                     AND NOT EXISTS(SELECT 1 FROM owners WHERE id=?1)",
                        [id],
                    )? as u32)
                })?;
            }
            _ => return Err(Error::invalid("invalid metadata cleanup phase")),
        }
        Ok(())
    }

    pub(super) fn control_logical_bytes(&self) -> Result<u64> {
        self.read(|connection| {
            Ok(connection.query_row(
                "SELECT coalesce(sum(json_extract(record,'$.file.logical_bytes')),0)
             FROM control_files WHERE json_extract(record,'$.file.removed')=0",
                [],
                |row| unsigned(row, 0),
            )?)
        })
    }
}
