// Copyright (c) Microsoft Corporation. All rights reserved.

use super::protocol_v2::{MARKER, Registration, Request, VIEW_MARKER, ViewRegistration};
use super::server::{Options, QuerySource};
use anyhow::{Context, Result, ensure};
use notify::{EventKind, Watcher};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tgrep_core::generations::{GenerationManager, IndexingProfile, Repository};
use tgrep_core::managed::{
    self, AdaptiveRequest, AttachRequest, CatalogCursor, CollectionBounds, CollectionProgress,
    CollectionRequest, Error, ErrorCategory, ExternalWork, Id, LegacyAttachRequest,
    MetadataMutation, MigrationRequest, Namespace, OperationRecord, OperationState, OperationToken,
    OwnerClaim, Policy, ReconcileRequest, RecoveryRequest, RefreshRequest, Token, ViewManager,
    ViewQuery, ViewSlot,
};
use tgrep_core::worktrees::{WorktreeOptions, WorktreeSnapshot};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectId {
    id: Id,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    #[serde(default)]
    cursor: Option<CatalogCursor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryPageRequest {
    #[serde(default)]
    cursor: Option<managed::InventoryCursor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerPage {
    #[serde(default)]
    after: Option<Id>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bootstrap {
    token: Token,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    claim: OwnerClaim,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationInput<T> {
    token: OperationToken,
    request: T,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Acknowledge {
    scope: Id,
    through: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OfflineCollection {
    token: OperationToken,
    request: CollectionRequest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StopRequest {
    token: OperationToken,
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|error| Error::invalid(error.to_string()).into())
}

fn hello(namespace: &Namespace) -> Result<Value> {
    Ok(json!({
        "storage_schema":managed::STORAGE_VERSION,
        "capabilities":[
            "versioned-views","operation-replay","atomic-base-advancement","adaptive-evaluation",
            "instance-owner-guards","catalog-pages","reference-aware-collection",
            "versioned-allocations","bound-checkpoints","offline-maintenance","stop-if-idle",
            "bounded-storage-inventory","namespace-discovery","maintenance-diagnostics"
        ],
        "profile":IndexingProfile::default(),
        "policy":namespace.policy()?,"allocation":namespace.allocation()?,
        "storage":namespace.header().storage,
        "storage_semantics":{
            "managed_reader_protection":namespace.header().storage == managed::policy::StorageMode::Managed,
            "legacy_storage":"always-retain-all; accounting covers observed compatible materializations",
            "memory_accounting":"process-local admitted capacity and mappings, not a resident-set cap"
        },
        "limits":{"request_bytes":managed::MAX_REQUEST_BYTES,"response_bytes":managed::MAX_RESPONSE_BYTES},
        "directory_sync":if cfg!(windows) { "unavailable" } else { "fsync" }
    }))
}

/// Shared online/offline catalog operations. Only the explicit apply path may
/// mutate storage; previews do not retire objects or release references.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StorageContext {
    Live,
    Inspect,
    Maintenance,
}

pub(super) fn storage_request(
    namespace: &Arc<Namespace>,
    method: &str,
    params: Value,
    context: StorageContext,
) -> Result<Value> {
    match method {
        #[cfg(feature = "managed-test-hooks")]
        "testing.install" => Ok(serde_json::to_value(
            namespace.install_test_fault(decode(params)?)?,
        )?),
        #[cfg(feature = "managed-test-hooks")]
        "testing.status" => {
            let _: Empty = decode(params)?;
            Ok(serde_json::to_value(namespace.test_fault_status()?)?)
        }
        #[cfg(feature = "managed-test-hooks")]
        "testing.release" => Ok(serde_json::to_value(
            namespace.release_test_fault(&decode::<ObjectId>(params)?.id)?,
        )?),
        "hello" => {
            let _: Empty = decode(params)?;
            hello(namespace)
        }
        "namespace.status" => {
            let _: Empty = decode(params)?;
            Ok(
                json!({"header":namespace.header(),"instance":namespace.instance(),
                "policy":namespace.policy()?,"allocation":namespace.allocation()?,"usage":namespace.work_usage()?}),
            )
        }
        "maintenance.status" => {
            let _: Empty = decode(params)?;
            Ok(serde_json::to_value(namespace.maintenance_diagnostics()?)?)
        }
        "views.recover" => Ok(serde_json::to_value(
            namespace.authoritative_view(&decode::<ObjectId>(params)?.id)?,
        )?),
        "objects.page" => Ok(serde_json::to_value(
            namespace.page(decode::<Page>(params)?.cursor)?,
        )?),
        "storage.inspect" => Ok(serde_json::to_value(
            namespace.inspect_storage(decode::<Page>(params)?.cursor)?,
        )?),
        "storage.inventory" => Ok(serde_json::to_value(
            namespace.inventory_page(decode::<InventoryPageRequest>(params)?.cursor)?,
        )?),
        "storage.inventory.close" => Ok(
            json!({"closed":namespace.close_inventory(&decode::<managed::InventoryCursor>(params)?)?}),
        ),
        "objects.inspect" => Ok(serde_json::to_value(
            namespace.object(&decode::<ObjectId>(params)?.id)?,
        )?),
        "objects.references" => Ok(serde_json::to_value(
            namespace.reference_counts(&decode::<ObjectId>(params)?.id)?,
        )?),
        "objects.eligibility" => Ok(serde_json::to_value(
            namespace.eligibility(&decode::<ObjectId>(params)?.id)?,
        )?),
        "collections.preview" => Ok(serde_json::to_value(
            namespace.preview_collection(decode::<Page>(params)?.cursor)?,
        )?),
        "owners.inspect" => Ok(serde_json::to_value(
            namespace.inspect_owner(&decode::<ObjectId>(params)?.id)?,
        )?),
        "owners.page" => Ok(serde_json::to_value(
            namespace.owner_page(decode::<OwnerPage>(params)?.after.as_ref())?,
        )?),
        "operations.inspect" => Ok(serde_json::to_value(
            namespace.operation(&decode::<ObjectId>(params)?.id)?,
        )?),
        "operations.lookup" => Ok(serde_json::to_value(
            namespace.operation_for_token(&decode::<OperationToken>(params)?)?,
        )?),
        "operations.pending" => Ok(serde_json::to_value(
            namespace.pending_operations(decode::<OwnerPage>(params)?.after.as_ref())?,
        )?),
        "cursors.close" => {
            Ok(json!({"closed":namespace.close_cursor(&decode::<CatalogCursor>(params)?)?}))
        }
        "owners.prepare"
        | "owners.register"
        | "owners.release"
        | "owners.reap"
        | "operations.cancel"
        | "operations.acknowledge"
        | "metadata.start"
        | "collections.start"
        | "collections.run"
        | "metadata.run"
        | "maintenance.recover" => {
            if context == StorageContext::Inspect {
                return Err(Error::invalid("mutation requires explicit --apply").into());
            }
            if context != StorageContext::Live
                && matches!(method, "metadata.start" | "collections.start")
            {
                return Err(Error::incompatible("asynchronous start requires a live daemon; use the bounded .run method offline").into());
            }
            match method {
                "owners.prepare" => Ok(serde_json::to_value(
                    namespace.prepare_owner_with_token(&decode::<Bootstrap>(params)?.token)?,
                )?),
                "owners.register" => Ok(serde_json::to_value(
                    namespace.register_owner(&decode::<Claim>(params)?.claim)?,
                )?),
                "owners.release" => Ok(serde_json::to_value(
                    namespace.release_owner(&decode::<Claim>(params)?.claim)?,
                )?),
                "owners.reap" => Ok(serde_json::to_value(
                    namespace.reap_owner(&decode::<ObjectId>(params)?.id)?,
                )?),
                "operations.cancel" => Ok(serde_json::to_value(
                    namespace.cancel_operation(&decode::<ObjectId>(params)?.id)?,
                )?),
                "operations.acknowledge" => {
                    let request: Acknowledge = decode(params)?;
                    Ok(
                        json!({"through":namespace.acknowledge_operations(&request.scope, request.through)?}),
                    )
                }
                "metadata.start" => {
                    let request: OperationInput<MetadataMutation> = decode(params)?;
                    namespace.validate_operation_owner(&request.token)?;
                    Ok(serde_json::to_value(namespace.accept_metadata_mutation(
                        request.token,
                        request.request,
                    )?)?)
                }
                "collections.start" => {
                    let request: OperationInput<CollectionRequest> = decode(params)?;
                    namespace.validate_operation_owner(&request.token)?;
                    Ok(serde_json::to_value(namespace.accept_operation(
                        request.token,
                        "collection",
                        serde_json::to_value(request.request)?,
                    )?)?)
                }
                "collections.run" => {
                    let request: OfflineCollection = decode(params)?;
                    let operation = namespace.accept_maintenance_operation(
                        request.token,
                        "collection",
                        serde_json::to_value(&request.request)?,
                    )?;
                    let progress = namespace.collect_pass(&operation.id, &request.request)?;
                    Ok(
                        json!({"operation_id":operation.id,"namespace":namespace.header().namespace,"progress":progress}),
                    )
                }
                "metadata.run" => {
                    let request: OperationInput<MetadataMutation> = decode(params)?;
                    let operation = namespace.accept_maintenance_operation(
                        request.token,
                        "metadata",
                        serde_json::to_value(request.request)?,
                    )?;
                    Ok(serde_json::to_value(
                        namespace.execute_metadata_mutation(&operation.id)?,
                    )?)
                }
                "maintenance.recover" => {
                    let request: OperationInput<RecoveryRequest> = decode(params)?;
                    Ok(serde_json::to_value(
                        namespace.recover_with_token(request.token, request.request)?,
                    )?)
                }
                _ => unreachable!(),
            }
        }
        _ => Err(Error::invalid(format!("unknown managed method: {method}")).into()),
    }
}

struct Monitor {
    watcher: Option<notify::RecommendedWatcher>,
    watched: HashSet<PathBuf>,
    fallback: Option<String>,
    watch_failure: Arc<Mutex<Option<String>>>,
    last_schedule: Instant,
    last_reconcile: Instant,
    pending: HashSet<Id>,
    failures: u32,
}

struct WatchSettings {
    no_watch: bool,
    mode: crate::serve::WatchMode,
    interval: Duration,
    watches_per_view: usize,
}

struct Observer {
    monitor: Arc<Mutex<Monitor>>,
    registry: Weak<Mutex<HashMap<Id, Arc<Mutex<Monitor>>>>>,
    view: Id,
    files: Vec<(PathBuf, managed::FileIdentity)>,
}

impl Drop for Observer {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            match registry.lock() {
                Ok(mut registry) => {
                    registry.remove(&self.view);
                }
                Err(_) => eprintln!("managed observer registry is poisoned; view {}", self.view),
            }
        }
        match self.monitor.lock() {
            Ok(mut monitor) => {
                monitor.watcher = None;
            }
            Err(_) => eprintln!("managed observer lock is poisoned; view {}", self.view),
        }
        for (path, identity) in &self.files {
            if let Err(error) = managed::remove_control_file(path, identity)
                && error.source_io_kind() != Some(std::io::ErrorKind::NotFound)
            {
                eprintln!("managed registration cleanup {}: {error}", path.display());
            }
        }
    }
}

struct State {
    registration: Registration,
    namespace: Arc<Namespace>,
    views: ViewManager,
    watches: WatchSettings,
    monitors: Arc<Mutex<HashMap<Id, Arc<Mutex<Monitor>>>>>,
    jobs: mpsc::SyncSender<Id>,
    queued: Mutex<HashMap<Id, managed::OperationReadGuard>>,
    admission: Mutex<bool>,
    requests: AtomicU64,
    queries: AtomicU64,
    background_batches: AtomicU64,
    stopping: AtomicBool,
    scheduler_diagnostics: Mutex<SchedulerDiagnostics>,
    lifecycle_epoch: AtomicU64,
    scheduler_snapshot: Mutex<Value>,
}

#[derive(Default, serde::Serialize)]
struct SchedulerDiagnostics {
    failed_ticks: u64,
    consecutive_failed_ticks: u64,
    last_failure_elapsed_ms: Option<u128>,
    last_successful_tick_elapsed_ms: Option<u128>,
    #[serde(skip)]
    last_error_detail: Option<String>,
}

impl SchedulerDiagnostics {
    fn observe(&mut self, elapsed: Duration, error: Option<String>) -> bool {
        if let Some(error) = error {
            let changed = self.last_error_detail.as_ref() != Some(&error);
            self.failed_ticks = self.failed_ticks.saturating_add(1);
            self.consecutive_failed_ticks = self.consecutive_failed_ticks.saturating_add(1);
            self.last_failure_elapsed_ms = Some(elapsed.as_millis());
            self.last_error_detail = Some(error);
            changed
        } else {
            self.consecutive_failed_ticks = 0;
            self.last_successful_tick_elapsed_ms = Some(elapsed.as_millis());
            false
        }
    }
}

struct RequestGuard(Arc<State>);
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.0.requests.fetch_sub(1, Ordering::AcqRel);
    }
}

struct QueryGuard<'a>(&'a State);
impl Drop for QueryGuard<'_> {
    fn drop(&mut self) {
        self.0.queries.fetch_sub(1, Ordering::AcqRel);
    }
}

