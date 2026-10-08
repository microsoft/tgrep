// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::{next_revision, sql_integer, text, unsigned};
use super::lifetime::ActivityGuard;
use super::{CommitState, Error, ErrorCategory, Id, Namespace, OperationState, Result};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    pub version: u64,
    pub coordinator: Option<Id>,
    pub storage_bytes: Option<u64>,
    pub staging_bytes: u64,
    pub private_work_bytes: u64,
    pub work_slots: u32,
}

impl Allocation {
    pub(crate) fn local(policy: &super::Policy) -> Self {
        Self {
            version: 1,
            coordinator: None,
            storage_bytes: None,
            staging_bytes: policy.work.staging_bytes,
            private_work_bytes: policy.work.private_work_bytes,
            work_slots: policy.work.workers,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        for value in [self.staging_bytes, self.private_work_bytes]
            .into_iter()
            .chain(self.storage_bytes)
        {
            if value == 0 || value > i64::MAX as u64 {
                return Err(Error::invalid(
                    "allocation byte values must be in 1..=i64::MAX",
                ));
            }
        }
        if !(1..=64).contains(&self.work_slots) {
            return Err(Error::invalid("allocation work_slots must be in 1..=64"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkRequest {
    pub allocation_version: u64,
    pub staging_bytes: u64,
    pub private_bytes: u64,
    pub slots: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReservationRecord {
    pub id: Id,
    pub instance: Id,
    pub operation: Id,
    pub owner: Option<Id>,
    pub request: WorkRequest,
    pub producer_ended: bool,
    pub error: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkUsage {
    pub reservations: u64,
    pub reserved_staging_bytes: u64,
    pub unconsumed_staging_bytes: u64,
    pub reserved_private_bytes: u64,
    pub reserved_slots: u64,
    pub object_logical_bytes: u64,
    pub catalog_file_bytes: u64,
    pub control_logical_bytes: u64,
    pub memory: super::MemoryUsage,
    pub storage: super::StorageUsage,
}

pub(crate) fn allocation_row(connection: &rusqlite::Connection) -> Result<Allocation> {
    let record: String = connection.query_row(
        "SELECT record FROM records WHERE kind='allocation' AND id='namespace'",
        [],
        |row| row.get(0),
    )?;
    let allocation: Allocation = serde_json::from_str(&record)?;
    allocation.validate()?;
    Ok(allocation)
}

pub(crate) fn ensure_admission(connection: &rusqlite::Connection) -> Result<()> {
    let state: String =
        connection.query_row("SELECT admission FROM state WHERE singleton=1", [], |row| {
            row.get(0)
        })?;
    if state != "open" {
        return Err(Error::busy(&format!("namespace-admission-{state}")));
    }
    Ok(())
}

fn reserved_usage(
    connection: &rusqlite::Connection,
    metadata: u64,
    allocated: super::Measurement<u64>,
    memory: &mut super::memory::MemoryState,
) -> Result<WorkUsage> {
    let memory = memory.usage(connection)?;
    let counters = super::accounting::counters(connection)?;
    let storage = super::accounting::ledger(connection, &counters, metadata, allocated)?;
    Ok(WorkUsage {
        reservations: counters.get("reservations")?,
        reserved_staging_bytes: counters.get("reserved_staging")?,
        reserved_private_bytes: counters.get("reserved_private")?,
        reserved_slots: counters.get("reserved_slots")?,
        object_logical_bytes: counters.get("object_logical")?,
        catalog_file_bytes: metadata,
        unconsumed_staging_bytes: counters.get("unconsumed_staging")?,
        control_logical_bytes: counters.get("control_logical")?,
        memory,
        storage,
    })
}

impl Namespace {
    pub(crate) fn check_operation_lifetime(
        &self,
        operation: &super::OperationRecord,
    ) -> Result<()> {
        if operation.instance != *self.instance() {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "operation-instance-ended",
                "inspect or recover the prior instance's receipt",
            )
            .committed(operation.committed_state));
        }
        let now = self.clock.now()?;
        if now.boot != operation.accepted_at.boot || now.millis < operation.accepted_at.millis {
            return Err(Error::new(
                ErrorCategory::RecoveryRequired,
                "operation-clock-unknown",
                "operation lifetime cannot be proved",
            ));
        }
        if now.millis - operation.accepted_at.millis
            >= self.policy()?.policy.work.operation_timeout_ms
        {
            return Err(Error::new(
                ErrorCategory::Deadline,
                "operation-deadline",
                "operation exhausted its accepted lifetime",
            ));
        }
        Ok(())
    }

    pub fn allocation(&self) -> Result<Allocation> {
        self.read(allocation_row)
    }

    pub fn update_allocation(
        &self,
        expected: u64,
        mut allocation: Allocation,
    ) -> Result<Allocation> {
        allocation.validate()?;
        self.transaction(|transaction| {
            ensure_admission(transaction)?;
            let previous = allocation_row(transaction)?;
            if previous.version != expected {
                return Err(Error::stale_version(previous.version));
            }
            allocation.version = expected.checked_add(1)
                .filter(|value| *value <= i64::MAX as u64)
                .ok_or_else(|| Error::corrupt("allocation version exhausted"))?;
            transaction.execute(
                "UPDATE records SET version=?1,record=?2 WHERE kind='allocation' AND id='namespace'",
                params![sql_integer(allocation.version)?, text(&allocation)?],
            )?;
            next_revision(transaction)?;
            Ok(allocation)
        })
    }

    /// Catalog files are namespace-local; other repositories never consume this allocation.
    pub fn work_usage(&self) -> Result<WorkUsage> {
        let (metadata, allocated) = self.catalog_file_usage()?;
        let mut memory = self.memory.lock()?;
        self.read(|connection| reserved_usage(connection, metadata, allocated, &mut memory))
    }

    pub(crate) fn catalog_file_bytes(&self) -> Result<u64> {
        Ok(self.catalog_file_usage()?.0)
    }

    fn catalog_file_usage(&self) -> Result<(u64, super::Measurement<u64>)> {
        let mut total = 0_u64;
        let mut allocated = 0_u64;
        let mut unavailable = None;
        for name in [
            "namespace.json",
            "owner.lock",
            "activity.lock",
            "catalog.sqlite",
            "catalog.sqlite-wal",
            "catalog.sqlite-shm",
            "catalog.sqlite-journal",
        ] {
            match self.directory.observe_file(name) {
                Ok(file) => {
                    total = total
                        .checked_add(file.logical_bytes)
                        .ok_or_else(|| Error::corrupt("catalog size overflow"))?;
                    match file.allocated_bytes {
                        super::Measurement::Observed { value } => {
                            allocated = allocated
                                .checked_add(value)
                                .ok_or_else(|| Error::corrupt("catalog allocation overflow"))?;
                        }
                        super::Measurement::Unavailable { reason } => unavailable = Some(reason),
                    }
                }
                Err(error)
                    if name.starts_with("catalog.sqlite-")
                        && error.source_io_kind() == Some(std::io::ErrorKind::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        Ok((
            total,
            match unavailable {
                Some(reason) => super::Measurement::Unavailable { reason },
                None => super::Measurement::Observed { value: allocated },
            },
        ))
    }

    pub fn reserve(
        self: &Arc<Self>,
        operation: &Id,
        owner: Option<&Id>,
        request: WorkRequest,
    ) -> Result<Arc<WorkPermit>> {
        self.reserve_inner(operation, owner, request, None)
    }

    pub(crate) fn reserve_view(
        self: &Arc<Self>,
        operation: &Id,
        owner: Option<&Id>,
        allocation_version: u64,
    ) -> Result<Arc<WorkPermit>> {
        let policy = self.policy()?.policy;
        let allocation = self.allocation()?;
        self.reserve_inner(
            operation,
            owner,
            WorkRequest {
                allocation_version,
                slots: 1,
                staging_bytes: (policy.work.staging_bytes / u64::from(policy.work.workers))
                    .min(allocation.staging_bytes),
                private_bytes: (policy.work.private_work_bytes / u64::from(policy.work.workers))
                    .min(allocation.private_work_bytes),
            },
            Some(policy.work.minimum_private_bytes()?),
        )
    }

    fn reserve_inner(
        self: &Arc<Self>,
        operation: &Id,
        owner: Option<&Id>,
        request: WorkRequest,
        private_minimum: Option<u64>,
    ) -> Result<Arc<WorkPermit>> {
        let activity = ActivityGuard::acquire(&self.directory)?;
        let policy = self.policy()?.policy;
        let operation_record = self.operation(operation)?;
        if operation_record.instance != *self.instance() {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "operation-instance-ended",
                "old-instance operations require inspection or recovery, not execution",
            )
            .committed(operation_record.committed_state));
        }
        let now = self.clock.now()?;
        if now.boot != operation_record.accepted_at.boot
            || now.millis < operation_record.accepted_at.millis
        {
            return Err(Error::new(
                ErrorCategory::RecoveryRequired,
                "operation-clock-unknown",
                "operation lifetime needs restart recovery",
            ));
        }
        let elapsed = now.millis - operation_record.accepted_at.millis;
        let remaining_ms = policy
            .work
            .operation_timeout_ms
            .checked_sub(elapsed)
            .filter(|remaining| *remaining != 0)
            .ok_or_else(|| {
                Error::new(
                    ErrorCategory::Deadline,
                    "operation-deadline",
                    "queue and execution time budget exhausted",
                )
            })?;
        let reclamation = operation_record.kind == "collection";
        if (!reclamation && request.staging_bytes == 0)
            || request.private_bytes == 0
            || request.slots == 0
        {
            return Err(Error::invalid(
                "reservations need positive byte and slot limits",
            ));
        }
        let (metadata, allocated) = self.catalog_file_usage()?;
        if operation_record.cancelled
            || (!reclamation && operation_record.committed_state != CommitState::NotCommitted)
            || !matches!(
                operation_record.state,
                OperationState::Accepted | OperationState::Preparing
            )
        {
            return Err(Error::busy("operation-is-not-admissible"));
        }
        if let Some(owner) = owner {
            self.validate_active_owner(owner)?;
        }
        let mut record = ReservationRecord {
            id: Id::new()?,
            instance: self.instance().clone(),
            operation: operation.clone(),
            owner: owner.cloned(),
            request,
            producer_ended: false,
            error: None,
        };
        let mut memory = self.memory.lock()?;
        self.transaction(|transaction| {
            ensure_admission(transaction)?;
            let allocation = allocation_row(transaction)?;
            if allocation.version != record.request.allocation_version {
                return Err(Error::stale_version(allocation.version));
            }
            let already: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM reservations WHERE operation_id=?1)",
                [operation.as_str()],
                |row| row.get(0),
            )?;
            if already {
                return Err(Error::busy("operation-already-reserved"));
            }
            let usage = reserved_usage(transaction, metadata, allocated, &mut memory)?;
            let private = usage
                .reserved_private_bytes
                .checked_add(usage.memory.unreserved_retained_private_estimate_bytes)
                .ok_or_else(|| Error::pressure("private-account-overflow"))?;
            if let Some(minimum) = private_minimum {
                let remaining = allocation
                    .private_work_bytes
                    .min(policy.work.private_work_bytes)
                    .saturating_sub(private);
                record.request.private_bytes = record.request.private_bytes.min(remaining);
                if record.request.private_bytes < minimum {
                    return Err(Error::pressure("private-work-allocation"));
                }
            }
            for (used, wanted, limit, reason) in [
                (
                    usage.reserved_staging_bytes,
                    record.request.staging_bytes,
                    allocation.staging_bytes.min(policy.work.staging_bytes),
                    "staging-allocation",
                ),
                (
                    private,
                    record.request.private_bytes,
                    allocation
                        .private_work_bytes
                        .min(policy.work.private_work_bytes),
                    "private-work-allocation",
                ),
                (
                    usage.reserved_slots,
                    u64::from(record.request.slots),
                    u64::from(allocation.work_slots.min(policy.work.workers)),
                    "work-slot-allocation",
                ),
            ] {
                if used.checked_add(wanted).is_none_or(|total| total > limit) {
                    return Err(Error::pressure(reason));
                }
            }
            if let Some(limit) = allocation.storage_bytes.filter(|_| !reclamation) {
                let wanted = usage
                    .storage
                    .charged_overlap_logical_bytes
                    .checked_add(record.request.staging_bytes);
                if wanted.is_none_or(|value| value > limit) {
                    return Err(Error::pressure("namespace-storage-allocation"));
                }
            }
            transaction.execute(
                "INSERT INTO reservations VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    record.id.as_str(),
                    operation.as_str(),
                    owner.map(Id::as_str),
                    sql_integer(record.request.staging_bytes)?,
                    record.request.slots,
                    text(&record)?
                ],
            )?;
            Ok(())
        })?;
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(remaining_ms))
            .ok_or_else(|| Error::invalid("operation deadline overflow"))?;
        Ok(Arc::new(WorkPermit {
            namespace: Arc::clone(self),
            record,
            bytes_written: AtomicU64::new(0),
            private_bytes: AtomicU64::new(0),
            retained_private_bytes: AtomicU64::new(0),
            peak_private_bytes: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            inventory_failed: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            deadline,
            last_check: Mutex::new(None),
            _activity: activity,
        }))
    }

    pub fn validate_active_owner(&self, id: &Id) -> Result<()> {
        let record = self.inspect_owner(id)?;
        if record.claim.instance != *self.instance() || record.released || !record.registered {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "inactive-owner",
                "owner is not active in this daemon",
            ));
        }
        match record.last_proof {
            super::OwnerProof::Held => Ok(()),
            super::OwnerProof::Ended => Err(Error::new(
                ErrorCategory::StaleIdentity,
                "owner-ended",
                "owner lifetime has ended",
            )),
            super::OwnerProof::Unknown => Err(Error::busy("owner-proof-unknown")),
        }
    }

    pub fn acknowledge_operations(&self, scope: &Id, through: u64) -> Result<u64> {
        let readers = self
            .operation_readers
            .lock()
            .map_err(|_| Error::corrupt("operation reader registry poisoned"))?;
        self.transaction(|transaction| {
            let floor: u64 = transaction.query_row("SELECT floor FROM scopes WHERE id=?1",
                [scope.as_str()], |row| unsigned(row, 0))?;
            if through <= floor { return Ok(floor); }
            for id in readers.keys() {
                let protected: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND scope=?2 AND sequence<=?3)",
                    params![id.as_str(),scope.as_str(),sql_integer(through)?], |row| row.get(0),
                )?;
                if protected { return Err(Error::busy("operation-result-still-in-use")); }
            }
            let pending: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE scope=?1 AND sequence<=?2 AND state NOT IN ('completed','cancelled','failed'))",
                params![scope.as_str(), sql_integer(through)?], |row| row.get(0),
            )?;
            if pending { return Err(Error::busy("uncompleted-operation-before-watermark")); }
            let referenced: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM operations p JOIN objects o
                 ON json_extract(o.record,'$.operation')=p.id AND o.state='preparing'
                 WHERE p.scope=?1 AND p.sequence<=?2)
                 OR EXISTS(SELECT 1 FROM operations p JOIN reservations r ON r.operation_id=p.id
                 WHERE p.scope=?1 AND p.sequence<=?2)",
                params![scope.as_str(),sql_integer(through)?], |row| row.get(0),
            )?;
            if referenced { return Err(Error::busy("operation-still-protects-preparation")); }
            transaction.execute("DELETE FROM operations WHERE scope=?1 AND sequence<=?2",
                params![scope.as_str(), sql_integer(through)?])?;
            transaction.execute("UPDATE scopes SET floor=?2 WHERE id=?1",
                params![scope.as_str(), sql_integer(through)?])?;
            Ok(through)
        })
    }

    pub(crate) fn record_operation_result(
        &self,
        id: &Id,
        result: std::result::Result<serde_json::Value, Error>,
        committed: bool,
    ) -> Result<()> {
        self.transaction(|transaction| {
            let record: String = transaction.query_row(
                "SELECT record FROM operations WHERE id=?1",
                [id.as_str()],
                |row| row.get(0),
            )?;
            let mut operation: super::OperationRecord = serde_json::from_str(&record)?;
            match result {
                Ok(result) => {
                    operation.state = OperationState::Completed;
                    operation.result = Some(result);
                    if committed {
                        operation.committed_state = CommitState::Committed;
                    }
                }
                Err(mut error) => {
                    operation.state = if operation.state == OperationState::Completed {
                        OperationState::Completed
                    } else if error.category == ErrorCategory::Cancelled {
                        OperationState::Cancelled
                    } else {
                        OperationState::Failed
                    };
                    // A builder's publication is not the parent view's commit.
                    // This fresh transaction reads the authoritative operation receipt.
                    error.committed_state = operation.committed_state;
                    error.operation_id = Some(id.to_string());
                    operation.error = Some(serde_json::to_value(error)?);
                }
            }
            Self::save_operation(transaction, &operation)
        })
    }

    pub fn fail_operation(&self, id: &Id, mut error: Error) -> Result<super::OperationRecord> {
        self.transaction(|transaction| {
            let record: String = transaction.query_row(
                "SELECT record FROM operations WHERE id=?1",
                [id.as_str()],
                |row| row.get(0),
            )?;
            let mut operation: super::OperationRecord = serde_json::from_str(&record)?;
            if matches!(
                operation.state,
                OperationState::Failed | OperationState::Cancelled
            ) {
                return Ok(operation);
            }
            error.committed_state = operation.committed_state;
            error.operation_id = Some(id.to_string());
            operation.state = if operation.state == OperationState::Completed {
                OperationState::Completed
            } else if error.category == ErrorCategory::Cancelled {
                OperationState::Cancelled
            } else {
                OperationState::Failed
            };
            operation.committed_state = error.committed_state;
            operation.error = Some(serde_json::to_value(error)?);
            Self::save_operation(transaction, &operation)?;
            Ok(operation)
        })
    }

    pub(crate) fn operation_error(&self, id: &Id, mut error: Error) -> Error {
        match self.operation(id) {
            Ok(record) => error.committed_state = record.committed_state,
            Err(receipt) => {
                error.committed_state = CommitState::Unknown;
                error.detail = format!(
                    "{}; authoritative receipt unavailable: {receipt}",
                    error.detail
                );
            }
        }
        error.operation_id = Some(id.to_string());
        error
    }

    pub(crate) fn defer_operation(&self, id: &Id, reason: &str) -> Result<()> {
        let now = self.clock.now()?;
        let timeout = self.policy()?.policy.work.operation_timeout_ms;
        self.transaction(|transaction| {
            let record: String = transaction.query_row(
                "SELECT record FROM operations WHERE id=?1",
                [id.as_str()],
                |row| row.get(0),
            )?;
            let mut operation: super::OperationRecord = serde_json::from_str(&record)?;
            if operation.cancelled {
                return Err(Error::new(
                    ErrorCategory::Cancelled,
                    "operation-cancelled",
                    "deferred work was cancelled",
                ));
            }
            if operation.committed_state != CommitState::NotCommitted {
                return Err(Error::busy("operation-already-committed"));
            }
            let executing: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM reservations WHERE operation_id=?1)",
                [id.as_str()],
                |row| row.get(0),
            )?;
            if executing {
                return Ok(());
            }
            if now.boot != operation.accepted_at.boot
                || now.millis < operation.accepted_at.millis
                || now.millis - operation.accepted_at.millis >= timeout
            {
                return Err(Error::new(
                    ErrorCategory::Deadline,
                    "operation-deadline",
                    "deferred operation lifetime exhausted",
                ));
            }
            operation.state = OperationState::Accepted;
            operation.progress["waiting_reason"] = serde_json::json!(reason);
            Self::save_operation(transaction, &operation)
        })
    }
}

