// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::{next_revision, sql_integer, text, unsigned};
use super::work::{allocation_row, ensure_admission};
use super::{
    Allocation, CommitState, Error, Id, Namespace, OperationRecord, OperationState, OperationToken,
    Policy, ReferenceKind, Result, Token, VersionedPolicy,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub struct OperationReadGuard {
    readers: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<Id, u32>>>,
    id: Id,
}

impl Drop for OperationReadGuard {
    fn drop(&mut self) {
        let mut readers = self.readers.lock().expect("operation reader registry");
        let count = readers
            .get_mut(&self.id)
            .expect("operation reader lifetime");
        *count -= 1;
        if *count == 0 {
            readers.remove(&self.id);
        }
    }
}

pub fn publish_control_file(path: &std::path::Path, value: &Value) -> Result<super::FileIdentity> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::invalid("control file needs a parent"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::invalid("control filename must be Unicode"))?;
    super::storage::Directory::open(parent)?.publish_json(name, value)
}

pub fn remove_control_file(path: &std::path::Path, identity: &super::FileIdentity) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::invalid("control file needs a parent"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::invalid("control filename must be Unicode"))?;
    super::storage::Directory::open(parent)?.remove_file(name, identity)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "method",
    content = "params",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub enum MetadataMutation {
    Allocation {
        expected_version: u64,
        allocation: Allocation,
    },
    Policy {
        expected_version: u64,
        policy: Policy,
    },
    Retain {
        object: Id,
    },
    Release {
        reference: Id,
    },
}

impl Namespace {
    pub fn hold_operation(&self, id: &Id) -> Result<OperationReadGuard> {
        let limit = self.policy()?.policy.work.max_receipts as usize;
        let mut readers = self
            .operation_readers
            .lock()
            .map_err(|_| Error::corrupt("operation reader registry poisoned"))?;
        if readers.len() >= limit && !readers.contains_key(id) {
            return Err(Error::pressure("operation-reader-limit"));
        }
        let count = readers.entry(id.clone()).or_default();
        *count = count
            .checked_add(1)
            .ok_or_else(|| Error::pressure("operation-reader-count"))?;
        drop(readers);
        let guard = OperationReadGuard {
            readers: std::sync::Arc::clone(&self.operation_readers),
            id: id.clone(),
        };
        self.operation(id)?;
        Ok(guard)
    }

    pub fn authoritative_view(&self, id: &Id) -> Result<super::ViewRecord> {
        self.read(|connection| super::views::view_row(connection, id))
    }

    /// A coordinator persists this namespace-scoped sequence before sending.
    /// Unlike an instance scope, its receipt remains addressable after restart.
    pub fn accept_maintenance_operation(
        &self,
        token: OperationToken,
        kind: &str,
        request: Value,
    ) -> Result<OperationRecord> {
        if token.scope != self.header().namespace {
            return Err(Error::invalid(
                "maintenance token scope must identify this namespace",
            ));
        }
        if !matches!(kind, "metadata" | "collection" | "idle-stop" | "recovery") {
            return Err(Error::invalid("unsupported maintenance operation kind"));
        }
        self.accept_operation(token, kind, request)
    }

    /// Internal jobs have a separate scope and allocator from every client's
    /// sequence space. The caller's retained token survives a missing response.
    pub fn accept_system_operation(
        &self,
        token: Token,
        kind: &str,
        request: Value,
    ) -> Result<OperationRecord> {
        let _serial = self
            .system_operations
            .lock()
            .map_err(|_| Error::corrupt("system operation allocator poisoned"))?;
        let sequence = self.read(|connection| {
            let previous: Option<u64> = connection.query_row(
                "SELECT sequence FROM operations WHERE scope=?1 AND token=?2",
                params![self.instance().as_str(),token.as_str()], |row| unsigned(row,0),
            ).optional()?;
            if let Some(sequence) = previous { return Ok(sequence); }
            let last: u64 = connection.query_row(
                "SELECT max(floor,coalesce((SELECT max(sequence) FROM operations WHERE scope=?1),0)) FROM scopes WHERE id=?1",
                [self.instance().as_str()], |row| unsigned(row,0),
            )?;
            last.checked_add(1).filter(|value| *value <= i64::MAX as u64)
                .ok_or_else(|| Error::corrupt("system operation sequence exhausted"))
        })?;
        self.accept_operation(
            OperationToken {
                scope: self.instance().clone(),
                sequence,
                token,
            },
            kind,
            request,
        )
    }