struct BackgroundGuard<'a>(&'a State);
impl Drop for BackgroundGuard<'_> {
    fn drop(&mut self) {
        self.0.background_batches.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Default)]
struct RetrySchedule {
    last: Option<Instant>,
    exponent: u32,
    requested: bool,
}

impl RetrySchedule {
    fn request(&mut self) {
        self.requested = true;
    }

    fn delay(&self, base: Duration) -> Duration {
        base.saturating_mul(1 << if self.requested { 0 } else { self.exponent })
    }

    fn due(&self, now: Instant, base: Duration) -> bool {
        self.last
            .is_none_or(|last| now.saturating_duration_since(last) >= self.delay(base))
    }

    fn completed(&mut self, now: Instant, progress: bool, more: bool) {
        self.last = Some(now);
        self.requested = more;
        self.exponent = if progress || more {
            0
        } else {
            (self.exponent + 1).min(6)
        };
    }
}

struct ManagedQuery {
    query: ViewQuery,
    slot: Arc<ViewSlot>,
}

impl QuerySource for ManagedQuery {
    fn root(&self) -> &Path {
        self.slot.root()
    }
    fn snapshot<T>(&self, read: impl FnOnce(WorktreeSnapshot<'_>) -> T) -> Result<T> {
        Ok(self.query.with_snapshot(read)?)
    }
    fn invalidate(&self, error: &anyhow::Error) -> Result<()> {
        self.slot.invalidate(&[], true)?;
        eprintln!(
            "managed candidate read closed view {}: {error:#}",
            self.slot.id()
        );
        Ok(())
    }
    fn check(&self) -> Result<()> {
        Ok(self.query.validate()?)
    }
}

fn monitor_directories(
    root: &Path,
    storage: &Path,
    budget: usize,
) -> Result<(HashSet<PathBuf>, HashSet<PathBuf>)> {
    let mut directories = HashSet::new();
    let mut stack = vec![(root.to_path_buf(), false)];
    let mut examined = 0_usize;
    let deadline = Instant::now() + Duration::from_millis(100);
    let (git, common) = tgrep_core::git_index::read_repository_dirs_bounded(root)?;
    let metadata: HashSet<_> = [std::fs::canonicalize(git)?, std::fs::canonicalize(common)?]
        .into_iter()
        .collect();
    for directory in &metadata {
        directories.insert(directory.clone());
        for name in ["refs", "info"] {
            let child = directory.join(name);
            match std::fs::symlink_metadata(&child) {
                Ok(entry) if entry.file_type().is_dir() => stack.push((child, true)),
                Ok(_) => anyhow::bail!("non-directory Git watch input; polling"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    while let Some((directory, git_metadata)) = stack.pop() {
        ensure!(
            directories.len() < budget && Instant::now() < deadline,
            "native watch registration budget; polling"
        );
        directories.insert(directory.clone());
        for entry in std::fs::read_dir(directory)? {
            examined += 1;
            ensure!(
                examined <= budget.saturating_mul(128) && Instant::now() < deadline,
                "native watch traversal budget; polling"
            );
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && (git_metadata || entry.file_name() != ".git")
                && entry.path() != root.join(".tgrep")
                && !entry.path().starts_with(storage)
            {
                ensure!(
                    directories.len() + stack.len() < budget,
                    "native watch count budget; polling"
                );
                stack.push((entry.path(), git_metadata));
            }
        }
    }
    Ok((directories, metadata))
}

fn process_memory() -> Value {
    let measured = |value: Option<u64>| match value {
        Some(value) => managed::Measurement::Observed { value },
        None => managed::Measurement::Unavailable {
            reason: "os-memory-counter-unavailable".into(),
        },
    };
    json!({
        "scope":"daemon-process",
        "resident_bytes":measured(crate::mem::process_rss_bytes()),
        "resident_peak_bytes":measured(crate::mem::peak_rss_bytes()),
        "private_bytes":measured(crate::mem::process_private_bytes()),
        "private_high_water_bytes":measured(crate::mem::peak_private_bytes()),
        "private_measurement_kind":if cfg!(windows) { "committed" } else if cfg!(target_os="linux") { "anonymous-resident" } else { "unavailable" },
        "private_high_water_kind":if cfg!(windows) { "os-high-water" } else if cfg!(target_os="linux") { "sampled-lower-bound" } else { "unavailable" }
    })
}

fn watch_event(
    slot: &ViewSlot,
    metadata: &HashSet<PathBuf>,
    watch_failure: &Mutex<Option<String>>,
    event: notify::Result<notify::Event>,
) -> managed::Result<()> {
    match event {
        Ok(event) if matches!(event.kind, EventKind::Access(_)) => Ok(()),
        Ok(event) => {
            let mut paths = Vec::new();
            let mut full = event.need_rescan()
                || event.paths.is_empty()
                || matches!(event.kind, EventKind::Any | EventKind::Other);
            for path in event.paths {
                if let Some(relative) = metadata
                    .iter()
                    .find_map(|directory| path.strip_prefix(directory).ok())
                {
                    let first = relative.components().next().map(|part| part.as_os_str());
                    if !path
                        .extension()
                        .is_some_and(|extension| extension == "lock")
                        && (relative.as_os_str().is_empty()
                            || [
                                "HEAD",
                                "index",
                                "config",
                                "config.worktree",
                                "packed-refs",
                                "commondir",
                                "refs",
                                "info",
                            ]
                            .iter()
                            .any(|name| first == Some(std::ffi::OsStr::new(name))))
                    {
                        full = true;
                    }
                    continue;
                }
                match path.strip_prefix(slot.root()) {
                    Ok(relative) if relative.starts_with(".tgrep") => {}
                    Ok(relative)
                        if relative.components().any(|part| part.as_os_str() == ".git")
                            && (relative != Path::new(".git") || path.is_dir()) => {}
                    Ok(relative) if relative == Path::new(".git") => full = true,
                    Ok(relative) if !relative.as_os_str().is_empty() => {
                        paths.push(relative.to_path_buf())
                    }
                    _ => full = true,
                }
            }
            if full || !paths.is_empty() {
                slot.invalidate(&paths, full).map(|_| ())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            eprintln!("managed watcher {}: {error}", slot.id());
            match watch_failure.lock() {
                Ok(mut failure) => *failure = Some(format!("native watcher failed: {error}")),
                Err(_) => eprintln!("managed watcher status lock is poisoned"),
            }
            slot.invalidate(&[], true).map(|_| ())
        }
    }
}

fn sync_monitor(
    slot: &Arc<ViewSlot>,
    monitor: &Arc<Mutex<Monitor>>,
    settings: &WatchSettings,
    storage: &Path,
) -> Result<()> {
    if settings.no_watch || matches!(settings.mode, crate::serve::WatchMode::Poll) {
        return Ok(());
    }
    let mut monitor = monitor
        .lock()
        .map_err(|_| Error::corrupt("monitor lock poisoned"))?;
    let failed = monitor
        .watch_failure
        .lock()
        .map_err(|_| Error::corrupt("watcher failure lock poisoned"))?
        .take();
    if let Some(failed) = failed {
        monitor.fallback = Some(failed);
    }
    if monitor.fallback.is_some() {
        monitor.watcher = None;
        monitor.watched.clear();
        return Ok(());
    }
    let (desired, metadata) =
        match monitor_directories(slot.root(), storage, settings.watches_per_view) {
            Ok(desired) => desired,
            Err(error) => {
                monitor.fallback = Some(format!("{error:#}"));
                monitor.watcher = None;
                monitor.watched.clear();
                slot.invalidate(&[], true)?;
                return Ok(());
            }
        };
    if monitor.watcher.is_none() {
        let weak = Arc::downgrade(slot);
        let watch_failure = Arc::clone(&monitor.watch_failure);
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Some(slot) = weak.upgrade() else {
                return;
            };
            let result = watch_event(&slot, &metadata, &watch_failure, event);
            if let Err(error) = result {
                eprintln!("managed watcher invalidation {}: {error}", slot.id());
            }
        });
        match watcher {
            Ok(watcher) => monitor.watcher = Some(watcher),
            Err(error) => {
                monitor.fallback = Some(format!("native watcher unavailable: {error}"));
                slot.invalidate(&[], true)?;
                return Ok(());
            }
        }
    }
    let removed: Vec<_> = monitor.watched.difference(&desired).cloned().collect();
    let added: Vec<_> = desired.difference(&monitor.watched).cloned().collect();
    let result = (|| -> notify::Result<()> {
        let watcher = monitor.watcher.as_mut().expect("installed watcher");
        for directory in &removed {
            watcher.unwatch(directory)?;
        }
        for directory in &added {
            watcher.watch(directory, notify::RecursiveMode::NonRecursive)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => monitor.watched = desired,
        Err(error) => {
            monitor.fallback = Some(format!("native registration failed: {error}"));
            monitor.watcher = None;
            monitor.watched.clear();
            slot.invalidate(&[], true)?;
        }
    }
    Ok(())
}

impl State {
    fn legacy_registration(&self) -> super::protocol::Registration {
        super::protocol::Registration {
            protocol: 1,
            instance: self.registration.instance.to_string(),
            repository: self.registration.repository.clone(),
            pid: self.registration.pid,
            port: self.registration.port,
            storage: self.registration.storage.clone(),
        }
    }

    fn legacy_describe(&self, view: &Id) -> Result<Value> {
        let status = self.views.status(view)?;
        let record = &status.authoritative;
        let key = record
            .current
            .as_ref()
            .map(|pin| &pin.key)
            .or_else(|| record.pending.as_ref().map(|pin| &pin.key))
            .ok_or_else(|| Error::corrupt("view has neither a pin nor an intent"))?;
        Ok(
            json!({"root":record.root.to_path()?,"view":record.id,"generation":key,"version":record.version,
            "ready":status.ready,"epoch":status.input_epoch,"leases":status.leases,
            "reconcile_running":status.work.is_some(),"last_error":status.error,
            "last_reconcile":status.last_reconcile,"backend":"shared-v2"}),
        )
    }

    fn wait_legacy_operation(&self, operation: &OperationRecord) -> Result<OperationRecord> {
        let started = Instant::now();
        let timeout =
            Duration::from_millis(self.namespace.policy()?.policy.work.operation_timeout_ms);
        loop {
            let current = self.namespace.operation(&operation.id)?;
            match current.state {
                OperationState::Completed => return Ok(current),
                OperationState::Cancelled | OperationState::Failed => {
                    let error = current
                        .error
                        .ok_or_else(|| Error::corrupt("terminal operation has no diagnostic"))?;
                    return Err(serde_json::from_value::<Error>(error)?.into());
                }
                _ => {}
            }
            if started.elapsed() >= timeout {
                return Err(Error::new(
                    ErrorCategory::Deadline,
                    "legacy-response-deadline",
                    "inspect the retained attachment before retrying",
                )
                .operation(operation.id.to_string())
                .committed(managed::CommitState::Unknown)
                .into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn legacy(&self, request: &Request) -> Result<Value> {
        let result = self.legacy_inner(request);
        if result.is_ok() && matches!(request.method.as_str(), "attach" | "detach" | "refresh") {
            self.lifecycle_epoch.fetch_add(1, Ordering::Release);
        }
        result
    }

    fn legacy_inner(&self, request: &Request) -> Result<Value> {
        if self.namespace.header().storage != managed::policy::StorageMode::CompatibilityRetainAll {
            return Err(Error::incompatible("legacy clients cannot use managed storage").into());
        }
        match request.method.as_str() {
            "hello" => {
                let _: Empty = decode(request.params.clone())?;
                Ok(
                    json!({"capabilities":["leases","recoverable-attach","worktree-overlays","refresh","search","files"],
                    "profile":IndexingProfile::default(),"retention":"retain-all"}),
                )
            }
            "attach" => {
                let input: LegacyAttachRequest = decode(request.params.clone())?;
                let operation = self.views.accept_legacy_attach(input.clone())?;
                let _receipt = self.namespace.hold_operation(&operation.id)?;
                self.enqueue(&operation)?;
                self.wait_legacy_operation(&operation)?;
                let lease = self.views.legacy_lease(&input.lease)?;
                self.legacy_describe(&lease.view)
            }
            "lookup" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Lookup {
                    root: PathBuf,
                }
                let input: Lookup = decode(request.params.clone())?;
                let view = self
                    .views
                    .lookup(&input.root)?
                    .ok_or_else(|| Error::busy("worktree-not-attached"))?;
                self.legacy_describe(&view.id)
            }
            "detach" | "refresh" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Lease {
                    root: PathBuf,
                    view: Id,
                    lease: Token,
                    #[serde(default)]
                    changed: Vec<PathBuf>,
                    #[serde(default)]
                    full: bool,
                }
                let input: Lease = decode(request.params.clone())?;
                let lease = self.views.legacy_lease(&input.lease)?;
                if lease.view != input.view
                    || lease.original.root.to_path()? != std::fs::canonicalize(input.root)?
                {
                    return Err(Error::new(
                        ErrorCategory::StaleIdentity,
                        "legacy-lease-mismatch",
                        "lease belongs to another view or root",
                    )
                    .into());
                }
                if request.method == "detach" {
                    if !input.changed.is_empty() || input.full {
                        return Err(Error::invalid("detach cannot refresh paths").into());
                    }
                    let detached = self.views.detach_legacy_lease(&input.lease)?;
                    return Ok(
                        json!({"view":input.view,"detached":true,"remaining_leases":detached.remaining_leases,
                        "root_handles_released":detached.root_handles_released}),
                    );
                }
                if lease.released {
                    return Err(Error::new(
                        ErrorCategory::StaleIdentity,
                        "lease-released",
                        "legacy lease has been released",
                    )
                    .into());
                }
                let slot = self.views.slot(&input.view)?;
                let epoch = slot.invalidate(&input.changed, input.full)?;
                let operation = self.views.accept_reconciliation(
                    Token::parse(Id::new()?.to_string())?,
                    ReconcileRequest {
                        view: input.view.clone(),
                        expected_version: self.views.recover(&input.view)?.version,
                        allocation_version: self.namespace.allocation()?.version,
                    },
                )?;
                let _receipt = self.namespace.hold_operation(&operation.id)?;
                self.enqueue(&operation)?;
                self.wait_legacy_operation(&operation)?;
                let mut result = self.legacy_describe(&input.view)?;
                result["processed_epoch"] = json!(epoch);
                Ok(result)
            }
            "status" | "search" | "files" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Query {
                    root: PathBuf,
                    view: Id,
                    query: Value,
                }
                let input: Query = decode(request.params.clone())?;
                let record = self.views.recover(&input.view)?;
                if record.root.to_path()? != std::fs::canonicalize(&input.root)? {
                    return Err(Error::invalid("legacy query root differs from view").into());
                }
                if request.method == "status" {
                    let _: Empty = decode(input.query)?;
                    return self.legacy_describe(&input.view);
                }
                self.query(&Request {
                    jsonrpc: "2.0".into(), protocol: managed::PROTOCOL_VERSION,
                    namespace: request.namespace.clone(), instance: request.instance.clone(),
                    repository: request.repository.clone(), id: request.id.clone(), method: request.method.clone(),
                    params: json!({"root":input.root,"view":input.view,"expected_version":record.version,"query":input.query}),
                })
            }
            _ => Err(Error::invalid("unknown legacy management method").into()),
        }
    }

    fn enqueue(&self, operation: &OperationRecord) -> Result<()> {
        if matches!(operation.kind.as_str(), "idle-stop" | "recovery") {
            return Ok(());
        }
        if !matches!(
            operation.state,
            OperationState::Accepted | OperationState::Preparing | OperationState::Cancelling
        ) {
            return Ok(());
        }
        if operation.instance != *self.namespace.instance() {
            return Err(Error::new(ErrorCategory::RecoveryRequired, "previous-instance-operation",
                "the previous instance's interrupted work must recover before its receipt can be replayed")
                .operation(operation.id.to_string()).committed(operation.committed_state).into());
        }
        if let Some(view) = operation
            .request
            .get("view")
            .and_then(Value::as_str)
            .map(Id::parse)
            .transpose()?
            && let Some(monitor) = self
                .monitors
                .lock()
                .map_err(|_| Error::corrupt("monitor registry poisoned"))?
                .get(&view)
                .cloned()
        {
            monitor
                .lock()
                .map_err(|_| Error::corrupt("monitor poisoned"))?
                .pending
                .insert(operation.id.clone());
        }
        let mut queued = self
            .queued
            .lock()
            .map_err(|_| Error::corrupt("job registry poisoned"))?;
        if queued.contains_key(&operation.id) {
            return Ok(());
        }
        queued.insert(
            operation.id.clone(),
            self.namespace.hold_operation(&operation.id)?,
        );
        match self.jobs.try_send(operation.id.clone()) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => {
                queued.remove(&operation.id);
                // Acceptance is durable; the bounded scheduler will retry it.
                Ok(())
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                queued.remove(&operation.id);
                Err(Error::new(
                    ErrorCategory::RecoveryRequired,
                    "workers-unavailable",
                    "accepted work requires restart recovery",
                )
                .operation(operation.id.to_string())
                .into())
            }
        }
    }

    fn lifecycle(&self, request: &Request) -> Result<Value> {
        let result = self.lifecycle_inner(request);
        if result.is_ok()
            && matches!(
                request.method.as_str(),
                "views.attach"
                    | "views.advance"
                    | "views.refresh"
                    | "views.adaptive"
                    | "views.detach"
                    | "owners.register"
                    | "owners.release"
                    | "owners.reap"
                    | "operations.cancel"
                    | "metadata.start"
            )
        {
            self.lifecycle_epoch.fetch_add(1, Ordering::Release);
        }
        result
    }

    fn lifecycle_inner(&self, request: &Request) -> Result<Value> {
        match request.method.as_str() {
            #[cfg(feature = "managed-test-hooks")]
            "testing.watch-event" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "kebab-case")]
                enum Kind {
                    Rescan,
                    Failure,
                }
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Input {
                    view: Id,
                    kind: Kind,
                }
                let input: Input = decode(request.params.clone())?;
                let slot = self.views.slot(&input.view)?;
                let monitor = self
                    .monitors
                    .lock()
                    .map_err(|_| Error::corrupt("monitor registry poisoned"))?
                    .get(&input.view)
                    .cloned()
                    .ok_or_else(|| Error::invalid("view has no monitor"))?;
                let failure = Arc::clone(
                    &monitor
                        .lock()
                        .map_err(|_| Error::corrupt("monitor poisoned"))?
                        .watch_failure,
                );
                let event = match input.kind {
                    Kind::Rescan => Ok(notify::Event::new(EventKind::Modify(
                        notify::event::ModifyKind::Any,
                    ))
                    .add_path(slot.root().join("notes.txt"))
                    .set_flag(notify::event::Flag::Rescan)),
                    Kind::Failure => Err(notify::Error::generic(
                        "injected native watcher uncertainty",
                    )),
                };
                watch_event(&slot, &HashSet::new(), &failure, event)?;
                Ok(json!({"input_epoch":slot.input_epoch()}))
            }
            "views.attach" => {
                let input: OperationInput<AttachRequest> = decode(request.params.clone())?;
                let operation = self.views.accept_attach(input.token, input.request)?;
                self.enqueue(&operation)?;
                Ok(serde_json::to_value(operation)?)
            }
            "views.advance" => {
                let input: OperationInput<MigrationRequest> = decode(request.params.clone())?;
                let operation = self.views.accept_migration(input.token, input.request)?;
                self.enqueue(&operation)?;
                Ok(serde_json::to_value(operation)?)
            }
            "views.refresh" => {
                let input: OperationInput<RefreshRequest> = decode(request.params.clone())?;
                let operation = self.views.accept_refresh(input.token, input.request)?;
                self.enqueue(&operation)?;
                Ok(serde_json::to_value(operation)?)
            }
            "views.adaptive" => {
                let input: OperationInput<AdaptiveRequest> = decode(request.params.clone())?;
                let operation = self.views.accept_adaptive(input.token, input.request)?;
                self.enqueue(&operation)?;
                Ok(serde_json::to_value(operation)?)
            }
            "views.recover" => Ok(serde_json::to_value(
                self.views
                    .recover(&decode::<ObjectId>(request.params.clone())?.id)?,
            )?),
            "views.status" => Ok(serde_json::to_value(
                self.views
                    .status(&decode::<ObjectId>(request.params.clone())?.id)?,
            )?),
            "views.detach" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Detach {
                    owner: Id,
                    lease: Token,
                }
                let input: Detach = decode(request.params.clone())?;
                Ok(serde_json::to_value(
                    self.views.detach_lease(&input.owner, &input.lease)?,
                )?)
            }
            "views.invalidate" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Invalidate {
                    view: Id,
                    expected_version: u64,
                    owner: Id,
                    changed: Vec<PathBuf>,
                    full: bool,
                }
                let input: Invalidate = decode(request.params.clone())?;
                Ok(
                    json!({"input_epoch":self.views.invalidate(&input.view, &input.owner,
                    input.expected_version, &input.changed, input.full)?}),
                )
            }
            "lookup" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Lookup {
                    root: PathBuf,
                }
                let input: Lookup = decode(request.params.clone())?;
                let view = self
                    .views
                    .lookup(&input.root)?
                    .ok_or_else(|| Error::busy("worktree-not-attached"))?;
                let intent = view.intent()?;
                let incarnation = view
                    .current
                    .as_ref()
                    .and_then(|pin| pin.incarnation.as_ref());
                Ok(
                    json!({"root":input.root,"view":view.id,"generation":intent.key,"version":view.version,
                    "incarnation":incarnation,"commit":intent.commit,"ready":self.views.status(&view.id)?.ready}),
                )
            }
            "stop-if-idle" => {
                let input: StopRequest = decode(request.params.clone())?;
                let mut admission = self
                    .admission
                    .lock()
                    .map_err(|_| Error::corrupt("admission gate poisoned"))?;
                *admission = false;
                let result = (|| {
                    self.views.drain_released()?;
                    self.namespace.stop_if_idle_with_token(
                        input.token,
                        ExternalWork {
                            queries: self.queries.load(Ordering::Acquire),
                            queued_jobs: self
                                .queued
                                .lock()
                                .map_err(|_| Error::corrupt("job registry poisoned"))?
                                .len() as u64,
                            requests: self.requests.load(Ordering::Acquire).saturating_sub(1),
                            background_batches: self.background_batches.load(Ordering::Acquire),
                        },
                    )
                })();
                match result {
                    Ok(outcome) => {
                        if outcome.stopping && outcome.instance == self.registration.instance {
                            self.stopping.store(true, Ordering::Release);
                        } else {
                            *admission = true;
                        }
                        Ok(serde_json::to_value(outcome)?)
                    }
                    Err(error) => {
                        if error.committed_state == managed::CommitState::NotCommitted {
                            *admission = true;
                        } else {
                            self.stopping.store(true, Ordering::Release);
                        }
                        Err(error.into())
                    }
                }
            }
            method => {
                if matches!(method, "collections.run" | "metadata.run") {
                    return Err(Error::invalid(
                        "use versioned owner-scoped start methods on a live daemon",
                    )
                    .into());
                }
                let mut result = storage_request(
                    &self.namespace,
                    method,
                    request.params.clone(),
                    StorageContext::Live,
                )?;
                if matches!(method, "metadata.start" | "collections.start") {
                    self.enqueue(&serde_json::from_value(result.clone())?)?;
                }
                if matches!(method, "owners.release" | "owners.reap") {
                    self.views
                        .drain_released()
                        .map_err(|error| error.committed(managed::CommitState::Committed))?;
                }
                if matches!(method, "namespace.status" | "maintenance.status") {
                    result["process_memory"] = process_memory();
                    result["scheduler"] = self
                        .scheduler_snapshot
                        .lock()
                        .map_err(|_| Error::corrupt("scheduler observations poisoned"))?
                        .clone();
                    let diagnostics = self
                        .scheduler_diagnostics
                        .lock()
                        .map_err(|_| Error::corrupt("scheduler diagnostics poisoned"))?;
                    result["scheduler"]["diagnostics"] = serde_json::to_value(&*diagnostics)?;
                    result["scheduler_error_detail"] = json!(diagnostics.last_error_detail);
                }
                Ok(result)
            }
        }
    }