/// A reservation stays durable until every producer has stopped using it.
pub struct WorkPermit {
    pub(crate) namespace: Arc<Namespace>,
    pub(crate) record: ReservationRecord,
    bytes_written: AtomicU64,
    private_bytes: AtomicU64,
    retained_private_bytes: AtomicU64,
    peak_private_bytes: AtomicU64,
    cancelled: AtomicBool,
    inventory_failed: AtomicBool,
    finished: AtomicBool,
    deadline: Instant,
    last_check: Mutex<Option<Instant>>,
    _activity: ActivityGuard,
}

impl WorkPermit {
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn operation_id(&self) -> &Id {
        &self.record.operation
    }
    pub fn staging_limit(&self) -> u64 {
        self.record.request.staging_bytes
    }
    pub fn private_limit(&self) -> u64 {
        self.record.request.private_bytes
    }
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }
    pub fn peak_private_bytes(&self) -> u64 {
        self.peak_private_bytes.load(Ordering::Relaxed)
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn retain_charge(&self, bytes: u64) {
        self.retained_private_bytes
            .fetch_add(bytes, Ordering::AcqRel);
    }

    pub(crate) fn release_retained_charge(&self, bytes: u64) {
        self.retained_private_bytes
            .fetch_sub(bytes, Ordering::AcqRel);
        self.private_bytes.fetch_sub(bytes, Ordering::AcqRel);
    }

    pub fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(Error::new(
                ErrorCategory::Cancelled,
                "operation-cancelled",
                "work was cancelled",
            ));
        }
        if self.finished.load(Ordering::Acquire) {
            return Err(Error::busy("reservation-released"));
        }
        if Instant::now() >= self.deadline {
            return Err(Error::new(
                ErrorCategory::Deadline,
                "operation-deadline",
                "operation time budget exhausted",
            ));
        }
        let mut last = self
            .last_check
            .lock()
            .map_err(|_| Error::corrupt("work control lock poisoned"))?;
        if last.is_none_or(|time| time.elapsed() >= Duration::from_millis(10)) {
            if self.namespace.operation(&self.record.operation)?.cancelled {
                self.cancelled.store(true, Ordering::Release);
                return Err(Error::new(
                    ErrorCategory::Cancelled,
                    "operation-cancelled",
                    "work was cancelled",
                ));
            }
            *last = Some(Instant::now());
        }
        Ok(())
    }

    pub(crate) fn charge_write(&self, amount: u64) -> Result<()> {
        self.check()?;
        self.bytes_written
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(amount)
                    .filter(|total| *total <= self.staging_limit())
            })
            .map_err(|_| Error::pressure("reserved-staging-exhausted"))?;
        Ok(())
    }

    pub(crate) fn write(&self, file: &mut std::fs::File, bytes: &[u8]) -> std::io::Result<usize> {
        self.charge_write(bytes.len() as u64)
            .map_err(std::io::Error::other)?;
        let result = file.write(bytes);
        let written = result.as_ref().copied().unwrap_or(0);
        self.bytes_written
            .fetch_sub((bytes.len() - written) as u64, Ordering::AcqRel);
        result
    }

    pub(crate) fn memory(self: &Arc<Self>, bytes: u64) -> Result<MemoryCharge> {
        self.check()?;
        let before = self
            .private_bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= self.private_limit())
            })
            .map_err(|_| Error::pressure("reserved-private-memory-exhausted"))?;
        self.peak_private_bytes
            .fetch_max(before + bytes, Ordering::Relaxed);
        Ok(MemoryCharge {
            permit: Arc::clone(self),
            bytes,
        })
    }

    pub(crate) fn finish(&self) -> Result<()> {
        let mut memory = self.namespace.memory.lock()?;
        if self.inventory_failed.load(Ordering::Acquire) {
            return Err(Error::new(
                ErrorCategory::RecoveryRequired,
                "write-inventory-incomplete",
                "reservation is retained until failed producer inventory is recovered",
            ));
        }
        if self.private_bytes.load(Ordering::Acquire)
            != self.retained_private_bytes.load(Ordering::Acquire)
        {
            return Err(Error::busy("reserved-producers-still-running"));
        }
        let result = self.namespace.transaction(|transaction| {
            let record: Option<String> = transaction
                .query_row(
                    "SELECT record FROM reservations WHERE id=?1",
                    [self.record.id.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(record) = record {
                let record: ReservationRecord = serde_json::from_str(&record)?;
                if record.instance != *self.namespace.instance() {
                    return Err(Error::new(
                        ErrorCategory::StaleIdentity,
                        "reservation-instance",
                        "reservation belongs to another instance",
                    ));
                }
                transaction.execute(
                    "DELETE FROM reservations WHERE id=?1",
                    [self.record.id.as_str()],
                )?;
            }
            Ok(())
        });
        memory.finish_reservation(&self.record.id);
        result?;
        self.finished.store(true, Ordering::Release);
        Ok(())
    }
}

impl Drop for WorkPermit {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Acquire)
            && let Err(error) = self.finish()
        {
            eprintln!(
                "managed reservation {} remains for recovery: {error}",
                self.record.id
            );
            // Every producer owns this permit. Its destructor is an
            // authoritative end-of-work transition, not a lease timeout.
            if let Err(recording) = self.namespace.transaction(|transaction| {
                let encoded: Option<String> = transaction
                    .query_row(
                        "SELECT record FROM reservations WHERE id=?1",
                        [self.record.id.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(encoded) = encoded {
                    let mut record: ReservationRecord = serde_json::from_str(&encoded)?;
                    if record.instance != self.record.instance
                        || record.operation != self.record.operation
                    {
                        return Err(Error::corrupt("ended producer reservation differs"));
                    }
                    record.producer_ended = true;
                    record.error = Some(serde_json::to_value(&error)?);
                    transaction.execute(
                        "UPDATE reservations SET record=?2 WHERE id=?1",
                        params![record.id.as_str(), text(&record)?],
                    )?;
                }
                Ok(())
            }) {
                eprintln!(
                    "managed reservation {} needs instance recovery: {recording}",
                    self.record.id
                );
            }
        }
    }
}

pub(crate) struct MemoryCharge {
    permit: Arc<WorkPermit>,
    bytes: u64,
}

impl MemoryCharge {
    pub(crate) fn resize(&mut self, bytes: u64) -> Result<()> {
        if bytes > self.bytes {
            self.grow(bytes - self.bytes)?;
        } else {
            self.permit
                .private_bytes
                .fetch_sub(self.bytes - bytes, Ordering::AcqRel);
            self.bytes = bytes;
        }
        Ok(())
    }

    pub(crate) fn retain(mut self, mapped: u64) -> Result<super::memory::RetainedMemory> {
        let retained =
            self.permit
                .namespace
                .memory
                .retain_reserved(self.bytes, mapped, &self.permit)?;
        self.bytes = 0;
        Ok(retained)
    }

    pub(crate) fn grow(&mut self, bytes: u64) -> Result<()> {
        self.permit.check()?;
        let total = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| Error::pressure("private-memory-account-overflow"))?;
        let before = self
            .permit
            .private_bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= self.permit.private_limit())
            })
            .map_err(|_| Error::pressure("reserved-private-memory-exhausted"))?;
        self.permit
            .peak_private_bytes
            .fetch_max(before + bytes, Ordering::Relaxed);
        self.bytes = total;
        Ok(())
    }
}

