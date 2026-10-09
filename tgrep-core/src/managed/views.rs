// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::{sql_integer, text, unsigned};
use super::{
    CheckpointBinding, CommitState, CurrentPin, Error, ErrorCategory, FileIdentity, Id, Namespace,
    NativePath, OperationRecord, OperationState, OperationToken, ReferenceKind, Result, RootAnchor,
    Token, WorkPermit,
};
use crate::generations::GenerationKey;
use crate::generations::{BuildStats, Generation, GenerationManager, IndexingProfile, Repository};
use crate::rooted::RootedDir;
use crate::worktrees::{
    OverlayCost, ReconcileStats, WorktreeOptions, WorktreeSnapshot, WorktreeView,
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

const RECONCILE_ATTEMPTS: u32 = 3;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewVersion {
    pub view: Id,
    pub version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachRequest {
    pub root: PathBuf,
    pub revision: Option<String>,
    pub profile: IndexingProfile,
    pub lease: Token,
    pub owner: Id,
    pub accept_current: Option<ViewVersion>,
    pub migratable: bool,
    pub allocation_version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyAttachRequest {
    pub root: PathBuf,
    pub revision: String,
    pub profile: IndexingProfile,
    pub lease: Token,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationRequest {
    pub view: Id,
    pub root: PathBuf,
    pub expected_version: u64,
    pub target_commit: String,
    pub profile: IndexingProfile,
    pub owner: Id,
    pub allocation_version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshRequest {
    pub view: Id,
    pub expected_version: u64,
    pub owner: Id,
    pub allocation_version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileRequest {
    pub view: Id,
    pub expected_version: u64,
    pub allocation_version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginalAttachment {
    pub root: NativePath,
    pub revision: Option<String>,
    pub profile: IndexingProfile,
    pub accept_current: Option<ViewVersion>,
    pub migratable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRecord {
    pub token: Token,
    pub owner: Option<Id>,
    pub instance: Id,
    pub view: Id,
    pub original: OriginalAttachment,
    pub exact_original_commit: String,
    pub original_version: u64,
    pub released: bool,
    pub operation: Id,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinIntent {
    pub commit: String,
    pub key: GenerationKey,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewRecord {
    pub id: Id,
    pub root: NativePath,
    pub root_identity: FileIdentity,
    pub root_anchor: Option<RootAnchor>,
    pub version: u64,
    pub revision: u64,
    pub current: Option<CurrentPin>,
    pub pending: Option<PinIntent>,
    pub checkpoint: Option<Id>,
    pub checkpoint_binding: Option<CheckpointBinding>,
    pub input_epoch: u64,
    pub reconciled_epoch: Option<u64>,
    pub active: bool,
    pub committed: bool,
    pub instance: Id,
}

impl ViewRecord {
    fn same_publication(&self, other: &Self) -> bool {
        self.id == other.id
            && self.version == other.version
            && self.root == other.root
            && self.root_identity == other.root_identity
            && self.root_anchor == other.root_anchor
            && self.current == other.current
            && self.pending == other.pending
            && self.checkpoint == other.checkpoint
            && self.checkpoint_binding == other.checkpoint_binding
            && self.input_epoch == other.input_epoch
            && self.reconciled_epoch == other.reconciled_epoch
            && self.committed == other.committed
    }

    pub fn pin(&self) -> Result<&CurrentPin> {
        self.current
            .as_ref()
            .ok_or_else(|| Error::busy("view-has-no-published-pin"))
    }

    pub fn intent(&self) -> Result<PinIntent> {
        if let Some(current) = &self.current {
            Ok(PinIntent {
                commit: current.commit.clone(),
                key: current.key.clone(),
            })
        } else {
            self.pending
                .clone()
                .ok_or_else(|| Error::corrupt("view has neither a pin nor a pending intent"))
        }
    }
}

pub struct PublishedView {
    pub record: ViewRecord,
    pub(crate) view: Arc<WorktreeView>,
}

struct SlotState {
    current: Option<Arc<PublishedView>>,
    preparing: Option<Arc<WorktreeView>>,
    original_root: Option<RootedDir>,
    work: Option<Id>,
    closing: bool,
    draining: bool,
    queries: u32,
    retired: Vec<Weak<PublishedView>>,
    observer: Option<Box<dyn Send>>,
    error: Option<Value>,
    last_stats: Option<ReconcileStats>,
}

pub struct ViewSlot {
    id: Id,
    root: PathBuf,
    namespace: Arc<Namespace>,
    epoch: AtomicU64,
    state: Mutex<SlotState>,
    serial: Mutex<()>,
}

pub type ObserveView = Arc<dyn Fn(&Arc<ViewSlot>) -> Result<Box<dyn Send>> + Send + Sync>;

pub struct ViewManager {
    namespace: Arc<Namespace>,
    legacy: Option<Arc<GenerationManager>>,
    options: WorktreeOptions,
    observer: Option<ObserveView>,
    slots: Mutex<BTreeMap<Id, Arc<ViewSlot>>>,
    admission: Mutex<()>,
}

#[derive(Debug, Serialize)]
pub struct ViewStatus {
    pub authoritative: ViewRecord,
    pub ready: bool,
    pub input_epoch: u64,
    pub queries: u32,
    pub old_snapshots: usize,
    pub leases: u64,
    pub closing: bool,
    pub work: Option<Id>,
    pub overlay: Option<OverlayCost>,
    pub last_reconcile: Option<ReconcileStats>,
    pub error: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct DetachResult {
    pub lease_released: bool,
    pub remaining_leases: u64,
    pub daemon_leases: u64,
    pub queries: u32,
    pub work: Option<Id>,
    pub root_handles_released: bool,
    pub root_proof_scope: &'static str,
}

fn root_key(root: &NativePath) -> Result<String> {
    Ok(blake3::hash(text(root)?.as_bytes()).to_hex().to_string())
}

fn lease_key(owner: Option<&Id>, token: &Token) -> String {
    format!("{}:{}", owner.map_or("legacy", Id::as_str), token.as_str())
}

fn lease_row(connection: &rusqlite::Connection, key: &str) -> Result<Option<LeaseRecord>> {
    let record: Option<String> = connection
        .query_row(
            "SELECT record FROM records WHERE kind='lease' AND id=?1",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    record
        .map(|record| Ok(serde_json::from_str(&record)?))
        .transpose()
}

pub(crate) fn view_row(connection: &rusqlite::Connection, id: &Id) -> Result<ViewRecord> {
    let record: Option<String> = connection
        .query_row(
            "SELECT record FROM records WHERE kind='view' AND id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let record = record.ok_or_else(|| {
        Error::new(
            ErrorCategory::CacheMissing,
            "view-not-found",
            "view identity is unavailable",
        )
    })?;
    let view: ViewRecord = serde_json::from_str(&record)?;
    if view.id != *id || view.version == 0 {
        return Err(Error::corrupt("view row identity or version differs"));
    }
    if view.committed != view.current.is_some() || view.current.is_some() == view.pending.is_some()
    {
        return Err(Error::corrupt(
            "view publication state disagrees with its pin or pending intent",
        ));
    }
    Ok(view)
}

fn save_view(transaction: &Transaction<'_>, view: &mut ViewRecord) -> Result<()> {
    view.revision = view
        .revision
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or_else(|| Error::corrupt("view revision exhausted"))?;
    transaction.execute(
        "INSERT INTO records VALUES('view',?1,?2,?3) ON CONFLICT(kind,id) DO UPDATE SET version=excluded.version,record=excluded.record",
        params![view.id.as_str(), sql_integer(view.revision)?, text(view)?],
    )?;
    Ok(())
}

fn lease_count(connection: &rusqlite::Connection, view: Option<&Id>) -> Result<u64> {
    Ok(connection.query_row(
        "SELECT count(*) FROM records WHERE kind='lease' AND json_extract(record,'$.released')=0
         AND (?1 IS NULL OR json_extract(record,'$.view')=?1)",
        [view.map(Id::as_str)],
        |row| unsigned(row, 0),
    )?)
}

fn active_owner(transaction: &Transaction<'_>, id: &Id, instance: &Id) -> Result<()> {
    let record: String = transaction.query_row(
        "SELECT record FROM owners WHERE id=?1",
        [id.as_str()],
        |row| row.get(0),
    )?;
    let record: super::OwnerRecord = serde_json::from_str(&record)?;
    if record.released || !record.registered || record.claim.instance != *instance {
        return Err(Error::new(
            ErrorCategory::StaleIdentity,
            "owner-inactive",
            "owner was released or belongs to another instance",
        ));
    }
    Ok(())
}

fn replace_view_references(transaction: &Transaction<'_>, view: &ViewRecord) -> Result<()> {
    transaction.execute(
        "DELETE FROM refs WHERE source_kind='view' AND source_id=?1",
        [view.id.as_str()],
    )?;
    if view.active {
        for target in view
            .current
            .iter()
            .filter_map(|pin| pin.incarnation.as_ref())
            .chain(view.checkpoint.iter())
        {
            Namespace::add_reference(
                transaction,
                &Id::new()?,
                ReferenceKind::View,
                view.id.as_str(),
                target,
                None,
            )?;
        }
    }
    Ok(())
}

impl ViewManager {
    pub fn new(
        namespace: Arc<Namespace>,
        legacy: Option<Arc<GenerationManager>>,
        options: WorktreeOptions,
        observer: Option<ObserveView>,
    ) -> Result<Self> {
        if (namespace.header().storage == super::policy::StorageMode::CompatibilityRetainAll)
            != legacy.is_some()
        {
            return Err(Error::incompatible(
                "compatibility mode requires its retain-all generation manager",
            ));
        }
        Ok(Self {
            namespace,
            legacy,
            options,
            observer,
            slots: Mutex::new(BTreeMap::new()),
            admission: Mutex::new(()),
        })
    }

    pub fn namespace(&self) -> &Arc<Namespace> {
        &self.namespace
    }

    pub fn accept_legacy_attach(&self, request: LegacyAttachRequest) -> Result<OperationRecord> {
        if self.legacy.is_none() {
            return Err(Error::incompatible(
                "unmanaged leases cannot attach to managed storage",
            ));
        }
        let token = Token::parse(format!(
            "legacy-{}",
            blake3::hash(request.lease.as_str().as_bytes()).to_hex()
        ))?;
        self.namespace.accept_system_operation(
            token,
            "legacy-attach",
            serde_json::to_value(request)?,
        )
    }

    pub fn accept_attach(
        &self,
        token: OperationToken,
        request: AttachRequest,
    ) -> Result<OperationRecord> {
        if token.scope != request.owner {
            return Err(Error::invalid("attachment token scope must be its owner"));
        }
        if request.accept_current.is_some() && request.revision.is_some() {
            return Err(Error::invalid(
                "choose either a new attachment revision or acceptance of the current pin",
            ));
        }
        self.namespace.validate_operation_owner(&token)?;
        self.namespace
            .accept_operation(token, "attach", serde_json::to_value(request)?)
    }

    pub fn accept_migration(
        &self,
        token: OperationToken,
        request: MigrationRequest,
    ) -> Result<OperationRecord> {
        if token.scope != request.owner {
            return Err(Error::invalid("migration token scope must be its owner"));
        }
        self.namespace.validate_operation_owner(&token)?;
        // Replay is checked before the mutable current version is inspected.
        self.namespace
            .accept_operation(token, "migrate", serde_json::to_value(request)?)
    }

    pub fn accept_refresh(
        &self,
        token: OperationToken,
        request: RefreshRequest,
    ) -> Result<OperationRecord> {
        if token.scope != request.owner {
            return Err(Error::invalid("refresh token scope must be its owner"));
        }
        self.namespace.validate_operation_owner(&token)?;
        self.namespace
            .accept_operation(token, "refresh", serde_json::to_value(request)?)
    }

    /// Queue reconciliation of invalidated or unready state. Callers requesting
    /// a full repair must invalidate first; `accept_refresh` always verifies.
    pub fn accept_reconciliation(
        &self,
        token: Token,
        request: ReconcileRequest,
    ) -> Result<OperationRecord> {
        self.namespace
            .accept_system_operation(token, "reconcile", serde_json::to_value(request)?)
    }

    pub fn active_owner(&self, view: &Id) -> Result<Option<Id>> {
        self.namespace.read(|connection| {
            let id: Option<String> = connection.query_row(
                "SELECT owners.id FROM owners JOIN records ON records.kind='lease'
                 AND json_extract(records.record,'$.owner')=owners.id
                 WHERE json_extract(records.record,'$.view')=?1 AND json_extract(records.record,'$.released')=0
                 AND json_extract(owners.record,'$.claim.instance')=?2
                 AND json_extract(owners.record,'$.registered')=1 AND json_extract(owners.record,'$.released')=0
                 ORDER BY owners.id LIMIT 1",
                params![view.as_str(),self.namespace.instance().as_str()], |row| row.get(0),
            ).optional()?;
            id.map(Id::parse).transpose()
        })
    }

    pub fn recover(&self, id: &Id) -> Result<ViewRecord> {
        self.namespace.read(|connection| view_row(connection, id))
    }

    pub fn lookup(&self, root: &Path) -> Result<Option<ViewRecord>> {
        let native = NativePath::from_path(&std::fs::canonicalize(root)?)?;
        let key = root_key(&native)?;
        self.namespace.read(|connection| {
            let id: Option<String> = connection
                .query_row(
                    "SELECT record FROM records WHERE kind='view-root' AND id=?1",
                    [&key],
                    |row| row.get(0),
                )
                .optional()?;
            id.map(|id| view_row(connection, &serde_json::from_str(&id)?))
                .transpose()
        })
    }

    fn optional_slot(&self, id: &Id) -> Result<Option<Arc<ViewSlot>>> {
        Ok(self
            .slots
            .lock()
            .map_err(|_| Error::corrupt("view registry poisoned"))?
            .get(id)
            .cloned())
    }

    pub fn lease(&self, owner: &Id, token: &Token) -> Result<LeaseRecord> {
        self.namespace
            .read(|connection| lease_row(connection, &lease_key(Some(owner), token)))?
            .ok_or_else(|| {
                Error::new(
                    ErrorCategory::CacheMissing,
                    "lease-not-found",
                    "no such owner lease",
                )
            })
    }

    pub fn legacy_lease(&self, token: &Token) -> Result<LeaseRecord> {
        self.namespace
            .read(|connection| lease_row(connection, &lease_key(None, token)))?
            .ok_or_else(|| {
                Error::new(
                    ErrorCategory::CacheMissing,
                    "lease-not-found",
                    "no such legacy lease",
                )
            })
    }

    pub fn detach_legacy_lease(&self, token: &Token) -> Result<DetachResult> {
        if self.legacy.is_none() {
            return Err(Error::incompatible("this namespace has no legacy leases"));
        }
        self.detach_participant(None, token, &self.legacy_lease(token)?.view)
    }

    pub fn detach_lease(&self, owner: &Id, token: &Token) -> Result<DetachResult> {
        let lease = self.lease(owner, token)?;
        self.detach(owner, token, &lease.view)
    }

    pub fn slot(&self, id: &Id) -> Result<Arc<ViewSlot>> {
        self.slots
            .lock()
            .map_err(|_| Error::corrupt("view registry poisoned"))?
            .get(id)
            .cloned()
            .ok_or_else(|| Error::busy("view-not-loaded"))
    }

    pub fn slots(&self) -> Result<Vec<Arc<ViewSlot>>> {
        Ok(self
            .slots
            .lock()
            .map_err(|_| Error::corrupt("view registry poisoned"))?
            .values()
            .cloned()
            .collect())
    }

    pub fn slots_page(&self, after: Option<&Id>) -> Result<(Vec<Arc<ViewSlot>>, Option<Id>)> {
        let limit = self.namespace.policy()?.policy.work.page_objects as usize;
        let registry = self
            .slots
            .lock()
            .map_err(|_| Error::corrupt("view registry poisoned"))?;
        let lower = after.cloned().map_or(Bound::Unbounded, Bound::Excluded);
        let slots: Vec<_> = registry
            .range((lower, Bound::Unbounded))
            .take(limit)
            .map(|(_, slot)| Arc::clone(slot))
            .collect();
        let next =
            (slots.len() == limit).then(|| slots.last().expect("positive page limit").id.clone());
        Ok((slots, next))
    }

    fn install_slot(&self, record: &ViewRecord, root: &Path) -> Result<Arc<ViewSlot>> {
        let slots = self
            .slots
            .lock()
            .map_err(|_| Error::corrupt("view registry poisoned"))?;
        if let Some(slot) = slots.get(&record.id) {
            return Ok(Arc::clone(slot));
        }
        if slots.len() >= self.namespace.policy()?.policy.work.max_views as usize {
            return Err(Error::pressure("active-view-limit"));
        }
        drop(slots);
        let original_root = RootedDir::open(root)?;
        if FileIdentity::of(&original_root.directory_handle()?)? != record.root_identity {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "worktree-replaced",
                "worktree identity changed before preparation",
            ));
        }
        let slot = Arc::new(ViewSlot {
            id: record.id.clone(),
            root: root.into(),
            namespace: Arc::clone(&self.namespace),
            epoch: AtomicU64::new(record.input_epoch),
            serial: Mutex::new(()),
            state: Mutex::new(SlotState {
                current: None,
                preparing: None,
                original_root: Some(original_root),
                work: None,
                closing: false,
                draining: false,
                queries: 0,
                retired: Vec::new(),
                observer: None,
                error: None,
                last_stats: None,
            }),
        });
        if let Some(observer) = &self.observer {
            let observer = observer(&slot)?;
            slot.state
                .lock()
                .map_err(|_| Error::corrupt("view lock poisoned"))?
                .observer = Some(observer);
        }
        let mut slots = self
            .slots
            .lock()
            .map_err(|_| Error::corrupt("view registry poisoned"))?;
        if let Some(existing) = slots.get(&record.id) {
            return Ok(Arc::clone(existing));
        }
        if slots.len() >= self.namespace.policy()?.policy.work.max_views as usize {
            return Err(Error::pressure("active-view-limit"));
        }
        {
            let mut live = self
                .namespace
                .live_views
                .lock()
                .map_err(|_| Error::corrupt("view metadata registry poisoned"))?;
            live.retain(|_, views| {
                views.retain(|view| view.strong_count() != 0);
                !views.is_empty()
            });
            live.entry(slot.id.clone())
                .or_default()
                .push(Arc::downgrade(&slot));
        }
        slots.insert(slot.id.clone(), Arc::clone(&slot));
        Ok(slot)
    }

    pub(super) fn reserve(
        &self,
        operation: &Id,
        owner: Option<&Id>,
        allocation_version: u64,
    ) -> Result<Arc<WorkPermit>> {
        self.namespace
            .reserve_view(operation, owner, allocation_version)
    }

    /// Execute an accepted job on a bounded worker, never on a query/router thread.
    pub fn execute(&self, id: &Id) -> Result<OperationRecord> {
        let _receipt = self.namespace.hold_operation(id)?;
        let operation = self.namespace.operation(id)?;
        if matches!(
            operation.state,
            OperationState::Completed | OperationState::Failed | OperationState::Cancelled
        ) {
            return Ok(operation);
        }
        if operation.committed_state != CommitState::NotCommitted {
            return Err(Error::new(
                ErrorCategory::RecoveryRequired,
                "operation-commit-needs-recovery",
                "read the authoritative view before resuming committed work",
            )
            .committed(operation.committed_state));
        }
        let result = match operation.kind.as_str() {
            "attach" => self.execute_attach(
                &operation,
                serde_json::from_value(operation.request.clone())?,
                false,
            ),
            "legacy-attach" => {
                if self.legacy.is_none() || operation.token.scope != *self.namespace.instance() {
                    return Err(Error::incompatible(
                        "legacy attachment needs a compatibility service",
                    ));
                }
                let request: LegacyAttachRequest =
                    serde_json::from_value(operation.request.clone())?;
                self.execute_attach(
                    &operation,
                    AttachRequest {
                        root: request.root,
                        revision: Some(request.revision),
                        profile: request.profile,
                        lease: request.lease,
                        owner: self.namespace.instance().clone(),
                        accept_current: None,
                        migratable: false,
                        allocation_version: self.namespace.allocation()?.version,
                    },
                    true,
                )
            }
            "migrate" => self.execute_migration(
                &operation,
                serde_json::from_value(operation.request.clone())?,
            ),
            "refresh" => self.execute_refresh(
                &operation,
                serde_json::from_value(operation.request.clone())?,
            ),
            "reconcile" => {
                let request: ReconcileRequest = serde_json::from_value(operation.request.clone())?;
                if operation.token.scope != *self.namespace.instance() {
                    return Err(Error::invalid(
                        "reconciliation needs the current service scope",
                    ));
                }
                self.refresh_view(
                    &operation,
                    &request.view,
                    request.expected_version,
                    request.allocation_version,
                    None,
                )
            }
            "adaptive" => self.execute_adaptive(
                &operation,
                serde_json::from_value(operation.request.clone())?,
            ),
            _ => return Err(Error::invalid("operation is not a view lifecycle job")),
        };
        if let Err(mut error) = result {
            if error.category == ErrorCategory::Busy
                && matches!(
                    error.reason_code.as_str(),
                    "view-work-active"
                        | "generation-already-materializing"
                        | "view-attachment-preparing"
                        | "object-retirement-in-progress"
                        | "catalog-checkpoint-readers-active"
                )
            {
                match self.namespace.defer_operation(id, &error.reason_code) {
                    Ok(()) => return self.namespace.operation(id),
                    Err(deferred) => error = deferred,
                }
            }
            self.namespace.record_operation_result(
                id,
                Err(error.operation(id.to_string())),
                false,
            )?;
        }
        self.namespace.operation(id)
    }

    fn execute_attach(
        &self,
        operation: &OperationRecord,
        request: AttachRequest,
        legacy: bool,
    ) -> Result<()> {
        let owner = (!legacy).then_some(&request.owner);
        let key = lease_key(owner, &request.lease);
        let previous = self
            .namespace
            .read(|connection| lease_row(connection, &key))?;
        let root = std::fs::canonicalize(&request.root)?;
        let native = NativePath::from_path(&root)?;
        let original = OriginalAttachment {
            root: native.clone(),
            revision: request.revision.clone(),
            profile: request.profile,
            accept_current: request.accept_current.clone(),
            migratable: request.migratable,
        };
        if let Some(previous) = previous {
            if text(&previous.original)? != text(&original)? {
                return Err(Error::new(
                    ErrorCategory::StaleVersion,
                    "lease-token-conflict",
                    "lease token has different original attachment inputs",
                ));
            }
            if previous.released || previous.instance != *self.namespace.instance() {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "lease-released",
                    "released or old-instance tokens cannot attach again",
                ));
            }
            self.namespace.fault(
                super::faults::Point::AttachBeforeResume,
                Some(&operation.id),
            )?;
            let record = self.recover(&previous.view)?;
            let loaded = self
                .optional_slot(&record.id)?
                .map(|slot| {
                    slot.state
                        .lock()
                        .map(|state| {
                            state.current.as_ref().is_some_and(|current| {
                                current.record.version == record.version
                                    && current.record.current == record.current
                            })
                        })
                        .map_err(|_| Error::corrupt("view lock poisoned"))
                })
                .transpose()?
                .unwrap_or(false);
            if record.committed && loaded {
                self.namespace.record_operation_result(
                    &operation.id,
                    Ok(json!({"lease":previous,"current":record})),
                    true,
                )?;
                return Ok(());
            }
            return self.materialize_attachment(operation, &request, &previous, record, &root);
        }
        let discovery = super::process::Control::bootstrap();
        let repository = Repository::discover_controlled(&root, &discovery)?;
        if repository.identity() != self.namespace.header().repository
            || Repository::root_controlled(&root, &discovery)? != root
        {
            return Err(Error::invalid(
                "attachment root must belong to this repository",
            ));
        }
        let root_id = FileIdentity::of(&RootedDir::open(&root)?.directory_handle()?)?;
        self.drain_released()?;
        let _admission = self
            .admission
            .lock()
            .map_err(|_| Error::corrupt("view admission poisoned"))?;
        let root_key = root_key(&native)?;
        let current: Option<ViewRecord> = self.namespace.read(|connection| {
            let id: Option<String> = connection
                .query_row(
                    "SELECT record FROM records WHERE kind='view-root' AND id=?1",
                    [&root_key],
                    |row| row.get(0),
                )
                .optional()?;
            id.map(|id| view_row(connection, &serde_json::from_str(&id)?))
                .transpose()
        })?;
        let current = current
            .map(|mut record| -> Result<ViewRecord> {
                self.namespace.transaction(|transaction| {
                    if record.active && lease_count(transaction, Some(&record.id))? == 0 {
                        record.active = false;
                        save_view(transaction, &mut record)?;
                        replace_view_references(transaction, &record)?;
                    }
                    Ok(())
                })?;
                Ok(record)
            })
            .transpose()?;
        let current = match current {
            Some(record) if !record.active => {
                if request.accept_current.is_some() {
                    return Err(Error::new(
                        ErrorCategory::StaleIdentity,
                        "accepted-view-inactive",
                        "the old view is inactive; attach a fresh view with an explicit revision",
                    ));
                }
                if let Some(anchor) = &record.root_anchor {
                    if self.namespace.root_busy(anchor)? {
                        return Err(Error::busy("previous-root-readers-active"));
                    }
                    self.namespace.withdraw_root(anchor)?;
                }
                None
            }
            current => current,
        };
        let mut record = if let Some(record) = current {
            if record.root_identity != root_id {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "worktree-replaced",
                    "root identity differs from the authoritative view",
                ));
            }
            if !record.committed {
                return Err(Error::busy("view-attachment-preparing"));
            }
            if legacy {
                let (commit, _) = self.namespace.generation_key(
                    &repository,
                    request
                        .revision
                        .as_deref()
                        .ok_or_else(|| Error::invalid("legacy attachment needs a revision"))?,
                    request.profile,
                )?;
                if record.version != 1 || record.pin()?.commit != commit {
                    return Err(Error::new(
                        ErrorCategory::StaleVersion,
                        "legacy-pin-conflict",
                        "legacy clients cannot acknowledge an advanced or different exact pin",
                    ));
                }
            } else {
                let expected = request.accept_current.as_ref().ok_or_else(|| Error::new(
                    ErrorCategory::StaleVersion, "accept-current-required", "joining a known worktree must explicitly accept its authoritative pin/version",
                ).committed(CommitState::NotCommitted))?;
                if expected.view != record.id || expected.version != record.version {
                    return Err(Error::stale_version(record.version));
                }
            }
            if request.profile != *record.pin()?.key.profile() {
                return Err(Error::incompatible(
                    "joining profile differs from current pin",
                ));
            }
            record
        } else {
            if request.accept_current.is_some() {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "unknown-accepted-view",
                    "no such current worktree view",
                ));
            }
            let (commit, managed_key) = self.namespace.generation_key(
                &repository,
                request.revision.as_deref().unwrap_or("HEAD"),
                request.profile,
            )?;
            let generation_key = if self.legacy.is_some() {
                crate::generations::GenerationKey::legacy(
                    &repository,
                    managed_key.tree_oid().into(),
                    request.profile,
                )
            } else {
                managed_key
            };
            ViewRecord {
                id: Id::new()?,
                root: native.clone(),
                root_identity: root_id,
                root_anchor: None,
                version: 1,
                revision: 0,
                current: None,
                pending: Some(PinIntent {
                    commit,
                    key: generation_key,
                }),
                checkpoint: None,
                checkpoint_binding: None,
                input_epoch: 0,
                reconciled_epoch: None,
                active: false,
                committed: false,
                instance: self.namespace.instance().clone(),
            }
        };
        let lease = LeaseRecord {
            token: request.lease.clone(),
            owner: owner.cloned(),
            instance: self.namespace.instance().clone(),
            view: record.id.clone(),
            original,
            exact_original_commit: record.intent()?.commit,
            original_version: record.version,
            released: false,
            operation: operation.id.clone(),
        };
        self.namespace.transaction(|transaction| {
            super::work::ensure_admission(transaction)?;
            if let Some(owner) = owner { active_owner(transaction, owner, self.namespace.instance())?; }
            let policy: String = transaction.query_row("SELECT policy FROM state WHERE singleton=1", [], |row| row.get(0))?;
            let policy: super::Policy = serde_json::from_str(&policy)?;
            if lease_count(transaction, None)? >= u64::from(policy.work.max_leases) { return Err(Error::pressure("lease-limit")); }
            record.active = true;
            record.instance = self.namespace.instance().clone();
            save_view(transaction, &mut record)?;
            self.namespace.fault(
                super::faults::Point::AttachBeforeLease, Some(&operation.id),
            )?;
            transaction.execute("INSERT INTO records VALUES('lease',?1,1,?2)", params![key, text(&lease)?])?;
            transaction.execute("INSERT INTO records VALUES('view-root',?1,1,?2) ON CONFLICT(kind,id) DO UPDATE SET record=excluded.record",
                params![root_key, text(&record.id)?])?;
            if record.committed { replace_view_references(transaction, &record)?; }
            let stored: String = transaction.query_row("SELECT record FROM operations WHERE id=?1",
                [operation.id.as_str()], |row| row.get(0))?;
            let mut accepted: OperationRecord = serde_json::from_str(&stored)?;
            accepted.progress["view"] = json!(record.id);
            accepted.progress["resolved_commit"] = json!(lease.exact_original_commit);
            Namespace::save_operation(transaction, &accepted)?;
            Ok(())
        })?;
        self.namespace.fault(
            super::faults::Point::AttachBeforeResume,
            Some(&operation.id),
        )?;
        let slot = self.install_slot(&record, &root)?;
        slot.state
            .lock()
            .map_err(|_| Error::corrupt("view lock poisoned"))?
            .closing = false;
        drop(_admission);
        let mut state = slot
            .state
            .lock()
            .map_err(|_| Error::corrupt("view lock poisoned"))?;
        if let Some(current) = &state.current
            && current.record.version == record.version
            && current.record.current == record.current
        {
            state.current = Some(Arc::new(PublishedView {
                record: record.clone(),
                view: Arc::clone(&current.view),
            }));
            drop(state);
            self.namespace.record_operation_result(
                &operation.id,
                Ok(json!({"lease":lease,"current":record})),
                true,
            )?;
            return Ok(());
        }
        drop(state);
        self.materialize_attachment(operation, &request, &lease, record, &root)
    }

    fn materialize_attachment(
        &self,
        operation: &OperationRecord,
        request: &AttachRequest,
        lease: &LeaseRecord,
        record: ViewRecord,
        root: &Path,
    ) -> Result<()> {
        let slot = self.install_slot(&record, root)?;
        let _serial = slot
            .serial
            .try_lock()
            .map_err(|_| Error::busy("view-work-active"))?;
        let permit = self.reserve(
            &operation.id,
            lease.owner.as_ref(),
            request.allocation_version,
        )?;
        let _work = slot.begin_work(operation)?;
        let (record, view, stats) = self.materialize_view(record, root, &permit)?;
        slot.install_preparing(Arc::clone(&view))?;
        let (checkpoint, reconcile, input) =
            self.prepare_checkpoint(&slot, &view, &record, &permit)?;
        self.publish(
            &slot,
            operation,
            record,
            view,
            checkpoint,
            reconcile,
            input,
            json!({"lease":lease,"build":stats}),
            &permit,
            false,
        )
    }

    fn materialize_view(
        &self,
        mut record: ViewRecord,
        root: &Path,
        permit: &Arc<WorkPermit>,
    ) -> Result<(ViewRecord, Arc<WorktreeView>, BuildStats)> {
        let repository =
            Repository::discover_controlled(root, &super::process::Control::work(permit))?;
        let intent = record.intent()?;
        let cached = record.current.as_ref().map(|pin| -> Result<_> {
            let generation = match (&self.legacy, &pin.incarnation) {
                (Some(legacy), None) => legacy.open_controlled(&pin.key, permit)?,
                (None, Some(incarnation)) => self
                    .namespace
                    .open_generation_controlled(incarnation, Some(permit))?,
                _ => return Err(Error::incompatible("current pin uses another storage mode")),
            };
            pin.validate_generation(&generation)?;
            let stats = BuildStats {
                reused_generation: true,
                tracked_entries: generation.entries().len(),
                ..BuildStats::default()
            };
            Ok((pin.clone(), generation, stats))
        });
        let cached = match cached.transpose() {
            Ok(cached) => cached,
            Err(error)
                if matches!(
                    error.category,
                    ErrorCategory::CacheEvicted | ErrorCategory::CacheMissing
                ) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        let (pin, generation, stats) = match cached {
            Some(cached) => cached,
            None => self.materialize(
                &repository,
                &intent.commit,
                *intent.key.profile(),
                None,
                permit,
            )?,
        };
        let view = match (&record.checkpoint, &record.checkpoint_binding) {
            (Some(checkpoint), Some(binding)) if record.current.as_ref() == Some(&pin) => {
                match self.namespace.restore_checkpoint(
                    checkpoint,
                    binding,
                    root,
                    Arc::clone(&generation),
                    self.options.clone(),
                    permit,
                ) {
                    Ok(view) => view,
                    Err(error)
                        if matches!(
                            error.category,
                            ErrorCategory::CacheEvicted | ErrorCategory::CacheMissing
                        ) =>
                    {
                        WorktreeView::new_controlled(
                            root,
                            generation,
                            self.options.clone(),
                            permit,
                        )?
                    }
                    Err(error) => return Err(error),
                }
            }
            (None, None) | (Some(_), Some(_)) => {
                WorktreeView::new_controlled(root, generation, self.options.clone(), permit)?
            }
            _ => {
                return Err(Error::corrupt(
                    "authoritative checkpoint identity and binding disagree",
                ));
            }
        };
        if view.root_identity()? != record.root_identity {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "worktree-replaced",
                "the restored view does not use the authoritative worktree identity",
            ));
        }
        record.current = Some(pin);
        record.pending = None;
        Ok((record, Arc::new(view), stats))
    }

    fn materialize(
        &self,
        repository: &Repository,
        commit: &str,
        profile: IndexingProfile,
        previous: Option<&Arc<Generation>>,
        permit: &Arc<WorkPermit>,
    ) -> Result<(CurrentPin, Arc<Generation>, BuildStats)> {
        if let Some(legacy) = &self.legacy {
            permit.check()?;
            let built = legacy.ensure_controlled(commit, profile, previous, permit)?;
            permit.check()?;
            self.namespace
                .record_compatibility_generation(&built.generation)?;
            let pin = CurrentPin {
                commit: commit.into(),
                key: built.generation.key().clone(),
                incarnation: None,
                fingerprint: built.generation.base().snapshot_id(),
            };
            return Ok((pin, built.generation, built.stats));
        }
        let built = self
            .namespace
            .ensure_generation(repository, commit, profile, previous, permit)?;
        Ok((
            CurrentPin::from_materialization(&built),
            built.generation,
            built.descriptor.stats,
        ))
    }

    fn check_participant(
        &self,
        view: &Id,
        owner: &Id,
        version: u64,
        require_migration: bool,
    ) -> Result<()> {
        self.namespace.validate_active_owner(owner)?;
        self.namespace.read(|connection| {
            let current = view_row(connection, view)?;
            if current.version != version { return Err(Error::stale_version(current.version)); }
            if !current.active { return Err(Error::busy("view-inactive")); }
            let participant: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM records WHERE kind='lease' AND json_extract(record,'$.view')=?1
                 AND json_extract(record,'$.owner')=?2 AND json_extract(record,'$.released')=0)",
                params![view.as_str(), owner.as_str()], |row| row.get(0),
            )?;
            if !participant { return Err(Error::new(ErrorCategory::StaleIdentity, "owner-not-attached", "owner has no active lease on this view")); }
            let fixed: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM records WHERE kind='lease' AND json_extract(record,'$.view')=?1
                 AND json_extract(record,'$.released')=0 AND json_extract(record,'$.original.migratable')=0)",
                [view.as_str()], |row| row.get(0),
            )?;
            if require_migration && fixed { return Err(Error::busy("fixed-pin-participant")); }
            Ok(())
        })
    }

    pub(super) fn check_migratable(&self, view: &Id, owner: &Id, version: u64) -> Result<()> {
        self.check_participant(view, owner, version, true)
    }

    pub fn invalidate(
        &self,
        view: &Id,
        owner: &Id,
        version: u64,
        changed: &[PathBuf],
        full: bool,
    ) -> Result<u64> {
        if changed.len() > self.namespace.policy()?.policy.work.queue_items as usize {
            return Err(Error::pressure("invalidation-path-limit"));
        }
        self.check_participant(view, owner, version, false)?;
        self.slot(view)?
            .invalidate_inner(Some(version), changed, full)
    }

    pub(super) fn execute_migration(
        &self,
        operation: &OperationRecord,
        request: MigrationRequest,
    ) -> Result<()> {
        self.check_migratable(&request.view, &request.owner, request.expected_version)?;
        let slot = self.slot(&request.view)?;
        if std::fs::canonicalize(&request.root)? != slot.root {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "migration-root-mismatch",
                "migration names another worktree",
            ));
        }
        let _serial = slot
            .serial
            .try_lock()
            .map_err(|_| Error::busy("view-work-active"))?;
        let authoritative = self.recover(&request.view)?;
        if authoritative.version != request.expected_version {
            return Err(Error::stale_version(authoritative.version));
        }
        let current = slot.current()?;
        if !current.record.same_publication(&authoritative) {
            return Err(Error::new(
                ErrorCategory::RecoveryRequired,
                "view-publication-needs-recovery",
                "refresh the authoritative publication before advancing it again",
            ));
        }
        if request.profile != *current.record.pin()?.key.profile() {
            return Err(Error::incompatible(
                "migration cannot change the attached indexing profile",
            ));
        }
        let permit = self.reserve(
            &operation.id,
            Some(&request.owner),
            request.allocation_version,
        )?;
        let _work = slot.begin_work(operation)?;
        let (commit, _) = current.view.repository().resolve_controlled(
            &request.target_commit,
            &super::process::Control::work(&permit),
        )?;
        if commit != request.target_commit {
            return Err(Error::invalid("migration requires an exact commit"));
        }
        self.namespace.transaction(|transaction| {
            for target in current
                .record
                .pin()?
                .incarnation
                .iter()
                .chain(current.record.checkpoint.iter())
            {
                Namespace::add_reference(
                    transaction,
                    &Id::new()?,
                    ReferenceKind::Migration,
                    operation.id.as_str(),
                    target,
                    Some(&request.owner),
                )?;
            }
            Ok(())
        })?;
        let (pin, generation, stats) = self.materialize(
            current.view.repository(),
            &commit,
            request.profile,
            Some(current.view.generation()),
            &permit,
        )?;
        let replacement = Arc::new(current.view.replacement(generation, &permit)?);
        let mut record = authoritative;
        record.version = record
            .version
            .checked_add(1)
            .filter(|value| *value <= i64::MAX as u64)
            .ok_or_else(|| Error::corrupt("view version exhausted"))?;
        record.current = Some(pin);
        slot.install_preparing(Arc::clone(&replacement))?;
        let (checkpoint, reconcile, input) =
            self.prepare_checkpoint(&slot, &replacement, &record, &permit)?;
        self.check_migratable(&request.view, &request.owner, request.expected_version)?;
        if operation.kind == "adaptive" {
            let decision: super::AdaptiveDecision =
                serde_json::from_value(operation.progress["adaptive"].clone())?;
            let policy = self.namespace.policy()?.policy;
            let super::policy::Advancement::Adaptive {
                min_reduction_bytes,
                min_reduction_percent,
                ..
            } = policy.advancement
            else {
                return Err(Error::incompatible(
                    "adaptive policy changed during preparation",
                ));
            };
            if !super::adaptive::sufficient(
                decision.overlay_bytes,
                replacement.overlay_cost()?.bytes,
                min_reduction_bytes,
                min_reduction_percent,
            ) {
                return Err(Error::busy("adaptive-insufficient-actual-benefit"));
            }
        }
        self.publish(
            &slot,
            operation,
            record,
            replacement,
            checkpoint,
            reconcile,
            input,
            json!({"build":stats}),
            &permit,
            true,
        )
    }

    fn execute_refresh(&self, operation: &OperationRecord, request: RefreshRequest) -> Result<()> {
        self.namespace.validate_active_owner(&request.owner)?;
        let participant: bool = self.namespace.read(|connection| Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM records WHERE kind='lease' AND json_extract(record,'$.view')=?1
             AND json_extract(record,'$.owner')=?2 AND json_extract(record,'$.released')=0)",
            params![request.view.as_str(),request.owner.as_str()], |row| row.get(0),
        )?))?;
        if !participant {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "owner-not-attached",
                "refresh requires an active lease on this view",
            ));
        }
        self.refresh_view(
            operation,
            &request.view,
            request.expected_version,
            request.allocation_version,
            Some(&request.owner),
        )
    }

    fn refresh_view(
        &self,
        operation: &OperationRecord,
        view: &Id,
        expected_version: u64,
        allocation_version: u64,
        owner: Option<&Id>,
    ) -> Result<()> {
        let slot = self.slot(view)?;
        let _serial = slot
            .serial
            .try_lock()
            .map_err(|_| Error::busy("view-work-active"))?;
        let mut record = self.recover(view)?;
        if record.version != expected_version {
            return Err(Error::stale_version(record.version));
        }
        let permit = self.reserve(&operation.id, owner, allocation_version)?;
        let _work = slot.begin_work(operation)?;
        if operation.kind == "reconcile"
            && self.complete_current_reconciliation(&slot, operation, &record, &permit)?
        {
            return Ok(());
        }
        let current = slot
            .state
            .lock()
            .map_err(|_| Error::corrupt("view lock poisoned"))?
            .current
            .clone();
        let (view, result) = match current {
            Some(current) if current.record.same_publication(&record) => {
                (Arc::clone(&current.view), json!({}))
            }
            _ => {
                let expected_pin = record.current.clone();
                let (restored, view, stats) =
                    self.materialize_view(record, slot.root(), &permit)?;
                if expected_pin.is_some() && restored.current != expected_pin {
                    return Err(Error::new(
                        ErrorCategory::RecoveryRequired,
                        "committed-generation-unavailable",
                        "refresh cannot replace the physical incarnation of a committed pin",
                    ));
                }
                record = restored;
                slot.install_preparing(Arc::clone(&view))?;
                (view, json!({"build":stats}))
            }
        };
        let (checkpoint, reconcile, input) =
            self.prepare_checkpoint(&slot, &view, &record, &permit)?;
        self.publish(
            &slot, operation, record, view, checkpoint, reconcile, input, result, &permit, false,
        )
    }

    fn complete_current_reconciliation(
        &self,
        slot: &ViewSlot,
        operation: &OperationRecord,
        record: &ViewRecord,
        permit: &WorkPermit,
    ) -> Result<bool> {
        let state = slot
            .state
            .lock()
            .map_err(|_| Error::corrupt("view lock poisoned"))?;
        if state.closing {
            return Err(Error::busy("view-closing"));
        }
        let Some(current) = &state.current else {
            return Ok(false);
        };
        let status = current.view.status()?;
        if !current.record.same_publication(record)
            || slot.epoch.load(Ordering::Acquire) != record.input_epoch
            || !status.ready
            || status.published_epoch != record.reconciled_epoch
        {
            return Ok(false);
        }
        let stats = ReconcileStats {
            epoch: record
                .reconciled_epoch
                .ok_or_else(|| Error::corrupt("ready view has no reconciled epoch"))?,
            ..ReconcileStats::default()
        };
        self.namespace.fault(
            super::faults::Point::ReconcileBeforeComplete,
            Some(&operation.id),
        )?;
        permit.check_now()?;
        self.namespace.transaction(|transaction| {
            super::work::ensure_admission(transaction)?;
            let allocation = super::work::allocation_row(transaction)?;
            if allocation.version != permit.record.request.allocation_version {
                return Err(Error::stale_version(allocation.version));
            }
            let previous = view_row(transaction, &record.id)?;
            if previous.version != record.version {
                return Err(Error::stale_version(previous.version));
            }
            if !previous.same_publication(record) {
                return Err(Error::busy("publication-state-changed"));
            }
            if lease_count(transaction, Some(&record.id))? == 0 {
                return Err(Error::busy("view-has-no-leases"));
            }
            let mut receipt: OperationRecord =
                serde_json::from_str(&transaction.query_row::<String, _, _>(
                    "SELECT record FROM operations WHERE id=?1",
                    [operation.id.as_str()],
                    |row| row.get(0),
                )?)?;
            if receipt.cancelled {
                return Err(Error::new(
                    ErrorCategory::Cancelled,
                    "operation-cancelled",
                    "cancelled before reconciliation completion",
                ));
            }
            receipt.committed_state = CommitState::Committed;
            receipt.state = OperationState::Completed;
            receipt.result = Some(json!({
                "current": previous,
                "reconcile": stats,
                "coalesced": true,
            }));
            Namespace::save_operation(transaction, &receipt)
        })?;
        Ok(true)
    }

    fn prepare_checkpoint(
        &self,
        slot: &ViewSlot,
        view: &WorktreeView,
        record: &ViewRecord,
        permit: &Arc<WorkPermit>,
    ) -> Result<(super::ProtectedCheckpoint, ReconcileStats, u64)> {
        let attempts = if self.namespace.operation(permit.operation_id())?.kind == "adaptive" {
            match self.namespace.policy()?.policy.advancement {
                super::policy::Advancement::Adaptive { max_attempts, .. } => {
                    RECONCILE_ATTEMPTS.min(max_attempts)
                }
                _ => {
                    return Err(Error::incompatible(
                        "adaptive policy changed before reconciliation",
                    ));
                }
            }
        } else {
            RECONCILE_ATTEMPTS
        };
        for _ in 0..attempts {
            permit.check()?;
            let input = slot.epoch.load(Ordering::Acquire);
            let stats = match view.refresh_controlled(permit) {
                Ok(stats) => stats,
                Err(crate::worktrees::WorktreeError::ChangedDuringReconcile) => continue,
                Err(error) => return Err(error.into()),
            };
            if slot.epoch.load(Ordering::Acquire) != input {
                continue;
            }
            let checkpoint = self.namespace.save_checkpoint(
                view,
                &record.id,
                record.version,
                input,
                record.pin()?,
                permit,
            )?;
            if slot.epoch.load(Ordering::Acquire) == input
                && view.status()?.published_epoch == Some(stats.epoch)
            {
                return Ok((checkpoint, stats, input));
            }
        }
        Err(Error::busy("view-preparation-invalidated"))
    }

    #[allow(clippy::too_many_arguments)]
    fn publish(
        &self,
        slot: &ViewSlot,
        operation: &OperationRecord,
        mut record: ViewRecord,
        view: Arc<WorktreeView>,
        checkpoint: super::ProtectedCheckpoint,
        stats: ReconcileStats,
        input: u64,
        mut result: Value,
        permit: &Arc<WorkPermit>,
        migration: bool,
    ) -> Result<()> {
        if migration {
            self.namespace
                .fault(super::faults::Point::MigrationPrepared, Some(&operation.id))?;
        }
        self.namespace
            .fault(super::faults::Point::ViewBeforeCommit, Some(&operation.id))?;
        let mut state = slot
            .state
            .lock()
            .map_err(|_| Error::corrupt("view lock poisoned"))?;
        if state.closing {
            return Err(Error::busy("view-closing"));
        }
        if slot.epoch.load(Ordering::Acquire) != input {
            return Err(Error::busy("publication-epoch-changed"));
        }
        if view.status()?.published_epoch != Some(stats.epoch) {
            return Err(Error::busy("checkpoint-epoch-changed"));
        }
        permit.check()?;
        let expected_publication = if migration {
            state
                .current
                .as_ref()
                .ok_or_else(|| Error::busy("view-not-ready"))?
                .record
                .clone()
        } else {
            record.clone()
        };
        record.checkpoint = Some(checkpoint.descriptor.incarnation.clone());
        record.checkpoint_binding = Some(checkpoint.descriptor.binding.clone());
        record.root_anchor = view.root_anchor().cloned();
        record.reconciled_epoch = Some(stats.epoch);
        record.input_epoch = input;
        record.committed = true;
        record.instance = self.namespace.instance().clone();
        let commitment = self.namespace.transaction(|transaction| {
            super::work::ensure_admission(transaction)?;
            let previous = view_row(transaction, &record.id)?;
            let expected = if migration { record.version - 1 } else { record.version };
            if previous.version != expected {
                return Err(Error::stale_version(previous.version));
            }
            if previous.revision != record.revision {
                if !previous.same_publication(&expected_publication) {
                    return Err(Error::busy("publication-state-changed"));
                }
                record.revision = previous.revision;
            }
            if lease_count(transaction, Some(&record.id))? == 0 { return Err(Error::busy("view-has-no-leases")); }
            if migration {
                let fixed: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM records WHERE kind='lease' AND json_extract(record,'$.view')=?1
                     AND json_extract(record,'$.released')=0 AND json_extract(record,'$.original.migratable')=0)",
                    [record.id.as_str()], |row| row.get(0),
                )?;
                if fixed { return Err(Error::busy("fixed-pin-participant")); }
            }
            let mut current_operation: OperationRecord = serde_json::from_str(&transaction.query_row::<String, _, _>(
                "SELECT record FROM operations WHERE id=?1", [operation.id.as_str()], |row| row.get(0),
            )?)?;
            if current_operation.cancelled { return Err(Error::new(ErrorCategory::Cancelled, "operation-cancelled", "cancelled before view commitment")); }
            save_view(transaction, &mut record)?;
            replace_view_references(transaction, &record)?;
            transaction.execute("DELETE FROM refs WHERE source_kind='migration' AND source_id=?1", [operation.id.as_str()])?;
            result["current"] = serde_json::to_value(&record)?;
            result["reconcile"] = serde_json::to_value(&stats)?;
            current_operation.committed_state = CommitState::Committed;
            current_operation.state = OperationState::Completed;
            current_operation.result = Some(result);
            Namespace::save_operation(transaction, &current_operation)
        }).and_then(|()| self.namespace.fault(super::faults::Point::ViewAfterCommit, Some(&operation.id))
            .map_err(|error| error.committed(CommitState::Committed)));
        if let Err(error) = commitment {
            if error.committed_state != CommitState::NotCommitted {
                slot.epoch.fetch_add(1, Ordering::AcqRel);
                state.error = Some(serde_json::to_value(&error)?);
            }
            return Err(error);
        }
        // Invalidation advances the common clock before taking this publication gate.
        if slot.epoch.load(Ordering::Acquire) != input {
            view.invalidate_all()
                .map_err(|error| Error::from(error).committed(CommitState::Committed))?;
        }
        let published = Arc::new(PublishedView { record, view });
        if let Some(old) = state.current.replace(published) {
            state.retired.push(Arc::downgrade(&old));
        }
        state.retired.retain(|view| view.strong_count() != 0);
        state.preparing = None;
        state.original_root = None;
        state.error = None;
        state.last_stats = Some(stats);
        self.namespace
            .fault(super::faults::Point::ViewAfterSwap, Some(&operation.id))
            .map_err(|error| error.committed(CommitState::Committed))?;
        Ok(())
    }

    pub fn status(&self, id: &Id) -> Result<ViewStatus> {
        let authoritative = self.recover(id)?;
        let leases = self
            .namespace
            .read(|connection| lease_count(connection, Some(id)))?;
        let Some(slot) = self.optional_slot(id)? else {
            return Ok(ViewStatus {
                input_epoch: authoritative.input_epoch,
                closing: !authoritative.active,
                authoritative,
                leases,
                ready: false,
                queries: 0,
                old_snapshots: 0,
                work: None,
                overlay: None,
                last_reconcile: None,
                error: None,
            });
        };
        let state = slot
            .state
            .lock()
            .map_err(|_| Error::corrupt("view lock poisoned"))?;
        let ready = if let Some(current) = &state.current {
            let status = current.view.status()?;
            !state.closing
                && current.record.same_publication(&authoritative)
                && current.record.input_epoch == slot.epoch.load(Ordering::Acquire)
                && status.ready
                && status.published_epoch == current.record.reconciled_epoch
        } else {
            false
        };
        Ok(ViewStatus {
            authoritative,
            ready,
            input_epoch: slot.epoch.load(Ordering::Acquire),
            queries: state.queries,
            old_snapshots: state
                .retired
                .iter()
                .filter(|view| view.strong_count() != 0)
                .count(),
            leases,
            closing: state.closing,
            work: state.work.clone(),
            overlay: state
                .current
                .as_ref()
                .map(|current| current.view.overlay_cost())
                .transpose()?,
            last_reconcile: state.last_stats.clone(),
            error: state.error.clone(),
        })
    }

    pub fn detach(&self, owner: &Id, token: &Token, view: &Id) -> Result<DetachResult> {
        self.detach_participant(Some(owner), token, view)
    }

    fn detach_participant(
        &self,
        owner: Option<&Id>,
        token: &Token,
        view: &Id,
    ) -> Result<DetachResult> {
        let key = lease_key(owner, token);
        let slot = self.optional_slot(view)?;
        let mut state = slot
            .as_ref()
            .map(|slot| {
                slot.state
                    .lock()
                    .map_err(|_| Error::corrupt("view lock poisoned"))
            })
            .transpose()?;
        let (released, remaining, total) = self.namespace.transaction(|transaction| {
            let mut lease = lease_row(transaction, &key)?.ok_or_else(|| {
                Error::new(
                    ErrorCategory::CacheMissing,
                    "lease-not-found",
                    "lease token is unknown",
                )
            })?;
            if lease.view != *view || lease.owner.as_ref() != owner {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "lease-identity-mismatch",
                    "lease belongs to another view or owner",
                ));
            }
            let released = !lease.released;
            lease.released = true;
            transaction.execute(
                "UPDATE records SET version=version+1,record=?2 WHERE kind='lease' AND id=?1",
                params![key, text(&lease)?],
            )?;
            Ok((
                released,
                lease_count(transaction, Some(view))?,
                lease_count(transaction, None)?,
            ))
        })?;
        (|| {
            if remaining == 0
                && let Some(state) = state.as_mut()
            {
                state.closing = true;
                if let Some(operation) = &state.work {
                    self.namespace.cancel_operation(operation)?;
                }
            }
            drop(state);
            if let Some(slot) = &slot {
                slot.drain()?;
            } else if remaining == 0 {
                self.namespace.transaction(|transaction| {
                    let mut record = view_row(transaction, view)?;
                    if record.active && lease_count(transaction, Some(view))? == 0 {
                        record.active = false;
                        save_view(transaction, &mut record)?;
                        replace_view_references(transaction, &record)?;
                    }
                    Ok(())
                })?;
            }
            let record = self.recover(view)?;
            let state = slot
                .as_ref()
                .map(|slot| {
                    slot.state
                        .lock()
                        .map_err(|_| Error::corrupt("view lock poisoned"))
                })
                .transpose()?;
            let queries = state.as_ref().map_or(0, |state| state.queries);
            let work = state.as_ref().and_then(|state| state.work.clone());
            let released_handles = if remaining != 0
                || state
                    .as_ref()
                    .is_some_and(|state| state.current.is_some() || state.work.is_some())
            {
                false
            } else if let Some(anchor) = &record.root_anchor {
                !self.namespace.root_busy(anchor)?
            } else {
                queries == 0
                    && state
                        .as_ref()
                        .is_none_or(|state| state.original_root.is_none())
                    && !self
                        .namespace
                        .root_identity_busy(&record.root, &record.root_identity)?
            };
            Ok(DetachResult {
                lease_released: released,
                remaining_leases: remaining,
                daemon_leases: total,
                queries,
                work,
                root_handles_released: released_handles,
                root_proof_scope: if record.root_anchor.is_some() {
                    "namespace-os-guard"
                } else {
                    "daemon-owned-handles-only"
                },
            })
        })()
        .map_err(|error: Error| error.committed(CommitState::Committed))
    }

    pub fn drain_released(&self) -> Result<usize> {
        self.drain_slots(self.slots()?)
    }

    pub fn drain_released_page(&self, after: Option<&Id>) -> Result<(usize, Option<Id>)> {
        let (slots, next) = self.slots_page(after)?;
        Ok((self.drain_slots(slots)?, next))
    }

    fn drain_slots(&self, slots: Vec<Arc<ViewSlot>>) -> Result<usize> {
        let _admission = self
            .admission
            .lock()
            .map_err(|_| Error::corrupt("view admission poisoned"))?;
        let mut drained = 0;
        for slot in slots {
            if self
                .namespace
                .read(|connection| lease_count(connection, Some(&slot.id)))?
                == 0
            {
                let mut state = slot
                    .state
                    .lock()
                    .map_err(|_| Error::corrupt("view lock poisoned"))?;
                state.closing = true;
                if let Some(operation) = &state.work {
                    self.namespace.cancel_operation(operation)?;
                }
                drop(state);
                drained += usize::from(slot.drain()?);
                let record = self.recover(slot.id())?;
                let state = slot
                    .state
                    .lock()
                    .map_err(|_| Error::corrupt("view lock poisoned"))?;
                let stopped = state.current.is_none()
                    && state.work.is_none()
                    && state.queries == 0
                    && state.original_root.is_none();
                drop(state);
                if stopped {
                    if let Some(anchor) = &record.root_anchor {
                        if self.namespace.root_busy(anchor)? {
                            continue;
                        }
                        self.namespace.withdraw_root(anchor)?;
                    }
                    self.slots
                        .lock()
                        .map_err(|_| Error::corrupt("view registry poisoned"))?
                        .remove(slot.id());
                }
            }
        }
        Ok(drained)
    }
}

