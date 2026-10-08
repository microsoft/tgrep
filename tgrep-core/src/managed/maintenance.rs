// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::{object_row, save_object, text, unsigned};
use super::lifetime::{ActivityGuard, lock_error};
use super::{
    CommitState, Error, ErrorCategory, FileIdentity, Id, Namespace, ObjectState, OperationRecord,
    OperationState, OwnerProof, ReservationRecord, Result,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryCursor {
    pub phase: u8,
    pub after: String,
    pub reservation: Option<Id>,
    pub object_after: String,
    pub object: Option<Id>,
    pub member_after: String,
}

impl RecoveryCursor {
    fn validate(&self) -> Result<()> {
        if self.phase > 13
            || self.after.len() > 512
            || self.object_after.len() > 32
            || self.member_after.len() > 256
        {
            return Err(Error::invalid(
                "recovery cursor exceeds its phase or identity bounds",
            ));
        }
        if matches!(self.phase, 4 | 5)
            && !self.after.is_empty()
            && self.after.parse::<u64>().is_err()
        {
            return Err(Error::invalid("invalid recovery row cursor"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRequest {
    pub cursor: Option<RecoveryCursor>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceIssue {
    pub kind: String,
    pub identity: String,
    pub error: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RecoveryProgress {
    pub examined: u32,
    pub owners_reaped: u32,
    pub reservations_released: u32,
    pub members_refreshed: u32,
    pub objects_retired: u32,
    pub objects_quarantined: u32,
    pub views_retired: u32,
    pub operations_recovered: u32,
    pub cleanup: super::CleanupCounts,
    pub cancelled: bool,
    pub measurements_complete: bool,
    pub mutation_committed: bool,
    pub issues: Vec<MaintenanceIssue>,
    pub next: Option<RecoveryCursor>,
    pub elapsed_nanos: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalWork {
    pub queries: u64,
    pub queued_jobs: u64,
    pub requests: u64,
    pub background_batches: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IdleOutcome {
    pub stopping: bool,
    pub namespace: Id,
    pub instance: Id,
    pub leases: u64,
    pub owners: u64,
    pub reservations: u64,
    pub operations: u64,
    pub operation_readers: u64,
    pub namespace_readers_or_work: bool,
    pub external: ExternalWork,
    pub committed_state: CommitState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceRecord {
    instance: Id,
    stopping: bool,
}

impl Namespace {
    /// The caller already holds external namespace ownership. Activating a new
    /// instance does not restore readiness or expire old managed owners.
    pub fn activate(&self) -> Result<()> {
        let _serial = self
            .maintenance
            .try_lock()
            .map_err(|_| Error::busy("namespace-maintenance-active"))?;
        self.transaction(|transaction| {
            let previous: Option<String> = transaction.query_row(
                "SELECT record FROM records WHERE kind='service' AND id='current'", [], |row| row.get(0),
            ).optional()?;
            if let Some(previous) = previous {
                let previous: serde_json::Value = serde_json::from_str(&previous)?;
                if previous["instance"] == serde_json::to_value(self.instance())? && previous["stopping"] == true {
                    return Err(Error::busy("stopped-instance-cannot-reactivate"));
                }
            }
            transaction.execute("UPDATE state SET admission='open' WHERE singleton=1", [])?;
            // Preserve v1's instance boundary, not an idle timeout or owner-death heuristic.
            transaction.execute(
                "UPDATE records SET version=version+1,record=json_set(record,'$.released',json('true'))
                 WHERE kind='lease' AND json_extract(record,'$.owner') IS NULL
                 AND json_extract(record,'$.released')=0 AND json_extract(record,'$.instance')!=?1",
                [self.instance().as_str()],
            )?;
            transaction.execute("UPDATE scopes SET closed=1 WHERE id!=?1 AND id!=?2 AND id NOT IN (SELECT id FROM owners)",
                params![self.instance().as_str(),self.header().namespace.as_str()])?;
            transaction.execute(
                "INSERT INTO records VALUES('service','current',1,?1) ON CONFLICT(kind,id) DO UPDATE SET version=version+1,record=excluded.record",
                [text(&serde_json::json!({"instance":self.instance(),"stopping":false}))?],
            )?;
            transaction.execute("INSERT INTO scopes(id) VALUES(?1) ON CONFLICT(id) DO NOTHING", [self.instance().as_str()])?;
            transaction.execute("INSERT INTO scopes(id) VALUES(?1) ON CONFLICT(id) DO NOTHING", [self.header().namespace.as_str()])?;
            Ok(())
        })
    }

    pub fn pending_operations(&self, after: Option<&Id>) -> Result<Vec<OperationRecord>> {
        let limit = self.policy()?.policy.work.page_objects;
        self.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT record FROM operations WHERE id>?1 AND state IN ('accepted','preparing','cancelling') ORDER BY id LIMIT ?2",
            )?;
            let rows = statement.query_map(params![after.map_or("", Id::as_str), limit], |row| row.get::<_, String>(0))?;
            rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
        })
    }

    pub fn owner_page(&self, after: Option<&Id>) -> Result<Vec<super::OwnerRecord>> {
        let limit = self.policy()?.policy.work.page_objects;
        self.read(|connection| {
            let mut statement =
                connection.prepare("SELECT record FROM owners WHERE id>?1 ORDER BY id LIMIT ?2")?;
            let rows = statement
                .query_map(params![after.map_or("", Id::as_str), limit], |row| {
                    row.get::<_, String>(0)
                })?;
            rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
        })
    }

    /// Close admission before looking for *any* protected category. A failed
    /// idle check reopens service; retained cache references alone are not work.
    pub fn stop_if_idle(&self, external: ExternalWork) -> Result<IdleOutcome> {
        self.stop_if_idle_inner(external, None)
    }

    /// Resolve shutdown authorization for the activated instance. A committed
    /// operation receipt or other catalog transaction alone is not a stop.
    pub fn stop_is_committed(&self) -> Result<bool> {
        self.read(|connection| {
            let state: Option<(String, String)> = connection
                .query_row(
                    "SELECT s.admission,r.record FROM state s JOIN records r
                     ON r.kind='service' AND r.id='current' WHERE s.singleton=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (admission, record) =
                state.ok_or_else(|| Error::corrupt("activated service record is missing"))?;
            let record: ServiceRecord = serde_json::from_str(&record)
                .map_err(|error| Error::corrupt(format!("invalid service record: {error}")))?;
            if record.instance != *self.instance() {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "service-instance-mismatch",
                    "the service record belongs to a different namespace instance",
                ));
            }
            if !matches!(admission.as_str(), "open" | "closed")
                || (record.stopping && admission != "closed")
            {
                return Err(Error::corrupt("service and admission state disagree"));
            }
            Ok(record.stopping)
        })
    }

    pub fn stop_if_idle_with_token(
        &self,
        token: super::OperationToken,
        external: ExternalWork,
    ) -> Result<IdleOutcome> {
        let operation =
            self.accept_maintenance_operation(token, "idle-stop", serde_json::json!({}))?;
        let _receipt = self.hold_operation(&operation.id)?;
        self.fault(super::faults::Point::IdleAccepted, Some(&operation.id))
            .map_err(|error| {
                error
                    .committed(CommitState::Committed)
                    .operation(operation.id.to_string())
            })?;
        if let Some(result) = operation.result {
            return Ok(serde_json::from_value(result)?);
        }
        if operation.instance != *self.instance() {
            return Err(Error::new(
                ErrorCategory::RecoveryRequired,
                "idle-stop-instance-ended",
                "inspect or recover the old operation; its token cannot stop a different instance",
            )
            .committed(operation.committed_state)
            .operation(operation.id.to_string()));
        }
        if let Some(error) = operation.error {
            return Err(serde_json::from_value(error)?);
        }
        self.check_operation_lifetime(&operation)?;
        let result = self.stop_if_idle_inner(external, Some(&operation.id));
        if let Err(error) = result {
            self.fail_operation(
                &operation.id,
                serde_json::from_value(serde_json::to_value(&error)?)?,
            )?;
            return Err(error);
        }
        result
    }

    fn stop_if_idle_inner(
        &self,
        external: ExternalWork,
        operation: Option<&Id>,
    ) -> Result<IdleOutcome> {
        let _serial = self
            .maintenance
            .try_lock()
            .map_err(|_| Error::busy("namespace-maintenance-active"))?;
        self.expire_cursors()?;
        let result = (|| {
            self.transaction(|transaction| {
                transaction.execute("UPDATE state SET admission='closed' WHERE singleton=1", [])?;
                Ok(())
            })?;
            self.fault(super::faults::Point::IdleAdmissionClosed, operation)?;
            let (leases, owners, reservations, operations) = self.read(|connection| {
                Ok(connection.query_row(
                    "SELECT
                     (SELECT count(*) FROM records WHERE kind='lease' AND json_extract(record,'$.released')=0),
                     (SELECT count(*) FROM owners WHERE json_extract(record,'$.released')=0),
                     (SELECT count(*) FROM reservations),
                     (SELECT count(*) FROM operations WHERE state IN ('accepted','preparing','cancelling')
                        AND (?1 IS NULL OR id!=?1))",
                    [operation.map(Id::as_str)], |row| Ok((unsigned(row,0)?,unsigned(row,1)?,unsigned(row,2)?,unsigned(row,3)?)),
                )?)
            })?;
            let activity = self.directory.open_file("activity.lock", true)?;
            if FileIdentity::of(&activity)? != self.header().activity_identity {
                return Err(Error::corrupt("idle activity anchor was replaced"));
            }
            let namespace_readers_or_work = match fs2::FileExt::try_lock_exclusive(&activity) {
                Ok(()) => false,
                Err(error) => {
                    let error = lock_error(error, "namespace-readers-or-work");
                    if error.category != ErrorCategory::Busy {
                        return Err(error);
                    }
                    true
                }
            };
            let readers = self
                .operation_readers
                .lock()
                .map_err(|_| Error::corrupt("operation reader registry poisoned"))?;
            let operation_readers = readers
                .iter()
                .map(|(id, count)| {
                    u64::from(*count).saturating_sub(u64::from(operation == Some(id)))
                })
                .sum::<u64>();
            let stopping = leases == 0
                && owners == 0
                && reservations == 0
                && operations == 0
                && operation_readers == 0
                && !namespace_readers_or_work
                && external.queries == 0
                && external.queued_jobs == 0
                && external.requests == 0
                && external.background_batches == 0;
            let outcome = IdleOutcome {
                stopping,
                namespace: self.header().namespace.clone(),
                instance: self.instance().clone(),
                leases,
                owners,
                reservations,
                operations,
                operation_readers,
                namespace_readers_or_work,
                external,
                committed_state: if stopping {
                    CommitState::Committed
                } else {
                    CommitState::NotCommitted
                },
            };
            self.transaction(|transaction| {
                if stopping {
                    transaction.execute(
                        "INSERT INTO records VALUES('service','current',1,?1) ON CONFLICT(kind,id) DO UPDATE SET version=version+1,record=excluded.record",
                        [text(&serde_json::json!({"instance":self.instance(),"stopping":true}))?],
                    )?;
                } else {
                    transaction.execute("UPDATE state SET admission='open' WHERE singleton=1", [])?;
                }
                if let Some(id) = operation {
                    let encoded: String = transaction.query_row("SELECT record FROM operations WHERE id=?1",
                        [id.as_str()], |row| row.get(0))?;
                    let mut operation: OperationRecord = serde_json::from_str(&encoded)?;
                    if operation.cancelled {
                        return Err(Error::new(ErrorCategory::Cancelled, "idle-stop-cancelled", "stop was cancelled before commitment"));
                    }
                    operation.state = OperationState::Completed;
                    operation.committed_state = outcome.committed_state;
                    operation.result = Some(serde_json::to_value(&outcome)?);
                    Self::save_operation(transaction, &operation)?;
                }
                Ok(())
            })?;
            self.fault(super::faults::Point::IdleCommitted, operation)
                .map_err(|error| error.committed(outcome.committed_state))?;
            Ok(outcome)
        })();
        if let Err(original) = &result {
            let stopping = self.stop_is_committed().map_err(|error| {
                Error::new(
                    ErrorCategory::RecoveryRequired,
                    "idle-state-unavailable",
                    format!("idle attempt: {original}; reading shutdown authorization: {error}"),
                )
                .committed(CommitState::Unknown)
            })?;
            if !stopping {
                self.transaction(|transaction| {
                    transaction.execute("UPDATE state SET admission='open' WHERE singleton=1", [])?;
                    Ok(())
                })
                .map_err(|error| {
                    Error::new(
                        ErrorCategory::RecoveryRequired,
                        "idle-admission-recovery",
                        format!("stop was not committed; idle attempt: {original}; restoring admission: {error}"),
                    )
                })?;
            }
        }
        result
    }

    /// Storage-only, bounded recovery. It never resolves a Git revision or
    /// assumes that a missing worktree proves the end of a client's lifetime.
    pub fn recover_pass(&self, cursor: Option<RecoveryCursor>) -> Result<RecoveryProgress> {
        let _serial = self
            .maintenance
            .try_lock()
            .map_err(|_| Error::busy("namespace-maintenance-active"))?;
        self.recover_pass_inner(cursor, None)
    }

    pub fn recover_with_token(
        &self,
        token: super::OperationToken,
        request: RecoveryRequest,
    ) -> Result<OperationRecord> {
        if let Some(cursor) = &request.cursor {
            cursor.validate()?;
        }
        let operation =
            self.accept_maintenance_operation(token, "recovery", serde_json::to_value(&request)?)?;
        let _receipt = self.hold_operation(&operation.id)?;
        if matches!(
            operation.state,
            OperationState::Completed | OperationState::Failed | OperationState::Cancelled
        ) {
            return Ok(operation);
        }
        if operation.instance != *self.instance() {
            return Err(Error::new(
                ErrorCategory::RecoveryRequired,
                "recovery-instance-ended",
                "inspect the old receipt and resume bounded recovery with a new namespace token",
            )
            .operation(operation.id.to_string())
            .committed(operation.committed_state));
        }
        if let Err(error) = self.check_operation_lifetime(&operation) {
            return self.fail_operation(&operation.id, error);
        }
        let _serial = self.maintenance.try_lock().map_err(|_| {
            Error::busy("namespace-maintenance-active").operation(operation.id.to_string())
        })?;
        self.transaction(|transaction| {
            let encoded: String = transaction.query_row(
                "SELECT record FROM operations WHERE id=?1",
                [operation.id.as_str()],
                |row| row.get(0),
            )?;
            let mut current: OperationRecord = serde_json::from_str(&encoded)?;
            if current.cancelled {
                return Err(Error::new(
                    ErrorCategory::Cancelled,
                    "recovery-cancelled",
                    "recovery was cancelled before execution",
                ));
            }
            current.state = OperationState::Preparing;
            // Recovery can commit independent units. Until a complete result is
            // durable, an interrupted pass must not claim that none took effect.
            current.committed_state = CommitState::Unknown;
            current.progress = serde_json::json!({"request":request,"measurements_complete":false});
            Self::save_operation(transaction, &current)
        })?;
        let progress = match self.recover_pass_inner(request.cursor, Some(&operation.id)) {
            Ok(progress) => progress,
            Err(error) => return self.fail_operation(&operation.id, error),
        };
        self.transaction(|transaction| {
            let encoded: String = transaction.query_row(
                "SELECT record FROM operations WHERE id=?1",
                [operation.id.as_str()],
                |row| row.get(0),
            )?;
            let mut current: OperationRecord = serde_json::from_str(&encoded)?;
            let changed = progress.mutation_committed
                || progress.owners_reaped != 0
                || progress.reservations_released != 0
                || progress.views_retired != 0
                || progress.operations_recovered != 0
                || progress.members_refreshed != 0
                || progress.objects_retired != 0
                || progress.objects_quarantined != 0
                || progress.cleanup != super::CleanupCounts::default();
            current.state = if progress.cancelled {
                OperationState::Cancelled
            } else {
                OperationState::Completed
            };
            current.committed_state = if changed {
                CommitState::Committed
            } else if !progress.measurements_complete {
                CommitState::Unknown
            } else {
                CommitState::NotCommitted
            };
            current.progress = serde_json::to_value(&progress)?;
            current.result = Some(serde_json::to_value(&progress)?);
            Self::save_operation(transaction, &current)
        })
        .map_err(|error| self.operation_error(&operation.id, error))?;
        self.operation(&operation.id)
    }

    fn recover_pass_inner(
        &self,
        cursor: Option<RecoveryCursor>,
        operation: Option<&Id>,
    ) -> Result<RecoveryProgress> {
        let observation = self.observe_pass(super::diagnostics::Kind::Recovery)?;
        let result = self.recover_units(cursor, operation);
        observation.recovery(&result)?;
        result
    }

    fn recover_units(
        &self,
        cursor: Option<RecoveryCursor>,
        operation: Option<&Id>,
    ) -> Result<RecoveryProgress> {
        let _activity = ActivityGuard::acquire(&self.directory)?;
        let policy = self.policy()?.policy;
        let started = Instant::now();
        let deadline = started
            .checked_add(Duration::from_millis(policy.collection.max_duration_ms))
            .ok_or_else(|| Error::invalid("recovery deadline overflow"))?;
        let mut cursor = cursor.unwrap_or_default();
        cursor.validate()?;
        let mut progress = RecoveryProgress {
            examined: 0,
            owners_reaped: 0,
            reservations_released: 0,
            views_retired: 0,
            members_refreshed: 0,
            objects_retired: 0,
            objects_quarantined: 0,
            operations_recovered: 0,
            cleanup: super::CleanupCounts::default(),
            cancelled: false,
            measurements_complete: true,
            mutation_committed: false,
            issues: Vec::new(),
            next: None,
            elapsed_nanos: 0,
        };
        while progress.examined < policy.collection.max_examined.min(policy.work.page_objects)
            && progress
                .cleanup
                .control_logical_bytes_reclaimed
                .saturating_add(4096)
                <= policy.collection.max_delete_bytes
            && Instant::now() < deadline
        {
            if let Some(operation) = operation {
                let record = self.operation(operation)?;
                if record.cancelled {
                    progress.cancelled = true;
                    break;
                }
                self.check_operation_lifetime(&record)?;
            }
            if cursor.phase >= 4 {
                if cursor.phase == 14 {
                    break;
                }
                let Some((id, encoded)) = self.cleanup_next(cursor.phase, &cursor.after)? else {
                    cursor.phase += 1;
                    cursor.after.clear();
                    continue;
                };
                progress.examined += 1;
                if let Err(error) =
                    self.cleanup_row(cursor.phase, &id, &encoded, &mut progress.cleanup)
                {
                    progress.mutation_committed |= error.committed_state == CommitState::Committed;
                    if error.committed_state != CommitState::NotCommitted {
                        progress.measurements_complete = false;
                    }
                    progress.issues.push(MaintenanceIssue {
                        kind: "catalog-cleanup".into(),
                        identity: id.clone(),
                        error: serde_json::to_value(error)?,
                    });
                }
                cursor.after = id;
                continue;
            }
            let table = match cursor.phase {
                0 => "owners",
                1 => "reservations",
                2 => "records",
                3 => "operations",
                _ => break,
            };
            let condition = if cursor.phase == 2 {
                "AND kind='view'"
            } else {
                ""
            };
            let next: Option<(String, String)> = self.read(|connection| {
                Ok(connection.query_row(
                    &format!("SELECT id,record FROM {table} WHERE id>?1 {condition} ORDER BY id LIMIT 1"),
                    [&cursor.after], |row| Ok((row.get(0)?, row.get(1)?)),
                ).optional()?)
            })?;
            let Some((id, encoded)) = next else {
                cursor.phase += 1;
                cursor.after.clear();
                continue;
            };
            progress.examined += 1;
            let result = match cursor.phase {
                0 => (|| {
                    let owner: super::OwnerRecord = serde_json::from_str(&encoded)?;
                    if owner.released {
                        return Ok(true);
                    }
                    if owner.registered
                        && self.inspect_owner(&owner.claim.owner)?.last_proof == OwnerProof::Ended
                    {
                        self.reap_owner(&owner.claim.owner)?;
                        progress.owners_reaped += 1;
                    } else if !owner.registered && owner.claim.instance != *self.instance() {
                        // Old, never-authorized bootstrap claims cannot register in this instance.
                        let file = self
                            .owners
                            .open_file(&format!("{}.lock", owner.claim.owner), true)?;
                        if FileIdentity::of(&file)? != owner.claim.guard_identity {
                            return Err(Error::corrupt("bootstrap guard replaced"));
                        }
                        fs2::FileExt::try_lock_exclusive(&file)
                            .map_err(|error| lock_error(error, "bootstrap-guard-held"))?;
                        self.transaction(|transaction| {
                            let mut owner = owner;
                            owner.released = true;
                            transaction.execute(
                                "UPDATE owners SET record=?2 WHERE id=?1",
                                params![id, text(&owner)?],
                            )?;
                            transaction.execute("UPDATE scopes SET closed=1 WHERE id=?1", [&id])?;
                            Ok(())
                        })?;
                        progress.owners_reaped += 1;
                    }
                    Ok(true)
                })(),
                1 => (|| {
                    let reservation: ReservationRecord = serde_json::from_str(&encoded)?;
                    if reservation.instance == *self.instance() && !reservation.producer_ended {
                        return Ok(true);
                    }
                    if cursor.reservation.as_ref() != Some(&reservation.id) {
                        cursor.object_after.clear();
                        cursor.object = None;
                        cursor.member_after.clear();
                    }
                    cursor.reservation = Some(reservation.id.clone());
                    let object: Option<String> = if let Some(object) = &cursor.object {
                        Some(object.to_string())
                    } else {
                        self.read(|connection| {
                        Ok(connection.query_row(
                            "SELECT id FROM objects WHERE id>?1 AND json_extract(record,'$.operation')=?2 AND state!='removed' ORDER BY id LIMIT 1",
                            params![cursor.object_after, reservation.operation.as_str()], |row| row.get(0),
                        ).optional()?)
                    })?
                    };
                    if let Some(object) = object {
                        let object_id = Id::parse(object.clone())?;
                        let record = self.object(&object_id)?;
                        if record.operation.as_ref() != Some(&reservation.operation) {
                            return Err(Error::invalid(
                                "recovery cursor names another reservation's object",
                            ));
                        }
                        let unsealed_creation: Option<String> = self.read(|connection| Ok(connection.query_row(
                            "SELECT name FROM member_creations WHERE object_id=?1 ORDER BY name LIMIT 1",
                            [object_id.as_str()], |row| row.get(0),
                        ).optional()?))?;
                        if record.guard_identity.is_none() || record.directory_identity.is_none() {
                            if record.state == ObjectState::Published {
                                return Err(Error::corrupt(
                                    "published object has an incomplete ownership seal",
                                ));
                            }
                            self.quarantine_unsealed(
                                &object_id,
                                reservation.request.staging_bytes,
                            )?;
                            progress.objects_quarantined += 1;
                            cursor.object = None;
                            cursor.member_after.clear();
                            cursor.object_after = object;
                            return Ok(false);
                        }
                        if unsealed_creation.is_some()
                            && matches!(
                                record.state,
                                ObjectState::Published | ObjectState::PendingDeletion
                            )
                        {
                            return Err(Error::corrupt(
                                "published or deleting object has an incomplete member creation",
                            ));
                        }
                        if matches!(record.state, ObjectState::Preparing | ObjectState::Retired) {
                            let expected = record
                                .guard_identity
                                .as_ref()
                                .ok_or_else(|| Error::corrupt("unsealed recovery object guard"))?;
                            let guard =
                                self.guards.open_file(&format!("{object_id}.lock"), true)?;
                            if FileIdentity::of(&guard)? != *expected {
                                return Err(Error::corrupt("recovery object guard differs"));
                            }
                            fs2::FileExt::try_lock_exclusive(&guard).map_err(|error| {
                                lock_error(error, "recovery-producer-or-reader-held")
                            })?;
                            let directory = self.objects.child(object_id.as_str())?;
                            if record.directory_identity.as_ref() != Some(&directory.identity()?) {
                                return Err(Error::corrupt("recovery object directory differs"));
                            }
                            if let Some(name) = unsealed_creation {
                                match directory.open_file(&name, false) {
                                    Ok(_) => {
                                        self.quarantine_unsealed(
                                            &object_id,
                                            reservation.request.staging_bytes,
                                        )?;
                                        progress.objects_quarantined += 1;
                                        cursor.object = None;
                                        cursor.member_after.clear();
                                        cursor.object_after = object;
                                    }
                                    Err(error)
                                        if error.source_io_kind()
                                            == Some(std::io::ErrorKind::NotFound) =>
                                    {
                                        self.transaction(|transaction| {
                                            transaction.execute("DELETE FROM member_creations WHERE object_id=?1 AND name=?2",
                                                params![object_id.as_str(), name])?;
                                            Ok(())
                                        })?;
                                        progress.mutation_committed = true;
                                        cursor.object = Some(object_id);
                                    }
                                    Err(error) => return Err(error),
                                }
                                return Ok(false);
                            }
                            let member: Option<String> = self.read(|connection| Ok(connection.query_row(
                                "SELECT record FROM members WHERE object_id=?1 AND name>?2 ORDER BY name LIMIT 1",
                                params![object_id.as_str(),cursor.member_after], |row| row.get(0),
                            ).optional()?))?;
                            if let Some(member) = member {
                                let member: super::FileRecord = serde_json::from_str(&member)?;
                                if !member.removed {
                                    let file = directory.open_file(&member.name, false)?;
                                    if FileIdentity::of(&file)? != member.identity {
                                        return Err(Error::new(
                                            ErrorCategory::StaleIdentity,
                                            "recovery-member-replaced",
                                            "known producer output was physically replaced",
                                        ));
                                    }
                                    if member.producer_open {
                                        self.record_file(&object_id, &member.name)?;
                                        progress.members_refreshed += 1;
                                    } else if member.pending_length.is_none()
                                        && (file.metadata()?.len() != member.logical_bytes
                                            || super::storage::file_change(&file)? != member.change)
                                    {
                                        let error = Error::new(
                                            ErrorCategory::StaleIdentity,
                                            "recovery-member-modified",
                                            "sealed producer output was externally modified and was preserved",
                                        );
                                        self.quarantine_object(
                                            &object_id,
                                            reservation.request.staging_bytes,
                                            &error,
                                        )?;
                                        progress.objects_quarantined += 1;
                                        progress.issues.push(MaintenanceIssue {
                                            kind: "staging".into(),
                                            identity: object_id.to_string(),
                                            error: serde_json::to_value(error)?,
                                        });
                                        cursor.object = None;
                                        cursor.member_after.clear();
                                        cursor.object_after = object;
                                        return Ok(false);
                                    }
                                }
                                cursor.object = Some(object_id);
                                cursor.member_after = member.name;
                                return Ok(false);
                            }
                            let retired = self.transaction(|transaction| {
                                let mut current = object_row(transaction, &object_id)?;
                                if current.state == ObjectState::Preparing {
                                    current.state = ObjectState::Retired;
                                    save_object(transaction, &mut current)?;
                                    return Ok(true);
                                }
                                Ok(false)
                            })?;
                            progress.objects_retired += u32::from(retired);
                        }
                        cursor.object = None;
                        cursor.member_after.clear();
                        cursor.object_after = object;
                        return Ok(false);
                    }
                    let mut memory = self.memory.lock()?;
                    self.transaction(|transaction| {
                        transaction.execute("DELETE FROM reservations WHERE id=?1", [&id])?;
                        Ok(())
                    })?;
                    memory.finish_reservation(&reservation.id);
                    cursor.reservation = None;
                    cursor.object_after.clear();
                    cursor.object = None;
                    cursor.member_after.clear();
                    progress.reservations_released += 1;
                    Ok(true)
                })(),
                2 => (|| {
                    let mut view: super::ViewRecord = serde_json::from_str(&encoded)?;
                    if view.instance == *self.instance() {
                        return Ok(true);
                    }
                    let retired = self.transaction(|transaction| {
                        let leases: bool = transaction.query_row(
                            "SELECT EXISTS(SELECT 1 FROM records WHERE kind='lease' AND json_extract(record,'$.view')=?1 AND json_extract(record,'$.released')=0)",
                            [&id], |row| row.get(0),
                        )?;
                        if !leases && view.active {
                            view.active = false;
                            transaction.execute("UPDATE records SET record=?2 WHERE kind='view' AND id=?1", params![id,text(&view)?])?;
                            transaction.execute("DELETE FROM refs WHERE source_kind='view' AND source_id=?1", [&id])?;
                            return Ok(true);
                        }
                        Ok(false)
                    })?;
                    progress.views_retired += u32::from(retired);
                    Ok(true)
                })(),
                3 => (|| {
                    if operation.is_some_and(|operation| operation.as_str() == id) {
                        return Ok(true);
                    }
                    let mut operation: OperationRecord = serde_json::from_str(&encoded)?;
                    if matches!(
                        operation.state,
                        OperationState::Completed
                            | OperationState::Failed
                            | OperationState::Cancelled
                    ) {
                        return Ok(true);
                    }
                    let old = operation.instance != *self.instance();
                    let now = self.clock.now()?;
                    let expired = now.boot == operation.accepted_at.boot
                        && now
                            .millis
                            .checked_sub(operation.accepted_at.millis)
                            .is_some_and(|elapsed| elapsed >= policy.work.operation_timeout_ms);
                    let reserved: bool = self.read(|connection| {
                        Ok(connection.query_row(
                            "SELECT EXISTS(SELECT 1 FROM reservations WHERE operation_id=?1)",
                            [&id],
                            |row| row.get(0),
                        )?)
                    })?;
                    if !reserved && (old || operation.cancelled || expired) {
                        operation.state = if operation.committed_state == CommitState::Committed {
                            OperationState::Completed
                        } else {
                            OperationState::Cancelled
                        };
                        operation.cancelled = operation.committed_state != CommitState::Committed;
                        if operation.result.is_none() {
                            if operation.kind == "collection"
                                && operation.committed_state == CommitState::Committed
                            {
                                let progress: super::CollectionProgress =
                                    serde_json::from_value(operation.progress.clone())?;
                                operation.result = Some(serde_json::to_value(progress)?);
                            } else {
                                let error = if expired {
                                    Error::new(ErrorCategory::Deadline, "operation-deadline", "unstarted work exhausted its accepted lifetime")
                                } else { Error::new(ErrorCategory::RecoveryRequired, "interrupted-operation",
                                    "the operation ended before a complete receipt; recover the authoritative view or resume with a new token") }
                                    .committed(operation.committed_state).operation(operation.id.to_string());
                                operation.error = Some(serde_json::to_value(error)?);
                                if operation.committed_state != CommitState::NotCommitted {
                                    operation.state = OperationState::Failed;
                                }
                            }
                        }
                        self.transaction(|transaction| {
                            transaction.execute("DELETE FROM refs WHERE source_kind IN ('migration','builder','predecessor','attachment') AND source_id=?1", [&id])?;
                            Self::save_operation(transaction, &operation)
                        })?;
                        progress.operations_recovered += 1;
                    }
                    Ok(true)
                })(),
                _ => unreachable!(),
            };
            match result {
                Ok(true) => cursor.after = id,
                Ok(false) => {}
                Err(error) => {
                    progress.mutation_committed |= error.committed_state == CommitState::Committed;
                    if error.committed_state != CommitState::NotCommitted {
                        progress.measurements_complete = false;
                    }
                    progress.issues.push(MaintenanceIssue {
                        kind: table.into(),
                        identity: id.clone(),
                        error: serde_json::to_value(error)?,
                    });
                    cursor.after = id;
                    cursor.reservation = None;
                    cursor.object_after.clear();
                    cursor.object = None;
                    cursor.member_after.clear();
                }
            }
        }
        progress.next = (cursor.phase < 14).then_some(cursor);
        progress.elapsed_nanos = u64::try_from(started.elapsed().as_nanos())
            .map_err(|_| Error::corrupt("recovery duration overflow"))?;
        Ok(progress)
    }
}