impl Drop for MemoryCharge {
    fn drop(&mut self) {
        self.permit
            .private_bytes
            .fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

pub(crate) struct ChargedWriter {
    file: Option<std::fs::File>,
    permit: Arc<WorkPermit>,
    _pin: Arc<super::lifetime::ObjectGuard>,
    name: String,
    authentication: Mutex<Option<Box<super::authentication::ProducerSeal>>>,
    _authentication_memory: MemoryCharge,
}

impl ChargedWriter {
    pub(crate) fn new(
        pin: Arc<super::lifetime::ObjectGuard>,
        name: &str,
        permit: Arc<WorkPermit>,
    ) -> Result<Self> {
        permit.check()?;
        let authentication_memory = permit.memory(super::authentication::PRODUCER_MEMORY)?;
        let created = (|| {
            permit.namespace.begin_member(&pin.id, name)?;
            permit.namespace.fault(
                super::faults::Point::MemberCreationIntentSaved,
                Some(permit.operation_id()),
            )?;
            let file = pin.directory.create_file(name)?;
            permit.namespace.fault(
                super::faults::Point::MemberCreated,
                Some(permit.operation_id()),
            )?;
            permit
                .namespace
                .record_open_file_state(&pin.id, name, &file, true)?;
            Ok(file)
        })();
        let file = match created {
            Ok(file) => file,
            Err(error) => {
                permit.inventory_failed.store(true, Ordering::Release);
                return Err(error);
            }
        };
        Ok(Self {
            file: Some(file),
            permit,
            _pin: pin,
            name: name.into(),
            authentication: Mutex::new(Some(Box::new(super::authentication::ProducerSeal::new()))),
            _authentication_memory: authentication_memory,
        })
    }

    pub(crate) fn sync_all(&self) -> Result<()> {
        self.authentication
            .lock()
            .map_err(|_| Error::corrupt("producer authentication lock poisoned"))?
            .as_mut()
            .expect("writer owns its authentication until drop")
            .flush(&self.permit.namespace, &self._pin.id, &self.name)?;
        self.file
            .as_ref()
            .expect("writer owns its file until drop")
            .sync_all()?;
        self.permit.namespace.record_open_file_state(
            &self._pin.id,
            &self.name,
            self.file.as_ref().expect("writer owns its file until drop"),
            true,
        )?;
        Ok(())
    }
}

impl Write for ChargedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let authentication = self
            .authentication
            .get_mut()
            .map_err(|_| std::io::Error::other("producer authentication lock poisoned"))?
            .as_mut()
            .expect("writer owns its authentication until drop");
        if authentication.available() == 0 {
            authentication
                .flush(&self.permit.namespace, &self._pin.id, &self.name)
                .map_err(std::io::Error::other)?;
        }
        let bytes = &bytes[..bytes.len().min(authentication.available())];
        let written = self.permit.write(
            self.file.as_mut().expect("writer owns its file until drop"),
            bytes,
        )?;
        authentication.accepted(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.permit.check().map_err(std::io::Error::other)?;
        self.file
            .as_mut()
            .expect("writer owns its file until drop")
            .flush()
    }
}

impl Drop for ChargedWriter {
    fn drop(&mut self) {
        drop(self.file.take());
        let result = (|| {
            let seal = self
                .authentication
                .get_mut()
                .map_err(|_| Error::corrupt("producer authentication lock poisoned"))?
                .take()
                .ok_or_else(|| Error::corrupt("producer authentication was already consumed"))?
                .finish(&self.permit.namespace, &self._pin.id, &self.name)?;
            self.permit
                .namespace
                .complete_member_seal(&self._pin.id, &self.name, seal)?;
            self.permit.namespace.record_file(&self._pin.id, &self.name)
        })();
        if let Err(error) = result {
            self.permit.inventory_failed.store(true, Ordering::Release);
            eprintln!(
                "managed producer {} keeps its reservation after inventory failure: {error}",
                self._pin.id
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed::faults::{Action, Point, Specification};
    use crate::managed::{ObjectKind, ObjectState, Token};

    fn fixture() -> (tempfile::TempDir, Arc<Namespace>, Arc<WorkPermit>) {
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
                Token::parse("producer").unwrap(),
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
        (temp, namespace, permit)
    }

    fn recover(namespace: &Namespace) -> Vec<crate::managed::MaintenanceIssue> {
        let mut cursor = None;
        let mut issues = Vec::new();
        for _ in 0..64 {
            let result = namespace.recover_pass(cursor).unwrap();
            issues.extend(result.issues);
            cursor = result.next;
            if cursor.is_none() {
                return issues;
            }
        }
        panic!("producer recovery did not complete its bounded traversal");
    }

    fn member(namespace: &Namespace, object: &Id) -> crate::managed::FileRecord {
        namespace
            .read(|connection| {
                let encoded: String = connection.query_row(
                    "SELECT record FROM members WHERE object_id=?1 AND name='payload'",
                    [object.as_str()],
                    |row| row.get(0),
                )?;
                Ok(serde_json::from_str(&encoded)?)
            })
            .unwrap()
    }

    #[test]
    fn intended_proof_hashes_every_accepted_byte_across_bounded_batches() {
        let (_temp, namespace, permit) = fixture();
        let (object, pin) = namespace
            .create_object(ObjectKind::BuildStage, None, &permit)
            .unwrap();
        let mut writer =
            ChargedWriter::new(Arc::clone(&pin), "payload", Arc::clone(&permit)).unwrap();
        let bytes: Vec<u8> = (0..(crate::managed::authentication::BLOCK_BYTES * 67 + 3))
            .map(|index| (index % 251) as u8)
            .collect();
        for fragment in bytes.chunks(3079) {
            writer.write_all(fragment).unwrap();
        }
        writer.sync_all().unwrap();
        assert!(member(&namespace, &object.id).seal.is_none());
        drop(writer);
        let record = member(&namespace, &object.id);
        assert!(!record.producer_open);
        let seal = record.seal.unwrap();
        assert_eq!(seal.length, bytes.len() as u64);
        assert_eq!(seal.blocks, 68);
        let mut manifest = crate::managed::authentication::manifest_hasher();
        namespace.read(|connection| {
            for (index, chunk) in bytes.chunks(crate::managed::authentication::BLOCK_BYTES).enumerate() {
                let (length, digest): (u32, Vec<u8>) = connection.query_row(
                    "SELECT length,digest FROM member_seals WHERE object_id=?1 AND name='payload' AND block_index=?2",
                    rusqlite::params![object.id.as_str(), i64::try_from(index).unwrap()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                assert_eq!(length as usize, chunk.len());
                assert_eq!(digest.as_slice(), blake3::hash(chunk).as_bytes());
                crate::managed::authentication::hash_block(&mut manifest,
                    &crate::managed::authentication::Block {
                        index: index as u64, length,
                        digest: digest.try_into().unwrap(),
                    });
            }
            Ok(())
        }).unwrap();
        assert_eq!(seal.manifest, *manifest.finalize().as_bytes());
        assert!(
            permit.peak_private_bytes.load(Ordering::Acquire)
                >= crate::managed::authentication::PRODUCER_MEMORY
        );
    }

    #[test]
    fn intended_proof_does_not_adopt_changed_output_before_producer_close() {
        let (_temp, namespace, permit) = fixture();
        let (object, pin) = namespace
            .create_object(ObjectKind::BuildStage, None, &permit)
            .unwrap();
        let mut writer =
            ChargedWriter::new(Arc::clone(&pin), "payload", Arc::clone(&permit)).unwrap();
        let intended = b"original producer bytes";
        writer.write_all(intended).unwrap();
        writer.sync_all().unwrap();
        let changed = vec![b'x'; intended.len()];
        std::fs::write(pin.directory.path().join("payload"), &changed).unwrap();
        drop(writer);
        assert_eq!(
            member(&namespace, &object.id).seal,
            Some(crate::managed::MemberSeal::bounded(intended).unwrap())
        );
        assert_ne!(
            member(&namespace, &object.id).seal,
            Some(crate::managed::MemberSeal::bounded(&changed).unwrap())
        );
    }

    #[test]
    fn intended_proof_retries_exact_rows_after_postcommit_response_loss() {
        let (_temp, namespace, permit) = fixture();
        let (object, pin) = namespace
            .create_object(ObjectKind::BuildStage, None, &permit)
            .unwrap();
        let mut writer =
            ChargedWriter::new(Arc::clone(&pin), "payload", Arc::clone(&permit)).unwrap();
        let bytes = [b'x'; crate::managed::authentication::BLOCK_BYTES];
        writer.write_all(&bytes).unwrap();
        namespace
            .install_test_fault(Specification {
                point: Point::CatalogAfterCommit,
                operation: None,
                skip_hits: 0,
                action: Action::Error {
                    category: ErrorCategory::Io,
                },
            })
            .unwrap();
        let error = writer.sync_all().unwrap_err();
        assert_eq!(error.committed_state, super::super::CommitState::Committed);
        writer.sync_all().unwrap();
        drop(writer);
        assert_eq!(
            member(&namespace, &object.id).seal,
            Some(crate::managed::MemberSeal::bounded(&bytes).unwrap())
        );
        let blocks: u64 = namespace
            .read(|connection| {
                Ok(connection.query_row(
                    "SELECT count(*) FROM member_seals WHERE object_id=?1 AND name='payload'",
                    [object.id.as_str()],
                    |row| super::super::catalog::unsigned(row, 0),
                )?)
            })
            .unwrap();
        assert_eq!(blocks, 1);
    }

    #[test]
    fn intended_proof_completion_crashes_never_invent_missing_content_evidence() {
        for point in [Point::MemberSealBeforeCommit, Point::MemberSealAfterCommit] {
            let (_temp, namespace, permit) = fixture();
            let (object, pin) = namespace
                .create_object(ObjectKind::BuildStage, None, &permit)
                .unwrap();
            let mut writer =
                ChargedWriter::new(Arc::clone(&pin), "payload", Arc::clone(&permit)).unwrap();
            writer.write_all(b"intended output").unwrap();
            writer.sync_all().unwrap();
            namespace
                .install_test_fault(Specification {
                    point,
                    operation: Some(permit.operation_id().clone()),
                    skip_hits: 0,
                    action: Action::Error {
                        category: ErrorCategory::Io,
                    },
                })
                .unwrap();
            drop(writer);
            assert!(member(&namespace, &object.id).producer_open);
            drop((pin, permit));
            assert_eq!(namespace.work_usage().unwrap().reservations, 1);
            let issues = recover(&namespace);
            let expected = if point == Point::MemberSealBeforeCommit {
                assert_eq!(issues.len(), 1);
                assert_eq!(issues[0].error["reason_code"], "member-content-unsealed");
                assert!(member(&namespace, &object.id).seal.is_none());
                ObjectState::Quarantined
            } else {
                assert!(issues.is_empty());
                assert!(!member(&namespace, &object.id).producer_open);
                assert_eq!(
                    member(&namespace, &object.id).seal,
                    Some(crate::managed::MemberSeal::bounded(b"intended output").unwrap())
                );
                ObjectState::Retired
            };
            assert_eq!(namespace.object(&object.id).unwrap().state, expected);
            assert_eq!(namespace.work_usage().unwrap().reservations, 0);
        }
    }

    #[test]
    fn live_writer_blocks_publication_even_after_sync_until_its_handle_is_closed() {
        let (_temp, namespace, permit) = fixture();
        let (object, pin) = namespace
            .create_object(ObjectKind::BuildStage, None, &permit)
            .unwrap();
        let mut writer =
            ChargedWriter::new(Arc::clone(&pin), "payload", Arc::clone(&permit)).unwrap();
        writer.write_all(b"first output").unwrap();
        writer.sync_all().unwrap();
        let error = namespace
            .publish_object(&object.id, None, None)
            .unwrap_err();
        assert_eq!(error.reason_code, "object-producers-not-sealed");
        writer.write_all(b" after sync").unwrap();
        writer.sync_all().unwrap();
        drop(writer);
        namespace.publish_object(&object.id, None, None).unwrap();
        assert!(ChargedWriter::new(Arc::clone(&pin), "late-output", Arc::clone(&permit)).is_err());
        assert_eq!(
            std::fs::read(pin.directory.path().join("payload")).unwrap(),
            b"first output after sync"
        );
        assert!(!pin.directory.path().join("late-output").exists());
    }

    #[test]
    fn failed_member_creation_keeps_unknown_files_and_releases_only_ended_work() {
        for point in [Point::MemberCreationIntentSaved, Point::MemberCreated] {
            let (_temp, namespace, permit) = fixture();
            let (object, pin) = namespace
                .create_object(ObjectKind::BuildStage, None, &permit)
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
            assert!(
                ChargedWriter::new(Arc::clone(&pin), "uncertain", Arc::clone(&permit)).is_err()
            );
            let path = pin.directory.path().join("uncertain");
            if point == Point::MemberCreated {
                std::fs::write(&path, b"never adopt this unknown content").unwrap();
            }
            drop((pin, permit));
            assert_eq!(namespace.work_usage().unwrap().reservations, 1);
            assert!(recover(&namespace).is_empty());
            let usage = namespace.work_usage().unwrap();
            assert_eq!(usage.reservations, 0);
            assert_eq!(usage.reserved_slots, 0);
            let state = namespace.object(&object.id).unwrap().state;
            if point == Point::MemberCreated {
                assert_eq!(state, ObjectState::Quarantined);
                assert_eq!(
                    usage.storage.uncertain_staging_charge_bytes + usage.object_logical_bytes,
                    1024 * 1024
                );
                assert_eq!(
                    std::fs::read(path).unwrap(),
                    b"never adopt this unknown content"
                );
                assert!(!namespace.eligibility(&object.id).unwrap().eligible);
            } else {
                assert_eq!(state, ObjectState::Retired);
                assert_eq!(usage.storage.uncertain_staging_charge_bytes, 0);
                assert!(namespace.eligibility(&object.id).unwrap().eligible);
            }
        }
    }

    #[test]
    fn producer_recovery_quarantines_modified_closed_output_without_reinventoring_it() {
        let (_temp, namespace, permit) = fixture();
        let (object, pin) = namespace
            .create_object(ObjectKind::BuildStage, None, &permit)
            .unwrap();
        let mut writer =
            ChargedWriter::new(Arc::clone(&pin), "payload", Arc::clone(&permit)).unwrap();
        writer.write_all(b"original").unwrap();
        writer.sync_all().unwrap();
        drop(writer);
        let path = pin.directory.path().join("payload");
        std::fs::write(&path, b"external replacement contents").unwrap();
        permit.inventory_failed.store(true, Ordering::Release);
        drop((pin, permit));
        let issues = recover(&namespace);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].error["reason_code"], "recovery-member-modified");
        assert_eq!(
            namespace.object(&object.id).unwrap().state,
            ObjectState::Quarantined
        );
        assert_eq!(namespace.work_usage().unwrap().reservations, 0);
        assert_eq!(
            std::fs::read(path).unwrap(),
            b"external replacement contents"
        );
    }
}