impl ViewSlot {
    pub fn id(&self) -> &Id {
        &self.id
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn input_epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub fn current(&self) -> Result<Arc<PublishedView>> {
        let state = self
            .state
            .lock()
            .map_err(|_| Error::corrupt("view lock poisoned"))?;
        if state.closing {
            return Err(Error::busy("view-closing"));
        }
        state
            .current
            .clone()
            .ok_or_else(|| Error::busy("view-not-ready"))
    }

    pub fn invalidate(&self, changed: &[PathBuf], full: bool) -> Result<u64> {
        self.invalidate_inner(None, changed, full)
    }

    fn invalidate_inner(
        &self,
        expected_version: Option<u64>,
        changed: &[PathBuf],
        full: bool,
    ) -> Result<u64> {
        for path in changed {
            crate::worktrees::relative_path(path)?;
        }
        let state = self
            .state
            .lock()
            .map_err(|_| Error::corrupt("view lock poisoned"))?;
        if let Some(version) = expected_version {
            let current = self
                .namespace
                .read(|connection| view_row(connection, &self.id))?;
            if current.version != version {
                return Err(Error::stale_version(current.version));
            }
        }
        let before = self
            .epoch
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| Error::corrupt("view input epoch exhausted"))?;
        for view in state
            .current
            .iter()
            .map(|current| &current.view)
            .chain(state.preparing.iter())
        {
            if full {
                view.invalidate_all()?;
            } else {
                for path in changed {
                    view.invalidate_path(path)?;
                }
            }
        }
        Ok(before + 1)
    }