    pub fn accept_metadata_mutation(
        &self,
        token: OperationToken,
        mutation: MetadataMutation,
    ) -> Result<OperationRecord> {
        match &mutation {
            MetadataMutation::Allocation { allocation, .. } => allocation.validate()?,
            MetadataMutation::Policy { policy, .. } => {
                policy.validate()?;
                if policy.storage != self.header().storage {
                    return Err(Error::incompatible("namespace storage mode is immutable"));
                }
            }
            _ => {}
        }
        self.accept_operation(token, "metadata", serde_json::to_value(mutation)?)
    }

    pub fn operation_for_token(&self, token: &OperationToken) -> Result<Option<OperationRecord>> {
        token.validate()?;
        self.read(|connection| {
            let floor: Option<u64> = connection
                .query_row(
                    "SELECT floor FROM scopes WHERE id=?1",
                    [token.scope.as_str()],
                    |row| unsigned(row, 0),
                )
                .optional()?;
            if floor.is_none_or(|floor| token.sequence <= floor) {
                return Err(Error::new(
                    super::ErrorCategory::ReceiptExpired,
                    "operation-scope-expired",
                    "unknown or acknowledged sequence cannot be interpreted as new work",
                )
                .committed(CommitState::Unknown));
            }
            let encoded: Option<String> = connection
                .query_row(
                    "SELECT record FROM operations WHERE scope=?1 AND (sequence=?2 OR token=?3)",
                    params![
                        token.scope.as_str(),
                        sql_integer(token.sequence)?,
                        token.token.as_str()
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            encoded
                .map(|encoded| {
                    let operation: OperationRecord = serde_json::from_str(&encoded)?;
                    if operation.token != *token {
                        return Err(Error::invalid("operation token and sequence disagree"));
                    }
                    Ok(operation)
                })
                .transpose()
        })
    }

    pub fn validate_operation_owner(&self, token: &OperationToken) -> Result<()> {
        if self.operation_for_token(token)?.is_none() {
            self.validate_active_owner(&token.scope)?;
        }
        Ok(())
    }

    pub fn acknowledge_system_operations(&self, protected: &[Id]) -> Result<bool> {
        let keep = self.policy()?.policy.work.page_objects.max(1);
        let through = self.read(|connection| {
            let mut through = connection
                .query_row(
                    "SELECT coalesce(max(sequence),0) FROM operations WHERE scope=?1",
                    [self.instance().as_str()],
                    |row| unsigned(row, 0),
                )?
                .saturating_sub(u64::from(keep));
            for id in protected {
                let sequence: Option<u64> = connection
                    .query_row(
                        "SELECT sequence FROM operations WHERE id=?1 AND scope=?2",
                        params![id.as_str(), self.instance().as_str()],
                        |row| unsigned(row, 0),
                    )
                    .optional()?;
                if let Some(sequence) = sequence {
                    through = through.min(sequence.saturating_sub(1));
                }
            }
            Ok(through)
        })?;
        match self.acknowledge_operations(self.instance(), through) {
            Ok(_) => Ok(true),
            Err(error) if error.category == super::ErrorCategory::Busy => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The resource CAS and its original receipt are committed in one catalog
    /// transaction, including when the caller loses the successful response.
    pub fn execute_metadata_mutation(&self, id: &Id) -> Result<OperationRecord> {
        let _receipt = self.hold_operation(id)?;
        let operation = self.operation(id)?;
        if matches!(
            operation.state,
            OperationState::Completed | OperationState::Failed | OperationState::Cancelled
        ) {
            return Ok(operation);
        }
        if operation.kind != "metadata" || operation.instance != *self.instance() {
            return Err(Error::invalid(
                "metadata operation belongs to another kind or instance",
            ));
        }
        if let Err(error) = self.check_operation_lifetime(&operation) {
            return self.fail_operation(id, error);
        }
        let mutation: MetadataMutation = serde_json::from_value(operation.request.clone())?;
        let _pin = match &mutation {
            MetadataMutation::Retain { object } => match self.pin(object) {
                Ok(pin) => Some(pin),
                Err(error) => return self.fail_operation(id, error),
            },
            _ => None,
        };
        let result = self.transaction(|transaction| {
            ensure_admission(transaction)?;
            let encoded: String = transaction.query_row(
                "SELECT record FROM operations WHERE id=?1", [id.as_str()], |row| row.get(0),
            )?;
            let mut current: OperationRecord = serde_json::from_str(&encoded)?;
            if current.cancelled { return Err(Error::new(super::ErrorCategory::Cancelled, "operation-cancelled", "metadata mutation was cancelled")); }
            if current.state == OperationState::Completed { return Ok(()); }
            let result = match mutation {
                MetadataMutation::Allocation { expected_version, mut allocation } => {
                    allocation.validate()?;
                    let previous = allocation_row(transaction)?;
                    if previous.version != expected_version { return Err(Error::stale_version(previous.version)); }
                    allocation.version = expected_version.checked_add(1).filter(|value| *value <= i64::MAX as u64)
                        .ok_or_else(|| Error::corrupt("allocation version exhausted"))?;
                    transaction.execute(
                        "UPDATE records SET version=?1,record=?2 WHERE kind='allocation' AND id='namespace'",
                        params![sql_integer(allocation.version)?, text(&allocation)?],
                    )?;
                    next_revision(transaction)?;
                    serde_json::to_value(allocation)?
                }
                MetadataMutation::Policy { expected_version, policy } => {
                    policy.validate()?;
                    if policy.storage != self.header().storage { return Err(Error::incompatible("namespace storage mode is immutable")); }
                    let previous: u64 = transaction.query_row(
                        "SELECT policy_version FROM state WHERE singleton=1", [], |row| unsigned(row,0),
                    )?;
                    if previous != expected_version { return Err(Error::stale_version(previous)); }
                    let version = previous.checked_add(1).filter(|value| *value <= i64::MAX as u64)
                        .ok_or_else(|| Error::corrupt("policy version exhausted"))?;
                    transaction.execute("UPDATE state SET policy_version=?1,policy=?2 WHERE singleton=1",
                        params![sql_integer(version)?,text(&policy)?])?;
                    next_revision(transaction)?;
                    serde_json::to_value(VersionedPolicy { version, policy })?
                }
                MetadataMutation::Retain { object } => {
                    let reference = Id::new()?;
                    Self::add_reference(transaction, &reference, ReferenceKind::Persistent, reference.as_str(), &object, None)?;
                    json!({"reference":reference,"object":object})
                }
                MetadataMutation::Release { reference } => {
                    let released = transaction.execute(
                        "DELETE FROM refs WHERE id=?1 AND source_kind='persistent'", [reference.as_str()],
                    )? != 0;
                    json!({"reference":reference,"released":released})
                }
            };
            current.state = OperationState::Completed;
            current.committed_state = CommitState::Committed;
            current.result = Some(result);
            Self::save_operation(transaction, &current)
        });
        if let Err(error) = result {
            if error.committed_state == CommitState::NotCommitted {
                self.record_operation_result(id, Err(error.operation(id.to_string())), false)?;
            } else {
                return Err(error.operation(id.to_string()));
            }
        }
        self.operation(id)
    }
}