    fn query(&self, request: &Request) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Query {
            root: PathBuf,
            view: Id,
            expected_version: u64,
            query: Value,
        }
        let input: Query = decode(request.params.clone())?;
        let slot = self.views.slot(&input.view)?;
        if std::fs::canonicalize(input.root)? != slot.root() {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "query-root-mismatch",
                "query root differs from its view",
            )
            .into());
        }
        if request.method == "status" {
            let status = self.views.status(slot.id())?;
            if status.authoritative.version != input.expected_version {
                return Err(Error::stale_version(status.authoritative.version).into());
            }
            let pin = status.authoritative.pin()?;
            let monitor = self
                .monitors
                .lock()
                .map_err(|_| Error::corrupt("monitor registry poisoned"))?
                .get(slot.id())
                .cloned();
            let monitor = monitor
                .as_ref()
                .map(|monitor| {
                    monitor
                        .lock()
                        .map_err(|_| Error::corrupt("monitor poisoned"))
                })
                .transpose()?;
            return Ok(json!({
                "root":slot.root(),"view":slot.id(),"generation":pin.key,"version":status.authoritative.version,
                "ready":status.ready,"epoch":status.input_epoch,"backend":"shared-v2","status":status,
                "watch_mode":if self.watches.no_watch { "disabled" } else if monitor.as_ref().is_some_and(|monitor| monitor.watcher.is_some()) { "native" } else { "poll" },
                "watch_fallback":monitor.as_ref().and_then(|monitor| monitor.fallback.as_ref()),
                "policy":self.namespace.policy()?,"usage":self.namespace.work_usage()?,
                "adaptive":self.views.last_adaptive(slot.id())?,
                "adaptive_error":self.views.last_adaptive_error(slot.id())?,
                "scheduler_error":self.scheduler_diagnostics.lock().map_err(|_| Error::corrupt("scheduler diagnostics poisoned"))?.last_error_detail
            }));
        }
        self.queries.fetch_add(1, Ordering::AcqRel);
        let _active = QueryGuard(self);
        let query = slot.query(input.expected_version)?;
        let published = &query.published().record;
        let pin = published.pin()?;
        let metadata = json!({"root":slot.root(),"view":slot.id(),"generation":pin.key,"version":published.version,
            "incarnation":pin.incarnation,"commit":pin.commit,"ready":true,"hidden_complete":true,"backend":"shared-v2"});
        let source = ManagedQuery { query, slot };
        let result = super::server::search_snapshot(
            &source,
            &input.query,
            request.method == "files",
            &request.id,
            metadata,
        )?;
        source.query.complete()?;
        Ok(result)
    }

    fn job(&self, id: &Id) -> Result<()> {
        let _receipt = self.namespace.hold_operation(id)?;
        let operation = self.namespace.operation(id)?;
        let result = match operation.kind.as_str() {
            "metadata" => self.namespace.execute_metadata_mutation(id).map(|_| ()),
            "collection" => {
                let request: CollectionRequest = serde_json::from_value(operation.request.clone())?;
                self.namespace.collect_pass(id, &request).map(|_| ())
            }
            _ => self.views.execute(id).map(|_| ()),
        };
        if let Err(error) = result {
            self.namespace.fail_operation(id, error)?;
        }
        let final_operation = self.namespace.operation(id)?;
        if let Some(view) = operation
            .request
            .get("view")
            .or_else(|| final_operation.progress.get("view"))
            .and_then(Value::as_str)
            .map(Id::parse)
            .transpose()?
        {
            let monitor = self
                .monitors
                .lock()
                .map_err(|_| Error::corrupt("monitor registry poisoned"))?
                .get(&view)
                .cloned();
            if let Some(monitor) = monitor {
                {
                    let mut state = monitor
                        .lock()
                        .map_err(|_| Error::corrupt("monitor poisoned"))?;
                    state.last_schedule = Instant::now();
                    if matches!(
                        final_operation.state,
                        OperationState::Completed
                            | OperationState::Failed
                            | OperationState::Cancelled
                    ) {
                        state.pending.remove(id);
                    }
                    if final_operation.state == OperationState::Completed
                        && matches!(
                            operation.kind.as_str(),
                            "attach" | "legacy-attach" | "migrate" | "refresh" | "reconcile"
                        )
                    {
                        state.last_reconcile = Instant::now();
                    }
                    state.failures = if final_operation.state == OperationState::Completed {
                        0
                    } else {
                        state.failures.saturating_add(1)
                    };
                }
                if self.views.recover(&view)?.active {
                    sync_monitor(
                        &self.views.slot(&view)?,
                        &monitor,
                        &self.watches,
                        self.namespace.path(),
                    )?;
                }
            }
        }
        self.views.drain_released()?;
        if operation.kind != "collection"
            && matches!(
                final_operation.state,
                OperationState::Completed | OperationState::Failed | OperationState::Cancelled
            )
        {
            self.lifecycle_epoch.fetch_add(1, Ordering::Release);
        }
        Ok(())
    }

    fn schedule_view(&self, slot: &Arc<ViewSlot>) -> Result<()> {
        let status = self.views.status(slot.id())?;
        if status.leases == 0 {
            return Ok(());
        }
        if status.closing || status.work.is_some() || !status.authoritative.committed {
            return Ok(());
        }
        let monitor = self
            .monitors
            .lock()
            .map_err(|_| Error::corrupt("monitor registry poisoned"))?
            .get(slot.id())
            .cloned();
        let Some(monitor) = monitor else {
            return Ok(());
        };
        let mut monitor = monitor
            .lock()
            .map_err(|_| Error::corrupt("monitor lock poisoned"))?;
        let failed = monitor
            .watch_failure
            .lock()
            .map_err(|_| Error::corrupt("watcher failure lock poisoned"))?
            .take();
        if let Some(failed) = failed {
            monitor.fallback = Some(failed);
            monitor.watcher = None;
            monitor.watched.clear();
        }
        let pending: Vec<_> = monitor
            .pending
            .iter()
            .take(self.namespace.policy()?.policy.work.page_objects as usize)
            .cloned()
            .collect();
        for id in pending {
            match self.namespace.operation(&id) {
                Ok(operation)
                    if matches!(
                        operation.state,
                        OperationState::Completed
                            | OperationState::Failed
                            | OperationState::Cancelled
                    ) =>
                {
                    monitor.pending.remove(&id);
                }
                Err(error) if error.category == ErrorCategory::ReceiptExpired => {
                    monitor.pending.remove(&id);
                }
                Err(error) => return Err(error.into()),
                _ => {}
            }
        }
        if !monitor.pending.is_empty() {
            return Ok(());
        }
        let retry = Duration::from_millis(200_u64.saturating_mul(1_u64 << monitor.failures.min(7)));
        let elapsed = monitor.last_schedule.elapsed();
        let periodic =
            !self.watches.no_watch && monitor.last_reconcile.elapsed() >= self.watches.interval;
        if (!status.ready || periodic) && elapsed >= retry {
            monitor.last_schedule = Instant::now();
            drop(monitor);
            if periodic {
                slot.invalidate(&[], true)?;
            }
            let operation = self.views.accept_reconciliation(
                Token::parse(Id::new()?.to_string())?,
                ReconcileRequest {
                    view: slot.id().clone(),
                    expected_version: status.authoritative.version,
                    allocation_version: self.namespace.allocation()?.version,
                },
            )?;
            self.enqueue(&operation)?;
        } else if status.ready
            && elapsed >= retry
            && matches!(
                self.namespace.policy()?.policy.advancement,
                managed::policy::Advancement::Adaptive { .. }
            )
        {
            monitor.last_schedule = Instant::now();
            drop(monitor);
            if let Some(owner) = self.views.active_owner(slot.id())?
                && self.views.automatic_advancement_due(slot.id(), &owner)?
            {
                let operation = self.views.accept_automatic_advancement(
                    Token::parse(Id::new()?.to_string())?,
                    AdaptiveRequest {
                        view: slot.id().clone(),
                        expected_version: status.authoritative.version,
                        allocation_version: self.namespace.allocation()?.version,
                        owner,
                    },
                )?;
                self.enqueue(&operation)?;
            }
        }
        Ok(())
    }
}