    fn install_preparing(&self, view: Arc<WorktreeView>) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::corrupt("view lock poisoned"))?;
        if state.closing {
            return Err(Error::busy("view-closing"));
        }
        if let Some(root) = &state.original_root {
            root.verify_root()?;
        }
        state.preparing = Some(view);
        Ok(())
    }

    fn begin_work<'a>(&'a self, operation: &OperationRecord) -> Result<ViewWork<'a>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::corrupt("view lock poisoned"))?;
        if state.closing || state.work.is_some() {
            return Err(Error::busy("view-work-active"));
        }
        self.namespace.transaction(|transaction| {
            let record: String = transaction.query_row(
                "SELECT record FROM operations WHERE id=?1",
                [operation.id.as_str()],
                |row| row.get(0),
            )?;
            let mut current: OperationRecord = serde_json::from_str(&record)?;
            if current.cancelled {
                return Err(Error::new(
                    ErrorCategory::Cancelled,
                    "operation-cancelled",
                    "work was cancelled before preparation",
                ));
            }
            current.state = OperationState::Preparing;
            Namespace::save_operation(transaction, &current)
        })?;
        state.work = Some(operation.id.clone());
        Ok(ViewWork {
            slot: self,
            operation: operation.id.clone(),
        })
    }

    pub fn query(self: &Arc<Self>, expected_version: u64) -> Result<ViewQuery> {
        let policy = self.namespace.policy()?.policy;
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::corrupt("view lock poisoned"))?;
        if state.closing {
            return Err(Error::busy("view-closing"));
        }
        if state.queries >= policy.work.queue_items {
            return Err(Error::pressure("view-query-limit"));
        }
        let published = state
            .current
            .clone()
            .ok_or_else(|| Error::busy("view-not-ready"))?;
        if published.record.version != expected_version {
            return Err(Error::stale_version(published.record.version));
        }
        let epoch = self.epoch.load(Ordering::Acquire);
        if epoch != published.record.input_epoch {
            return Err(Error::busy("view-input-pending"));
        }
        let status = published.view.status()?;
        if !status.ready || status.published_epoch != published.record.reconciled_epoch {
            return Err(Error::busy("checkpoint-not-current"));
        }
        state.queries += 1;
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(policy.work.operation_timeout_ms))
            .ok_or_else(|| Error::invalid("query deadline overflow"))?;
        Ok(ViewQuery {
            slot: Arc::clone(self),
            published,
            input_epoch: epoch,
            deadline,
        })
    }

    fn drain(&self) -> Result<bool> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::corrupt("view lock poisoned"))?;
        if !state.closing || state.draining || state.queries != 0 || state.work.is_some() {
            return Ok(false);
        }
        self.namespace.transaction(|transaction| {
            if lease_count(transaction, Some(&self.id))? != 0 {
                return Err(Error::busy("view-has-leases"));
            }
            let mut record = view_row(transaction, &self.id)?;
            if record.active {
                record.active = false;
                save_view(transaction, &mut record)?;
                replace_view_references(transaction, &record)?;
            }
            Ok(())
        })?;
        let observer = state.observer.take();
        state.draining = true;
        drop(state);
        // Native watcher destruction can wait for callbacks that invalidate
        // this slot, so it must run without the slot mutex.
        drop(observer);
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::corrupt("view lock poisoned"))?;
        state.preparing = None;
        state.original_root = None;
        state.current = None;
        state.retired.retain(|view| view.strong_count() != 0);
        state.draining = false;
        Ok(true)
    }
}