fn failure(error: anyhow::Error) -> Value {
    let failure = match error.downcast::<Error>() {
        Ok(error) => error,
        Err(error) => match error.downcast::<std::io::Error>() {
            Ok(error) => Error::io(error),
            Err(error) => match error.downcast::<tgrep_core::Error>() {
                Ok(error) => Error::from(error),
                Err(error) => Error::invalid(format!("{error:#}")),
            },
        },
    };
    json!({"code":-32002,"message":failure.detail,"data":failure})
}

fn respond(mut stream: TcpStream, request: Option<&Request>, state: &State, result: Result<Value>) {
    let id = request.map_or(Value::Null, |request| request.id.clone());
    let response = match result {
        Ok(mut data) if request.is_some_and(|request| request.protocol == 1) => {
            data["protocol"] = json!(1);
            data["instance"] = json!(state.registration.instance);
            data["repository"] = json!(state.registration.repository);
            json!({"jsonrpc":"2.0","id":id,"result":data})
        }
        Ok(data) => json!({"jsonrpc":"2.0","id":id,"result":{
            "protocol":managed::PROTOCOL_VERSION,"namespace":state.registration.namespace,
            "instance":state.registration.instance,"repository":state.registration.repository,"data":data}}),
        Err(error) => json!({"jsonrpc":"2.0","id":id,"error":failure(error)}),
    };
    let result = (|| -> Result<()> {
        let mut bytes = serde_json::to_vec(&response)?;
        if bytes.len() > managed::MAX_RESPONSE_BYTES {
            bytes = serde_json::to_vec(
                &json!({"jsonrpc":"2.0","id":id,"error":failure(Error::pressure("response-byte-limit").into())}),
            )?;
        }
        stream.write_all(&bytes)?;
        stream.write_all(b"\n")?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("managed response not delivered: {error:#}");
    }
}

struct DaemonRegistration {
    path: PathBuf,
    identity: managed::FileIdentity,
    sentinel: (PathBuf, managed::FileIdentity),
    _git_lock: File,
}

impl Drop for DaemonRegistration {
    fn drop(&mut self) {
        for (path, identity) in [
            (&self.path, &self.identity),
            (&self.sentinel.0, &self.sentinel.1),
        ] {
            if let Err(error) = managed::remove_control_file(path, identity)
                && error.source_io_kind() != Some(std::io::ErrorKind::NotFound)
            {
                eprintln!(
                    "managed daemon registration cleanup {}: {error}",
                    path.display()
                );
            }
        }
    }
}

pub(super) fn run(root: &Path, options: Options<'_>, policy: Policy) -> Result<()> {
    policy.validate()?;
    ensure!(
        options.hint_budget >= policy.work.max_views as usize,
        "watcher queue must cover every managed view"
    );
    let repository = Repository::discover(root)?;
    let namespace = Namespace::initialize(&repository, options.storage, policy.clone())?;
    let git_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(repository.common_dir().join("tgrep-daemon-v1.lock"))?;
    fs2::FileExt::try_lock_exclusive(&git_lock)
        .context("another shared daemon owns the repository")?;
    namespace.activate()?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let registration = Registration {
        protocol: managed::PROTOCOL_VERSION,
        namespace: namespace.header().namespace.clone(),
        instance: namespace.instance().clone(),
        repository: repository.identity().into(),
        pid: std::process::id(),
        port: listener.local_addr()?.port(),
        storage: namespace.path().into(),
    };
    let monitors = Arc::new(Mutex::new(HashMap::new()));
    let observer_registration = registration.clone();
    let observer_registry = Arc::clone(&monitors);
    let weak_registry = Arc::downgrade(&monitors);
    let observer_storage = namespace.path().to_path_buf();
    let observer_namespace = Arc::clone(&namespace);
    let watches = WatchSettings {
        no_watch: options.no_watch,
        mode: options.watch_mode,
        interval: options.poll_interval,
        watches_per_view: options.watch_budget / policy.work.max_views as usize,
    };
    let observer_watches = WatchSettings {
        no_watch: watches.no_watch,
        mode: watches.mode,
        interval: watches.interval,
        watches_per_view: watches.watches_per_view,
    };
    let observer: managed::ObserveView = Arc::new(move |slot| {
        let git = tgrep_core::git_index::read_repository_dirs(slot.root())
            .map_err(Error::io)?
            .0;
        let marker = git.join(VIEW_MARKER);
        let marker_identity = managed::publish_control_file(
            &marker,
            &json!(ViewRegistration {
                daemon: observer_registration.clone(),
                root: slot.root().into(),
                view: slot.id().clone(),
            }),
        )?;
        let sentinel = git.join(super::protocol::VIEW_MARKER);
        let sentinel_value = if observer_namespace.header().storage
            == managed::policy::StorageMode::CompatibilityRetainAll
        {
            let view = observer_namespace.authoritative_view(slot.id())?;
            let key = view
                .current
                .as_ref()
                .map(|pin| &pin.key)
                .or_else(|| view.pending.as_ref().map(|pin| &pin.key))
                .ok_or_else(|| Error::corrupt("view intent is absent"))?;
            json!(super::protocol::ViewRegistration {
                daemon: super::protocol::Registration {
                    protocol: 1,
                    instance: observer_registration.instance.to_string(),
                    repository: observer_registration.repository.clone(),
                    pid: observer_registration.pid,
                    port: observer_registration.port,
                    storage: observer_registration.storage.clone(),
                },
                root: slot.root().into(),
                view: slot.id().to_string(),
                generation: key.clone(),
            })
        } else {
            json!({"protocol":2,"namespace":observer_registration.namespace,"instance":observer_registration.instance,
                "managed":true,"view":slot.id()})
        };
        let sentinel_identity = managed::publish_control_file(&sentinel, &sentinel_value)?;
        let monitor = Arc::new(Mutex::new(Monitor {
            watcher: None,
            watched: HashSet::new(),
            fallback: None,
            last_schedule: Instant::now(),
            watch_failure: Arc::new(Mutex::new(None)),
            last_reconcile: Instant::now(),
            pending: HashSet::new(),
            failures: 0,
        }));
        sync_monitor(slot, &monitor, &observer_watches, &observer_storage).map_err(|error| {
            Error::new(
                ErrorCategory::Io,
                "watch-installation-failed",
                format!("{error:#}"),
            )
        })?;
        observer_registry
            .lock()
            .map_err(|_| Error::corrupt("monitor registry poisoned"))?
            .insert(slot.id().clone(), Arc::clone(&monitor));
        Ok(Box::new(Observer {
            monitor,
            registry: weak_registry.clone(),
            view: slot.id().clone(),
            files: vec![(marker, marker_identity), (sentinel, sentinel_identity)],
        }))
    });
    let legacy = if policy.storage == managed::policy::StorageMode::CompatibilityRetainAll {
        let bases = options.storage.join("bases");
        match std::fs::create_dir(&bases) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }

        Some(Arc::new(GenerationManager::with_storage(
            repository.clone(),
            &bases,
        )?))
    } else {
        None
    };
    let views = ViewManager::new(
        Arc::clone(&namespace),
        legacy,
        WorktreeOptions {
            hint_capacity: options.hint_budget / policy.work.max_views as usize,
            ..WorktreeOptions::default()
        },
        Some(observer),
    )?;
    let (jobs, jobs_rx) = mpsc::sync_channel(policy.work.queue_items as usize);
    let state = Arc::new(State {
        registration: registration.clone(),
        namespace,
        views,
        watches,
        monitors,
        jobs,
        queued: Mutex::new(HashMap::new()),
        admission: Mutex::new(true),
        requests: AtomicU64::new(0),
        queries: AtomicU64::new(0),
        background_batches: AtomicU64::new(0),
        stopping: AtomicBool::new(false),
        scheduler_diagnostics: Mutex::new(SchedulerDiagnostics::default()),
        lifecycle_epoch: AtomicU64::new(1),
        scheduler_snapshot: Mutex::new(json!({"initializing":true})),
    });
    let marker = repository.common_dir().join(MARKER);
    let marker_identity = managed::publish_control_file(&marker, &json!(registration))?;
    let sentinel = repository.common_dir().join(super::MARKER);
    let sentinel_value = if policy.storage == managed::policy::StorageMode::CompatibilityRetainAll {
        json!(state.legacy_registration())
    } else {
        json!({"protocol":2,"namespace":registration.namespace,"instance":registration.instance})
    };
    let sentinel_identity = managed::publish_control_file(&sentinel, &sentinel_value)?;
    let _registration = DaemonRegistration {
        path: marker,
        identity: marker_identity,
        sentinel: (sentinel, sentinel_identity),
        _git_lock: git_lock,
    };
    eprintln!(
        "shared-v2 listening on {} (namespace {})",
        registration.port, registration.namespace
    );
    thread::scope(|scope| -> Result<()> {
        struct Shutdown<'a>(&'a AtomicBool);
        impl Drop for Shutdown<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let _shutdown = Shutdown(&state.stopping);
        let jobs_rx = Arc::new(Mutex::new(jobs_rx));
        for _ in 0..policy.work.workers {
            let receiver = Arc::clone(&jobs_rx);
            let state = Arc::clone(&state);
            scope.spawn(move || {
                while !state.stopping.load(Ordering::Acquire) {
                    let job = receiver
                        .lock()
                        .expect("managed job receiver")
                        .recv_timeout(Duration::from_millis(50));
                    let id = match job {
                        Ok(id) => id,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    if let Err(error) = state.job(&id) {
                        eprintln!("managed operation {id}: {error:#}");
                    }
                    state
                        .queued
                        .lock()
                        .expect("managed job registry")
                        .remove(&id);
                }
            });
        }
        let (connections, connections_rx) =
            mpsc::sync_channel::<(TcpStream, RequestGuard)>(policy.work.queue_items as usize);
        let connections_rx = Arc::new(Mutex::new(connections_rx));
        let (queries, queries_rx) = mpsc::sync_channel::<(TcpStream, Request, RequestGuard)>(
            policy.work.queue_items as usize,
        );
        let queries_rx = Arc::new(Mutex::new(queries_rx));
        let (legacy_jobs, legacy_rx) = mpsc::sync_channel::<(TcpStream, Request, RequestGuard)>(
            policy.work.queue_items as usize,
        );
        let legacy_state = Arc::clone(&state);
        scope.spawn(move || {
            while !legacy_state.stopping.load(Ordering::Acquire) {
                match legacy_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok((stream, request, _active)) => respond(
                        stream,
                        Some(&request),
                        &legacy_state,
                        legacy_state.legacy(&request),
                    ),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        for _ in 0..2 {
            let receiver = Arc::clone(&queries_rx);
            let query_state = Arc::clone(&state);
            scope.spawn(move || {
                while !query_state.stopping.load(Ordering::Acquire) {
                    let job = receiver
                        .lock()
                        .expect("managed query receiver")
                        .recv_timeout(Duration::from_millis(50));
                    match job {
                        Ok((stream, request, _active)) => {
                            let result = if request.protocol == 1 {
                                query_state.legacy(&request)
                            } else {
                                query_state.query(&request)
                            };
                            respond(stream, Some(&request), &query_state, result);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
            let receiver = Arc::clone(&connections_rx);
            let query_sender = queries.clone();
            let legacy_sender = legacy_jobs.clone();
            let state = Arc::clone(&state);
            scope.spawn(move || {
                while !state.stopping.load(Ordering::Acquire) {
                    let job = receiver
                        .lock()
                        .expect("managed connection receiver")
                        .recv_timeout(Duration::from_millis(50));
                    let (mut stream, active) = match job {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let request = (|| -> Result<Request> {
                        let line = super::protocol::read_line(
                            &mut stream,
                            managed::MAX_REQUEST_BYTES as u64,
                        )?;
                        let value: Value = serde_json::from_slice(&line)?;
                        let request: Request = if value["protocol"] == 1 {
                            if state.namespace.header().storage
                                != managed::policy::StorageMode::CompatibilityRetainAll
                            {
                                return Err(Error::incompatible(
                                    "legacy clients cannot enter managed storage",
                                )
                                .into());
                            }
                            let legacy: super::protocol::Request = decode(value)?;
                            Request {
                                jsonrpc: legacy.jsonrpc,
                                protocol: legacy.protocol,
                                namespace: state.registration.namespace.clone(),
                                instance: Id::parse(legacy.instance)?,
                                repository: legacy.repository,
                                method: legacy.method,
                                params: legacy.params,
                                id: legacy.id,
                            }
                        } else {
                            decode(value)?
                        };
                        if request.jsonrpc != "2.0"
                            || !matches!(request.protocol, 1 | managed::PROTOCOL_VERSION)
                            || request.namespace != state.registration.namespace
                            || request.instance != state.registration.instance
                            || request.repository != state.registration.repository
                        {
                            return Err(Error::new(
                                ErrorCategory::StaleIdentity,
                                "wrong-service-identity",
                                "namespace, protocol, or instance differs",
                            )
                            .into());
                        }
                        Ok(request)
                    })();
                    let request = match request {
                        Ok(request) => request,
                        Err(error) => {
                            respond(stream, None, &state, Err(error));
                            continue;
                        }
                    };
                    if request.protocol == 1
                        || matches!(request.method.as_str(), "search" | "files" | "status")
                    {
                        let sender = if request.protocol == 1
                            && matches!(request.method.as_str(), "attach" | "detach" | "refresh")
                        {
                            &legacy_sender
                        } else {
                            &query_sender
                        };
                        match sender.try_send((stream, request, active)) {
                            Ok(()) => {}
                            Err(
                                mpsc::TrySendError::Full((stream, request, _active))
                                | mpsc::TrySendError::Disconnected((stream, request, _active)),
                            ) => {
                                respond(
                                    stream,
                                    Some(&request),
                                    &state,
                                    Err(Error::pressure("query-queue-full").into()),
                                );
                            }
                        }
                    } else {
                        let result = state.lifecycle(&request);
                        respond(stream, Some(&request), &state, result);
                    }
                }
            });
        }
        let scheduler = Arc::clone(&state);
        scope.spawn(move || {
            let started = Instant::now();
            let mut pending_after = None;
            let mut recovery = None;
            let mut collection = None;
            let mut collection_cursor = None;
            let mut collection_version = None;
            let mut last_collection = Instant::now();
            let mut recovery_schedule = RetrySchedule::default();
            let mut collection_schedule = RetrySchedule::default();
            let mut pending_schedule = RetrySchedule::default();
            let mut lifecycle_epoch = 0;
            let mut collection_requested = false;
            let mut recovered_changes = false;
            let mut collection_changes = false;
            let mut view_after: Option<Id> = None;
            let mut drain_after: Option<Id> = None;
            while !scheduler.stopping.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(100));
                let tick = (|| -> Result<()> {
                    {
                        let admission = scheduler.admission.lock().map_err(|_| Error::corrupt("admission gate poisoned"))?;
                        if !*admission { return Ok(()); }
                        scheduler.background_batches.fetch_add(1, Ordering::AcqRel);
                    }
                    let _background = BackgroundGuard(&scheduler);
                    let policy = scheduler.namespace.policy()?;
                    let retry = Duration::from_millis(policy.policy.collection.retry_ms);
                    let now = Instant::now();
                    let epoch = scheduler.lifecycle_epoch.load(Ordering::Acquire);
                    if epoch != lifecycle_epoch {
                        lifecycle_epoch = epoch;
                        recovery_schedule.request();
                        pending_schedule.request();
                        collection_requested = true;
                        collection_schedule.request();
                    }
                    if recovery_schedule.due(now, retry) {
                        match scheduler.namespace.recover_pass(recovery.clone()) {
                            Ok(progress) => {
                                recovered_changes |= progress.mutation_committed || progress.owners_reaped != 0
                                    || progress.reservations_released != 0 || progress.views_retired != 0
                                    || progress.operations_recovered != 0 || progress.objects_retired != 0
                                    || progress.objects_quarantined != 0 || progress.members_refreshed != 0
                                    || progress.cleanup != managed::CleanupCounts::default();
                                recovery = progress.next;
                                recovery_schedule.completed(Instant::now(), recovered_changes, recovery.is_some());
                                if recovery.is_none() { recovered_changes = false; }
                                for issue in progress.issues {
                                    eprintln!("managed recovery {} {}: {}", issue.kind, issue.identity, issue.error);
                                }
                            }
                            Err(error) => {
                                recovery_schedule.completed(Instant::now(), false, false);
                                return Err(error.into());
                            }
                        }
                    }
                    if pending_schedule.due(now, Duration::from_millis(100)) {
                        let pending = scheduler.namespace.pending_operations(pending_after.as_ref())?;
                        for operation in &pending {
                            if operation.instance == *scheduler.namespace.instance() { scheduler.enqueue(operation)?; }
                        }
                        pending_after = if pending.len() == policy.policy.work.page_objects as usize {
                            pending.last().map(|operation| operation.id.clone())
                        } else { None };
                        pending_schedule.completed(Instant::now(), false, pending_after.is_some());
                    }
                    drain_after = scheduler.views.drain_released_page(drain_after.as_ref())?.1;
                    let (slots, next) = scheduler.views.slots_page(view_after.as_ref())?;
                    for slot in &slots { scheduler.schedule_view(slot)?; }
                    view_after = next;
                    if let Some(id) = collection.take() {
                        let previous = scheduler.namespace.operation(&id)?;
                        if !matches!(previous.state, OperationState::Completed | OperationState::Failed | OperationState::Cancelled) {
                            collection = Some(id);
                        } else if let Some(result) = previous.result {
                            let progress: CollectionProgress = serde_json::from_value(result)?;
                            collection_changes |= progress.retired != 0 || progress.removed != 0 || progress.recovered_objects != 0
                                || progress.logical_bytes_reclaimed != 0 || progress.recovered_logical_bytes != 0;
                            collection_cursor = progress.next;
                            collection_schedule.completed(Instant::now(), collection_changes, collection_cursor.is_some());
                            if collection_cursor.is_none() { collection_changes = false; }
                        } else {
                            collection_schedule.completed(Instant::now(), false, false);
                            if let Some(error) = previous.error { eprintln!("managed collection failed: {error}"); }
                        }
                    }
                    {
                        let queued = scheduler.queued.lock().map_err(|_| Error::corrupt("job registry poisoned"))?;
                        let mut protected: Vec<Id> = queued.keys().cloned().collect();
                        protected.extend(collection.iter().cloned());
                        scheduler.namespace.acknowledge_system_operations(&protected)?;
                    }
                    let usage = scheduler.namespace.work_usage()?;
                    let pressure = policy.policy.collection.on_pressure
                        && usage.storage.charged_budget_shortfall_bytes.is_some_and(|shortfall| shortfall != 0);
                    let enabled = policy.policy.collection.on_pressure
                        || matches!(policy.policy.collection.schedule, managed::policy::Schedule::Periodic { .. });
                    let periodic = match policy.policy.collection.schedule {
                        managed::policy::Schedule::Disabled => false,
                        managed::policy::Schedule::Periodic { interval_ms } => last_collection.elapsed() >= Duration::from_millis(interval_ms),
                    };
                    if collection.is_none() && enabled
                        && (periodic || pressure || collection_requested || collection_cursor.is_some())
                        && collection_schedule.due(Instant::now(), retry)
                    {
                        if collection_version != Some(policy.version) {
                            collection_cursor = None;
                            collection_version = Some(policy.version);
                        }
                        let request = CollectionRequest { policy_version: policy.version,
                            allocation_version: scheduler.namespace.allocation()?.version,
                            bounds: CollectionBounds::from_policy(&policy.policy.collection), cursor: collection_cursor.take() };
                        let operation = scheduler.namespace.accept_system_operation(Token::parse(Id::new()?.to_string())?, "collection", serde_json::to_value(request)?)?;
                        scheduler.enqueue(&operation)?;
                        collection = Some(operation.id);
                        last_collection = Instant::now();
                        collection_requested = false;
                    }
                    *scheduler.scheduler_snapshot.lock().map_err(|_| Error::corrupt("scheduler observations poisoned"))? = json!({
                        "initializing":false,"tick_ms":100,"maximum_backoff_multiplier":64,
                        "automatic_collection_enabled":enabled,"pressure":pressure,
                        "recovery_continuation":recovery.is_some(),"collection_continuation":collection_cursor.is_some(),
                        "collection_in_flight":collection.is_some(),
                        "recovery_retry_ms":recovery_schedule.delay(retry).as_millis(),
                        "collection_retry_ms":collection_schedule.delay(retry).as_millis(),
                        "pending_retry_ms":pending_schedule.delay(Duration::from_millis(100)).as_millis()
                    });
                    Ok(())
                })();
                let mut diagnostics = scheduler.scheduler_diagnostics.lock().expect("scheduler diagnostics");
                if diagnostics.observe(started.elapsed(), tick.err().map(|error| format!("{error:#}"))) {
                    eprintln!("managed scheduler: {}", diagnostics.last_error_detail.as_deref().expect("observed error"));
                }
            }
        });
        while !state.stopping.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                    let admission = state
                        .admission
                        .lock()
                        .map_err(|_| Error::corrupt("admission gate poisoned"))?;
                    if !*admission {
                        respond(
                            stream,
                            None,
                            &state,
                            Err(Error::busy("daemon-admission-closed").into()),
                        );
                        continue;
                    }
                    state.requests.fetch_add(1, Ordering::AcqRel);
                    match connections.try_send((stream, RequestGuard(Arc::clone(&state)))) {
                        Ok(()) => {}
                        Err(
                            mpsc::TrySendError::Full((stream, _active))
                            | mpsc::TrySendError::Disconnected((stream, _active)),
                        ) => {
                            respond(
                                stream,
                                None,
                                &state,
                                Err(Error::pressure("connection-queue-full").into()),
                            );
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(error) => {
                    state.stopping.store(true, Ordering::Release);
                    return Err(error.into());
                }
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod scheduling_tests {
    use super::*;

    #[test]
    fn successful_ticks_preserve_timestamped_failure_evidence_without_exposing_details_in_aggregates()
     {
        let mut observations = SchedulerDiagnostics::default();
        assert!(observations.observe(Duration::from_millis(1), Some("local path detail".into())));
        assert!(!observations.observe(Duration::from_millis(2), Some("local path detail".into())));
        assert!(!observations.observe(Duration::from_millis(3), None));
        assert_eq!(observations.failed_ticks, 2);
        assert_eq!(observations.consecutive_failed_ticks, 0);
        assert_eq!(observations.last_failure_elapsed_ms, Some(2));
        assert_eq!(observations.last_successful_tick_elapsed_ms, Some(3));
        assert_eq!(
            observations.last_error_detail.as_deref(),
            Some("local path detail")
        );
        assert!(
            !serde_json::to_string(&observations)
                .unwrap()
                .contains("local path detail")
        );
    }

    #[test]
    fn empty_passes_back_off_and_lifecycle_triggers_coalesce() {
        let base = Duration::from_millis(100);
        let mut now = Instant::now();
        let mut schedule = RetrySchedule::default();
        assert!(schedule.due(now, base));
        for exponent in 1..=8 {
            schedule.completed(now, false, false);
            let delay = base * (1 << exponent.min(6));
            assert_eq!(schedule.delay(base), delay);
            assert!(!schedule.due(now + delay - Duration::from_millis(1), base));
            now += delay;
            assert!(schedule.due(now, base));
        }
        schedule.completed(now, false, false);
        for _ in 0..100 {
            schedule.request();
        }
        assert!(
            !schedule.due(now, base),
            "coalesced triggers must not remove the minimum retry interval"
        );
        assert!(schedule.due(now + base, base));
        schedule.completed(now + base, false, true);
        assert_eq!(
            schedule.delay(base),
            base,
            "bounded traversal continuations should progress"
        );
    }
}