struct ViewWork<'a> {
    slot: &'a ViewSlot,
    operation: Id,
}

impl Drop for ViewWork<'_> {
    fn drop(&mut self) {
        let result = (|| -> Result<()> {
            let mut state = self
                .slot
                .state
                .lock()
                .map_err(|_| Error::corrupt("view lock poisoned"))?;
            state.work = None;
            state.preparing = None;
            self.slot.namespace.transaction(|transaction| {
                transaction.execute(
                    "DELETE FROM refs WHERE source_kind='migration' AND source_id=?1",
                    [self.operation.as_str()],
                )?;
                Ok(())
            })?;
            drop(state);
            self.slot.drain()?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!(
                "managed view {} needs drain recovery: {error}",
                self.slot.id
            );
        }
    }
}

pub struct ViewQuery {
    slot: Arc<ViewSlot>,
    published: Arc<PublishedView>,
    input_epoch: u64,
    deadline: Instant,
}

impl ViewQuery {
    pub fn published(&self) -> &Arc<PublishedView> {
        &self.published
    }

    pub fn validate(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            return Err(Error::new(
                ErrorCategory::Deadline,
                "query-deadline",
                "query time budget exhausted",
            ));
        }
        if self.slot.epoch.load(Ordering::Acquire) != self.input_epoch {
            return Err(Error::busy("query-input-changed"));
        }
        let status = self.published.view.status()?;
        if !status.ready || status.published_epoch != self.published.record.reconciled_epoch {
            return Err(Error::busy("query-snapshot-changed"));
        }
        Ok(())
    }

    pub fn with_snapshot<T>(&self, read: impl FnOnce(WorktreeSnapshot<'_>) -> T) -> Result<T> {
        self.validate()?;
        let result = self.published.view.with_snapshot(read)?;
        self.validate()?;
        Ok(result)
    }

    pub fn complete(self) -> Result<()> {
        self.validate()?;
        if let Some(generation) = &self.published.record.pin()?.incarnation {
            self.slot
                .namespace
                .transaction(|transaction| super::catalog::touch_object(transaction, generation))?;
        }
        self.validate()
    }
}

impl Drop for ViewQuery {
    fn drop(&mut self) {
        let result = (|| -> Result<()> {
            {
                let mut state = self
                    .slot
                    .state
                    .lock()
                    .map_err(|_| Error::corrupt("view lock poisoned"))?;
                state.queries = state
                    .queries
                    .checked_sub(1)
                    .ok_or_else(|| Error::corrupt("query count underflow"))?;
            }
            self.slot.drain()?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("managed query drain {} failed: {error}", self.slot.id);
        }
    }
}
