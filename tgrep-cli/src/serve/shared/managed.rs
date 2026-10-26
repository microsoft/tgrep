// Copyright (c) Microsoft Corporation. All rights reserved.

use super::protocol_v2::{
    AUTH_MARKER, Authentication, MARKER, Registration, Request, VIEW_MARKER, ViewRegistration,
};
use super::server::{Options, QuerySource};
use anyhow::{Context, Result, ensure};
use notify::{EventKind, Watcher};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tgrep_core::generations::{ContentEligibility, GenerationManager, IndexingProfile, Repository};
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
            "bounded-storage-inventory","namespace-discovery","maintenance-diagnostics",
            "private-rpc-authentication","bounded-reconciliation","reconciliation-phase-metrics",
            "event-driven-native-reconciliation","serving-native-periodic-verification",
            "content-eligibility-profile","paged-results","filtered-files"
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

const VERIFICATION_DUTY_FACTOR: u32 = 19;
const REREAD_INTERVALS: u32 = 15;

fn verification_delay(interval: Duration, cost: Duration) -> Duration {
    interval.max(cost.saturating_mul(VERIFICATION_DUTY_FACTOR))
}

fn content_verification_due(
    interval: Duration,
    cost: Duration,
    verification_elapsed: Duration,
    reread_elapsed: Duration,
    attempt_elapsed: Duration,
) -> bool {
    reread_elapsed >= interval.saturating_mul(REREAD_INTERVALS)
        && attempt_elapsed >= verification_delay(interval, cost)
        && verification_elapsed >= interval
}

struct Monitor {
    watcher: Option<notify::RecommendedWatcher>,
    watched: HashSet<PathBuf>,
    fallback: Option<String>,
    watch_failure: Arc<Mutex<Option<String>>>,
    watch_counters: Arc<WatchCounters>,
    last_schedule: Instant,
    last_reconcile: Instant,
    /// Last completed full pass or terminal serving-verification attempt.
    last_verification: Instant,
    /// Last successful content pass, not advanced by failed or cancelled attempts.
    last_reread: Instant,
    last_reread_attempt: Instant,
    stat_cost: Duration,
    reread_cost: Duration,
    verification: Option<&'static str>,
    verification_tier: Option<&'static str>,
    pending: HashSet<Id>,
    failures: u32,
    #[cfg(feature = "managed-test-hooks")]
    suppress_events: Arc<std::sync::atomic::AtomicBool>,
}

struct WatchSettings {
    no_watch: bool,
    mode: crate::serve::WatchMode,
    interval: Duration,
    watches_per_view: usize,
}

/// Native watcher input accounting, reported per view in status.
#[derive(Default)]
struct WatchCounters {
    events: AtomicU64,
    access: AtomicU64,
    create: AtomicU64,
    modify: AtomicU64,
    remove: AtomicU64,
    other: AtomicU64,
    errors: AtomicU64,
    unchanged_paths: AtomicU64,
    invalidated_paths: AtomicU64,
    full_invalidations: AtomicU64,
    tree_invalidations: AtomicU64,
    git_metadata_ignored: AtomicU64,
    epoch_advances: AtomicU64,
}

impl WatchCounters {
    fn add(counter: &AtomicU64, value: u64) {
        counter.fetch_add(value, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Value {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        json!({
            "events": load(&self.events),
            "by_kind": {
                "access": load(&self.access),
                "create": load(&self.create),
                "modify": load(&self.modify),
                "remove": load(&self.remove),
                "other": load(&self.other),
                "error": load(&self.errors),
            },
            "unchanged_paths_filtered": load(&self.unchanged_paths),
            "invalidated_paths": load(&self.invalidated_paths),
            "full_invalidations": load(&self.full_invalidations),
            "tree_invalidations": load(&self.tree_invalidations),
            "git_metadata_ignored": load(&self.git_metadata_ignored),
            "epoch_advances": load(&self.epoch_advances),
        })
    }
}

impl WatchSettings {
    fn registration_mode(&self) -> notify::RecursiveMode {
        if cfg!(any(windows, target_os = "macos")) {
            notify::RecursiveMode::Recursive
        } else {
            notify::RecursiveMode::NonRecursive
        }
    }

    fn polling(&self, monitor: &Monitor) -> bool {
        !self.no_watch
            && (matches!(self.mode, crate::serve::WatchMode::Poll)
                || monitor.watcher.is_none()
                || monitor.fallback.is_some())
    }

    fn reconciliation_status(&self, polling: bool) -> Value {
        let mut triggers = vec![
            "first-attachment-or-restore",
            "explicit-full-refresh",
            "candidate-read-error",
        ];
        if !self.no_watch {
            triggers.extend(["watcher-error-or-overflow", "git-or-ignore-change"]);
        }
        let periodic = !self.no_watch;
        let polling = polling || (periodic && matches!(self.mode, crate::serve::WatchMode::Poll));
        if polling {
            triggers.push("poll-interval");
        } else if periodic {
            triggers.push("periodic-verification");
        }
        json!({
            "configured_watch_mode": if self.no_watch { "disabled" }
                else if matches!(self.mode, crate::serve::WatchMode::Poll) { "poll" } else { "auto" },
            "native_periodic_full_verification": periodic,
            "periodic_full_verification": periodic,
            "interval_ms": periodic.then_some(self.interval.as_millis()),
            "poll_interval_ms": self.interval.as_millis(),
            "metadata_interval_ms": periodic.then_some(self.interval.as_millis()),
            "content_interval_ms": periodic.then_some(self.interval.saturating_mul(REREAD_INTERVALS).as_millis()),
            "verification_idle_cost_multiplier": VERIFICATION_DUTY_FACTOR,
            "incremental_coverage": "unavailable",
            "change_journal": "not-integrated",
            "native_missed_changes": if periodic { "repaired-by-periodic-verification" }
                else { "requires-explicit-refresh" },
            "native_verification_availability": "serving-until-difference",
            "native_registration_mode": if matches!(self.registration_mode(), notify::RecursiveMode::Recursive) {
                "recursive-roots"
            } else { "per-directory" },
            "native_watch_budget_per_view": self.watches_per_view,
            "full_verification_reason": if polling { Some("polling-update-mechanism") }
                else if periodic { Some("periodic-native-verification") } else { None },
            "full_repair_triggers": triggers
        })
    }
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

/// Scheduling class of an accepted managed job. A free worker runs the queued
/// job that became due first, and a job becomes due its class delay after it
/// was queued. Newer work of a higher class therefore overtakes a lower class
/// for at most that delay, and no class starves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum JobClass {
    /// Client operations, and reconciliation of a view that is not ready.
    Foreground,
    /// Collection passes.
    Maintenance,
    /// Serving verification and automatic advancement of ready views.
    Background,
}

impl JobClass {
    const ALL: [Self; 3] = [Self::Foreground, Self::Maintenance, Self::Background];

    fn of(operation: &OperationRecord, instance: &Id) -> Self {
        Self::classify(
            &operation.kind,
            &operation.request,
            operation.token.scope == *instance,
        )
    }

    fn classify(kind: &str, request: &Value, automatic: bool) -> Self {
        match kind {
            "collection" => Self::Maintenance,
            "reconcile" if request["verify"].as_bool() == Some(true) => Self::Background,
            "adaptive" if automatic => Self::Background,
            _ => Self::Foreground,
        }
    }

    /// How long a queued job of this class yields to newer foreground work.
    fn delay(self) -> Duration {
        match self {
            Self::Foreground => Duration::ZERO,
            Self::Maintenance => Duration::from_secs(5),
            Self::Background => Duration::from_secs(600),
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    fn name(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Maintenance => "maintenance",
            Self::Background => "background",
        }
    }
}

#[derive(Default)]
struct JobQueueState {
    queued: [VecDeque<(Id, Instant)>; 3],
    running: Vec<(Id, JobClass, Instant)>,
}

impl JobQueueState {
    fn len(&self) -> usize {
        self.queued.iter().map(VecDeque::len).sum()
    }

    fn is_empty(&self) -> bool {
        self.queued.iter().all(VecDeque::is_empty)
    }

    fn push(&mut self, id: Id, class: JobClass, at: Instant, capacity: usize) -> bool {
        if self.len() >= capacity {
            return false;
        }
        self.queued[class.index()].push_back((id, at));
        true
    }

    /// Takes the queued job that became due first; ties go to the higher class.
    fn pop(&mut self) -> Option<(Id, JobClass)> {
        let (class, _) = JobClass::ALL
            .into_iter()
            .filter_map(|class| {
                let (_, at) = self.queued[class.index()].front()?;
                Some((class, at.checked_add(class.delay()).unwrap_or(*at)))
            })
            .min_by_key(|(_, due)| *due)?;
        let (id, _) = self.queued[class.index()].pop_front()?;
        Some((id, class))
    }

    /// Moves a queued job into a higher class, keeping its queue time.
    fn promote(&mut self, id: &Id, class: JobClass) -> bool {
        for lower in JobClass::ALL.into_iter().filter(|lower| *lower > class) {
            let queue = &mut self.queued[lower.index()];
            let Some(position) = queue.iter().position(|(queued, _)| queued == id) else {
                continue;
            };
            let Some(job) = queue.remove(position) else {
                continue;
            };
            let target = &mut self.queued[class.index()];
            let position = target.partition_point(|(_, at)| *at <= job.1);
            target.insert(position, job);
            return true;
        }
        false
    }
}

/// Bounded queue of accepted managed jobs. Its lock is a leaf held only for
/// container updates that cannot leave the state inconsistent, so a poisoned
/// lock is recovered.
struct JobQueue {
    capacity: usize,
    state: Mutex<JobQueueState>,
    available: Condvar,
}

impl JobQueue {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(JobQueueState::default()),
            available: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, JobQueueState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, id: Id, class: JobClass) -> bool {
        let pushed = self.lock().push(id, class, Instant::now(), self.capacity);
        if pushed {
            self.available.notify_one();
        }
        pushed
    }

    /// Waits up to `timeout` for a job and marks it running.
    fn pop(&self, timeout: Duration) -> Option<RunningJob<'_>> {
        let (mut state, _) = self
            .available
            .wait_timeout_while(self.lock(), timeout, |state| state.is_empty())
            .unwrap_or_else(PoisonError::into_inner);
        let (id, class) = state.pop()?;
        state.running.push((id.clone(), class, Instant::now()));
        Some(RunningJob { queue: self, id })
    }

    fn promote(&self, id: &Id, class: JobClass) -> bool {
        self.lock().promote(id, class)
    }

    fn running(&self, id: &Id) -> bool {
        self.lock()
            .running
            .iter()
            .any(|(running, _, _)| running == id)
    }

    fn snapshot(&self) -> Value {
        let state = self.lock();
        let now = Instant::now();
        let mut queued = serde_json::Map::new();
        for class in JobClass::ALL {
            let queue = &state.queued[class.index()];
            queued.insert(
                class.name().to_owned(),
                json!({
                    "depth": queue.len(),
                    "oldest_wait_ms": queue
                        .front()
                        .map(|(_, at)| now.saturating_duration_since(*at).as_millis()),
                }),
            );
        }
        let running: Vec<Value> = state
            .running
            .iter()
            .map(|(id, class, started)| {
                json!({
                    "operation": id,
                    "class": class.name(),
                    "elapsed_ms": now.saturating_duration_since(*started).as_millis(),
                })
            })
            .collect();
        json!({"capacity": self.capacity, "queued": queued, "running": running})
    }

    /// Reports where each accepted operation waits: running, queued in a class,
    /// or accepted but not queued in this process.
    fn describe(&self, ids: &[&Id]) -> Vec<Value> {
        let state = self.lock();
        let now = Instant::now();
        ids.iter()
            .map(|&id| {
                if let Some((_, class, started)) =
                    state.running.iter().find(|(running, _, _)| running == id)
                {
                    return json!({
                        "operation": id,
                        "state": "running",
                        "class": class.name(),
                        "elapsed_ms": now.saturating_duration_since(*started).as_millis(),
                    });
                }
                for class in JobClass::ALL {
                    if let Some((_, at)) = state.queued[class.index()]
                        .iter()
                        .find(|(queued, _)| queued == id)
                    {
                        return json!({
                            "operation": id,
                            "state": "queued",
                            "class": class.name(),
                            "wait_ms": now.saturating_duration_since(*at).as_millis(),
                        });
                    }
                }
                json!({"operation": id, "state": "accepted"})
            })
            .collect()
    }
}

/// Marks a job running until it drops, including on panic.
struct RunningJob<'a> {
    queue: &'a JobQueue,
    id: Id,
}

impl RunningJob<'_> {
    fn id(&self) -> &Id {
        &self.id
    }
}

impl Drop for RunningJob<'_> {
    fn drop(&mut self) {
        self.queue
            .lock()
            .running
            .retain(|(id, _, _)| *id != self.id);
    }
}

struct State {
    registration: Registration,
    authentication: Authentication,
    namespace: Arc<Namespace>,
    views: ViewManager,
    watches: WatchSettings,
    monitors: Arc<Mutex<HashMap<Id, Arc<Mutex<Monitor>>>>>,
    jobs: JobQueue,
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

    /// `progress` resets the backoff. An unproductive continuation keeps the
    /// current backoff so a traversal that reclaims nothing is paced instead
    /// of spinning; the backoff grows when such a traversal completes.
    fn completed(&mut self, now: Instant, progress: bool, more: bool) {
        self.last = Some(now);
        if progress {
            self.exponent = 0;
        } else if !more {
            self.exponent = (self.exponent + 1).min(6);
        }
        self.requested = more && self.exponent == 0;
    }
}

const SCHEDULER_TICK: Duration = Duration::from_millis(100);
const SCHEDULER_IDLE_TICK: Duration = Duration::from_millis(1600);

/// Paces catalog-backed scheduler ticks. Each base tick compares a cheap
/// in-memory signature (lifecycle epoch, view input epochs, queued jobs); a
/// change runs a full tick at once. Otherwise consecutive quiet full ticks
/// back off to `SCHEDULER_IDLE_TICK`, so an idle namespace does not spend
/// catalog and anchor-verification I/O that competes with running work.
#[derive(Default)]
struct IdlePacer {
    signature: Option<(u64, u64, usize, usize)>,
    last: Option<Instant>,
    quiet: u32,
}

impl IdlePacer {
    fn delay(&self) -> Duration {
        SCHEDULER_TICK
            .saturating_mul(1 << self.quiet.min(4))
            .min(SCHEDULER_IDLE_TICK)
    }

    fn due(&self, now: Instant, signature: (u64, u64, usize, usize)) -> bool {
        self.signature != Some(signature)
            || self
                .last
                .is_none_or(|last| now.saturating_duration_since(last) >= self.delay())
    }

    fn completed(&mut self, now: Instant, signature: (u64, u64, usize, usize), quiet: bool) {
        self.signature = Some(signature);
        self.last = Some(now);
        self.quiet = if quiet { (self.quiet + 1).min(4) } else { 0 };
    }
}

struct ManagedQuery {
    query: ViewQuery,
    slot: Arc<ViewSlot>,
}

/// Recovery counts as productive only when it actually changed or reclaimed
/// something. Verification bytes and committed-error flags alone do not reset
/// the backoff, so a pass that reclaims nothing cannot spin hot.
fn recovery_productive(progress: &managed::RecoveryProgress) -> bool {
    let mut cleanup = progress.cleanup.clone();
    cleanup.control_verification_bytes = 0;
    progress.owners_reaped != 0
        || progress.reservations_released != 0
        || progress.views_retired != 0
        || progress.operations_recovered != 0
        || progress.objects_retired != 0
        || progress.objects_quarantined != 0
        || progress.members_refreshed != 0
        || cleanup != managed::CleanupCounts::default()
}

/// Scheduler phase failures. A failed phase is reported, but never prevents
/// the remaining phases of the same tick from running.
#[derive(Default)]
struct PhaseFailures(Vec<(&'static str, anyhow::Error)>);

impl PhaseFailures {
    fn record(&mut self, phase: &'static str, error: anyhow::Error) {
        self.0.push((phase, error));
    }

    fn check(&mut self, phase: &'static str, result: Result<()>) {
        if let Err(error) = result {
            self.record(phase, error);
        }
    }

    fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self.0.iter().map(|(phase, _)| *phase).collect();
        names.dedup();
        names
    }

    fn into_result(self) -> Result<()> {
        if self.0.is_empty() {
            return Ok(());
        }
        let detail = self
            .0
            .iter()
            .map(|(phase, error)| format!("{phase}: {error:#}"))
            .collect::<Vec<_>>()
            .join("; ");
        Err(anyhow::anyhow!(detail))
    }
}

impl QuerySource for ManagedQuery {
    fn root(&self) -> &Path {
        self.slot.root()
    }
    fn eligibility(&self) -> Result<ContentEligibility> {
        Ok(self
            .query
            .published()
            .record
            .pin()?
            .key
            .profile()
            .eligibility)
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
    fn eventual(&self) -> bool {
        self.query.eventual()
    }
    fn requeue(&self, relative: &[String]) -> Result<()> {
        let paths: Vec<_> = relative.iter().map(PathBuf::from).collect();
        self.slot.invalidate(&paths, false)?;
        Ok(())
    }
}

fn monitor_directories(
    root: &Path,
    storage: &Path,
    budget: usize,
    mode: notify::RecursiveMode,
    deadline: Instant,
) -> Result<(HashSet<PathBuf>, Vec<PathBuf>)> {
    let mut directories = HashSet::new();
    let mut stack = vec![(root.to_path_buf(), false)];
    let mut examined = 0_usize;
    let (git, common) = tgrep_core::git_index::read_repository_dirs_bounded(root)?;
    let mut metadata = vec![std::fs::canonicalize(git)?, std::fs::canonicalize(common)?];
    metadata.dedup();
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
    if matches!(mode, notify::RecursiveMode::Recursive) {
        // ReadDirectoryChangesW and FSEvents cover descendants, including new
        // directories. Count subscriptions, not the directories they cover.
        directories.insert(root.to_path_buf());
        let candidates = directories.clone();
        directories.retain(|directory| {
            !candidates
                .iter()
                .any(|parent| parent != directory && directory.starts_with(parent))
        });
        ensure!(
            directories.len() <= budget,
            "native watch count budget; polling"
        );
        return Ok((directories, metadata));
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
    metadata: &[PathBuf],
    storage: &Path,
    watch_failure: &Mutex<Option<String>>,
    counters: &WatchCounters,
    event: notify::Result<notify::Event>,
) -> managed::Result<()> {
    WatchCounters::add(&counters.events, 1);
    match event {
        Ok(event) if matches!(event.kind, EventKind::Access(_)) => {
            WatchCounters::add(&counters.access, 1);
            Ok(())
        }
        Ok(event) => {
            WatchCounters::add(
                match event.kind {
                    EventKind::Create(_) => &counters.create,
                    EventKind::Modify(_) => &counters.modify,
                    EventKind::Remove(_) => &counters.remove,
                    _ => &counters.other,
                },
                1,
            );
            // Windows reports attribute-only changes as Modify(Any); renames
            // are excluded because both endpoints must be rechecked.
            let content_candidate = matches!(
                event.kind,
                EventKind::Modify(
                    notify::event::ModifyKind::Any
                        | notify::event::ModifyKind::Data(_)
                        | notify::event::ModifyKind::Metadata(_)
                        | notify::event::ModifyKind::Other
                )
            ) && !event.need_rescan();
            let mut paths = Vec::new();
            let full = event.need_rescan();
            let mut tree =
                event.paths.is_empty() || matches!(event.kind, EventKind::Any | EventKind::Other);
            for path in event.paths {
                if path.starts_with(storage) {
                    continue;
                }
                if let Some(relative) = metadata
                    .iter()
                    .filter_map(|directory| path.strip_prefix(directory).ok())
                    .min_by_key(|relative| relative.components().count())
                {
                    let first = relative.components().next().map(|part| part.as_os_str());
                    if !path
                        .extension()
                        .is_some_and(|extension| extension == "lock")
                        && ([
                            "HEAD",
                            "index",
                            "config",
                            "config.worktree",
                            "commondir",
                            "info",
                        ]
                        .iter()
                        .any(|name| first == Some(std::ffi::OsStr::new(name)))
                            || relative.starts_with(Path::new("refs").join("heads")))
                    {
                        // Index membership can change case-insensitive ignore
                        // exemptions; HEAD also drives adaptive advancement.
                        tree = true;
                    } else if relative.as_os_str().is_empty()
                        && !matches!(event.kind, EventKind::Modify(_))
                    {
                        tree = true;
                    } else {
                        WatchCounters::add(&counters.git_metadata_ignored, 1);
                    }
                    continue;
                }
                match path.strip_prefix(slot.root()) {
                    Ok(relative) if relative.starts_with(".tgrep") => {}
                    Ok(relative)
                        if relative.components().any(|part| part.as_os_str() == ".git")
                            && (relative != Path::new(".git") || path.is_dir()) => {}
                    Ok(relative) if relative == Path::new(".git") => tree = true,
                    Ok(relative) if !relative.as_os_str().is_empty() => {
                        // Drop notifications that left a file's trusted
                        // metadata equal to the indexed evidence, so external
                        // walkers and attribute churn do not trigger passes.
                        if content_candidate && let Ok(metadata) = std::fs::metadata(&path) {
                            if metadata.is_dir() {
                                // A parent notification may be the only sign of
                                // new membership. Rewalk, but do not reread its subtree.
                                tree = true;
                                continue;
                            }
                            if slot.unchanged_file(relative, &metadata) {
                                WatchCounters::add(&counters.unchanged_paths, 1);
                                continue;
                            }
                        }
                        paths.push(relative.to_path_buf())
                    }
                    _ => tree = true,
                }
            }
            if full || tree || !paths.is_empty() {
                WatchCounters::add(&counters.invalidated_paths, paths.len() as u64);
                WatchCounters::add(&counters.full_invalidations, u64::from(full));
                WatchCounters::add(&counters.tree_invalidations, u64::from(tree && !full));
                WatchCounters::add(&counters.epoch_advances, 1);
                if tree && !full {
                    slot.invalidate_tree(&paths).map(|_| ())
                } else {
                    slot.invalidate(&paths, full).map(|_| ())
                }
            } else {
                Ok(())
            }
        }
        Err(error) => {
            WatchCounters::add(&counters.errors, 1);
            WatchCounters::add(&counters.full_invalidations, 1);
            WatchCounters::add(&counters.epoch_advances, 1);
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
    let mode = settings.registration_mode();
    let (desired, metadata) = match monitor_directories(
        slot.root(),
        storage,
        settings.watches_per_view,
        mode,
        Instant::now() + Duration::from_millis(100),
    ) {
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
        let watch_counters = Arc::clone(&monitor.watch_counters);
        let storage = storage.to_path_buf();
        #[cfg(feature = "managed-test-hooks")]
        let suppress = Arc::clone(&monitor.suppress_events);
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            #[cfg(feature = "managed-test-hooks")]
            if suppress.load(Ordering::Acquire) {
                return;
            }
            let Some(slot) = weak.upgrade() else {
                return;
            };
            let result = watch_event(
                &slot,
                &metadata,
                &storage,
                &watch_failure,
                &watch_counters,
                event,
            );
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
            // inotify can retire a deleted or moved directory before this sync.
            if let Err(error) = watcher.unwatch(directory)
                && !matches!(error.kind, notify::ErrorKind::WatchNotFound)
            {
                return Err(error);
            }
        }
        for directory in &added {
            watcher.watch(directory, mode)?;
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
    fn watch_status(&self, view: &Id) -> Result<Value> {
        let monitor = self
            .monitors
            .lock()
            .map_err(|_| Error::corrupt("monitor registry poisoned"))?
            .get(view)
            .cloned();
        let monitor = monitor
            .as_ref()
            .map(|monitor| {
                monitor
                    .lock()
                    .map_err(|_| Error::corrupt("monitor poisoned"))
            })
            .transpose()?;
        let polling = monitor
            .as_ref()
            .is_some_and(|monitor| self.watches.polling(monitor));
        let queued_work = monitor.as_ref().map(|monitor| {
            let mut pending: Vec<&Id> = monitor.pending.iter().collect();
            pending.sort();
            self.jobs.describe(&pending)
        });
        Ok(json!({
            "watch_mode": if self.watches.no_watch { "disabled" }
                else if polling { "poll" }
                else if monitor.is_some() { "native" } else { "unavailable" },
            "watch_fallback": monitor.as_ref().and_then(|monitor| monitor.fallback.as_ref()),
            "watch_count": monitor.as_ref().map_or(0, |monitor| monitor.watched.len()),
            "watch_events": monitor.as_ref().map(|monitor| monitor.watch_counters.snapshot()),
            "last_verification": monitor.as_ref().map(|monitor| json!({
                "outcome": monitor.verification,
                "tier": monitor.verification_tier,
                "age_ms": monitor.last_verification.elapsed().as_millis(),
                "content_age_ms": monitor.last_reread.elapsed().as_millis(),
                "content_attempt_age_ms": monitor.last_reread_attempt.elapsed().as_millis(),
                "metadata_delay_ms": verification_delay(self.watches.interval, monitor.stat_cost).as_millis(),
                "content_delay_ms": verification_delay(self.watches.interval.saturating_mul(REREAD_INTERVALS), monitor.reread_cost).as_millis(),
                "content_retry_delay_ms": verification_delay(self.watches.interval, monitor.reread_cost).as_millis(),
            })),
            "reconciliation_policy": self.watches.reconciliation_status(polling),
            "queued_work": queued_work
        }))
    }

    fn namespace_reconciliation_status(&self) -> Result<Value> {
        let monitors = self
            .monitors
            .lock()
            .map_err(|_| Error::corrupt("monitor registry poisoned"))?;
        let (mut native, mut polling, mut disabled) = (0_usize, 0_usize, 0_usize);
        let mut watch_count = 0_usize;
        for monitor in monitors.values() {
            let monitor = monitor
                .lock()
                .map_err(|_| Error::corrupt("monitor poisoned"))?;
            watch_count += monitor.watched.len();
            if self.watches.no_watch {
                disabled += 1;
            } else if self.watches.polling(&monitor) {
                polling += 1;
            } else {
                native += 1;
            }
        }
        let mut status = self.watches.reconciliation_status(polling != 0);
        status["scope"] = json!("monitored-views");
        status["native_views"] = json!(native);
        status["polling_views"] = json!(polling);
        status["disabled_views"] = json!(disabled);
        status["watch_count"] = json!(watch_count);
        Ok(status)
    }

    fn legacy_registration(&self) -> super::protocol::Registration {
        super::protocol::Registration {
            protocol: 1,
            instance: self.authentication.legacy_token.to_string(),
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
            "ready":status.ready,"serving":status.serving,"epoch":status.input_epoch,"leases":status.leases,
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
                        verify: false,
                        metadata_only: false,
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
        if !self.jobs.push(
            operation.id.clone(),
            JobClass::of(operation, self.namespace.instance()),
        ) {
            queued.remove(&operation.id);
            // Acceptance is durable; the bounded scheduler will retry it.
        }
        Ok(())
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
            "testing.reconcile-timer" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Input {
                    view: Id,
                    elapsed_ms: u64,
                    #[serde(default)]
                    reread_elapsed_ms: Option<u64>,
                    #[serde(default)]
                    reread_attempt_elapsed_ms: Option<u64>,
                }
                let input: Input = decode(request.params.clone())?;
                if input.elapsed_ms > 86_400_000
                    || input
                        .reread_elapsed_ms
                        .is_some_and(|elapsed| elapsed > 86_400_000)
                    || input
                        .reread_attempt_elapsed_ms
                        .is_some_and(|elapsed| elapsed > 86_400_000)
                {
                    return Err(Error::invalid("test timer exceeds one day").into());
                }
                let slot = self.views.slot(&input.view)?;
                let monitor = self
                    .monitors
                    .lock()
                    .map_err(|_| Error::corrupt("monitor registry poisoned"))?
                    .get(&input.view)
                    .cloned()
                    .ok_or_else(|| Error::invalid("view has no monitor"))?;
                self.settle_view_work(&input.view, &monitor)?;
                self.schedule_view_with_elapsed(&slot, |_| {
                    (
                        Duration::from_secs(30),
                        Duration::from_millis(input.elapsed_ms),
                        Duration::from_millis(input.elapsed_ms),
                        Duration::from_millis(input.reread_elapsed_ms.unwrap_or(input.elapsed_ms)),
                        Duration::from_millis(
                            input
                                .reread_attempt_elapsed_ms
                                .or(input.reread_elapsed_ms)
                                .unwrap_or(input.elapsed_ms),
                        ),
                    )
                })?;
                Ok(json!({"input_epoch":slot.input_epoch()}))
            }
            #[cfg(feature = "managed-test-hooks")]
            "testing.suppress-watch-events" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Input {
                    view: Id,
                    suppress: bool,
                }
                let input: Input = decode(request.params.clone())?;
                let monitor = self
                    .monitors
                    .lock()
                    .map_err(|_| Error::corrupt("monitor registry poisoned"))?
                    .get(&input.view)
                    .cloned()
                    .ok_or_else(|| Error::invalid("view has no monitor"))?;
                let monitor = monitor
                    .lock()
                    .map_err(|_| Error::corrupt("monitor poisoned"))?;
                monitor
                    .suppress_events
                    .store(input.suppress, Ordering::Release);
                Ok(json!({"suppressed":input.suppress}))
            }
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
                let (failure, counters) = {
                    let monitor = monitor
                        .lock()
                        .map_err(|_| Error::corrupt("monitor poisoned"))?;
                    (
                        Arc::clone(&monitor.watch_failure),
                        Arc::clone(&monitor.watch_counters),
                    )
                };
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
                watch_event(
                    &slot,
                    &[],
                    self.namespace.path(),
                    &failure,
                    &counters,
                    event,
                )?;
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
            "views.status" => {
                let id = decode::<ObjectId>(request.params.clone())?.id;
                let mut status = serde_json::to_value(self.views.status(&id)?)?;
                let watch = self.watch_status(&id)?;
                for field in [
                    "watch_mode",
                    "watch_fallback",
                    "watch_count",
                    "watch_events",
                    "last_verification",
                    "reconciliation_policy",
                    "queued_work",
                ] {
                    status[field] = watch[field].clone();
                }
                Ok(status)
            }
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
                        match self.namespace.stop_is_committed() {
                            Ok(true) => self.stopping.store(true, Ordering::Release),
                            Ok(false) => *admission = true,
                            Err(state_error) => {
                                let mut unresolved = Error::new(
                                    ErrorCategory::RecoveryRequired,
                                    "idle-state-unavailable",
                                    format!("idle attempt: {error}; reading shutdown authorization: {state_error}"),
                                )
                                .committed(managed::CommitState::Unknown);
                                unresolved.operation_id = error.operation_id;
                                return Err(unresolved.into());
                            }
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
                    result["reconciliation_policy"] = self.namespace_reconciliation_status()?;
                    result["process_memory"] = process_memory();
                    result["scheduler"] = self
                        .scheduler_snapshot
                        .lock()
                        .map_err(|_| Error::corrupt("scheduler observations poisoned"))?
                        .clone();
                    result["scheduler"]["job_queue"] = self.jobs.snapshot();
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
            let watch = self.watch_status(slot.id())?;
            return Ok(json!({
                "root":slot.root(),"view":slot.id(),"generation":pin.key,"version":status.authoritative.version,
                "ready":status.ready,"serving":status.serving,"epoch":status.input_epoch,"backend":"shared-v2","status":status,
                "watch_mode":watch["watch_mode"],
                "watch_fallback":watch["watch_fallback"],
                "watch_count":watch["watch_count"],
                "watch_events":watch["watch_events"],
                "last_verification":watch["last_verification"],
                "reconciliation_policy":watch["reconciliation_policy"],
                "queued_work":watch["queued_work"],
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
            "incarnation":pin.incarnation,"commit":pin.commit,"ready":true,"lagging":query.lagging(),
            "hidden_complete":true,"ordered_filters":true,
            "multiline_replace":true,"report_replacements":true,"backend":"shared-v2"});
        let source = ManagedQuery { query, slot };
        // Pages end well before the operation deadline so long scans resume
        // instead of failing.
        let page_time =
            Duration::from_millis(self.namespace.policy()?.policy.work.operation_timeout_ms / 2)
                .min(super::server::PAGE_TIME);
        let result = super::server::search_snapshot_parallel(
            &source,
            &input.query,
            request.method == "files",
            &request.id,
            metadata,
            page_time,
        )?;
        source.query.complete()?;
        Ok(result)
    }

    fn job(&self, id: &Id) -> Result<()> {
        let started = Instant::now();
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
                        && !final_operation
                            .result
                            .as_ref()
                            .is_some_and(|result| result["coalesced"] == true)
                    {
                        state.last_reconcile = Instant::now();
                    }
                    let verify =
                        operation.kind == "reconcile" && operation.request["verify"] == true;
                    let result = final_operation.result.as_ref();
                    let outcome = match final_operation.state {
                        OperationState::Completed
                            if result
                                .is_some_and(|result| result["verification"] == "superseded") =>
                        {
                            Some("superseded")
                        }
                        OperationState::Completed
                            if verify
                                && result.is_some_and(|result| result["verified"] == true) =>
                        {
                            Some(if result.is_some_and(|result| result["changed"] == true) {
                                "repaired"
                            } else {
                                "unchanged"
                            })
                        }
                        OperationState::Completed
                            if result.is_some_and(|result| result["reconcile"]["full"] == true) =>
                        {
                            Some("full-pass")
                        }
                        OperationState::Completed
                            if operation.kind == "reconcile"
                                && operation.request["metadata_only"] == true
                                && !result.is_some_and(|result| result["coalesced"] == true) =>
                        {
                            Some("metadata-pass")
                        }
                        OperationState::Failed if verify => Some("failed"),
                        OperationState::Cancelled if verify => Some("cancelled"),
                        _ => None,
                    };
                    if let Some(outcome) = outcome {
                        let completed = Instant::now();
                        state.last_verification = completed;
                        state.verification = Some(outcome);
                        let metadata_only =
                            outcome != "full-pass" && operation.request["metadata_only"] == true;
                        state.verification_tier =
                            Some(if metadata_only { "metadata" } else { "content" });
                        let cost = result
                            .and_then(|result| {
                                result["reconcile"]["timings"]["admitted_us"].as_u64()
                            })
                            .map(Duration::from_micros)
                            .unwrap_or_else(|| started.elapsed());
                        if metadata_only {
                            state.stat_cost = cost;
                        } else {
                            state.reread_cost = cost;
                            state.last_reread_attempt = completed;
                            if matches!(
                                outcome,
                                "full-pass" | "unchanged" | "repaired" | "superseded"
                            ) {
                                state.last_reread = completed;
                            }
                        }
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

    /// Returns whether the view still needs prompt scheduler attention: it
    /// accepted work, or it is stale with no reconciliation running.
    fn schedule_view(&self, slot: &Arc<ViewSlot>) -> Result<bool> {
        self.schedule_view_with_elapsed(slot, |monitor| {
            (
                monitor.last_schedule.elapsed(),
                monitor.last_reconcile.elapsed(),
                monitor.last_verification.elapsed(),
                monitor.last_reread.elapsed(),
                monitor.last_reread_attempt.elapsed(),
            )
        })
    }

    fn schedule_view_with_elapsed(
        &self,
        slot: &Arc<ViewSlot>,
        elapsed: impl FnOnce(&Monitor) -> (Duration, Duration, Duration, Duration, Duration),
    ) -> Result<bool> {
        let status = self.views.status(slot.id())?;
        if status.leases == 0 {
            return Ok(false);
        }
        if status.closing || status.work.is_some() || !status.authoritative.committed {
            return Ok(false);
        }
        let monitor = self
            .monitors
            .lock()
            .map_err(|_| Error::corrupt("monitor registry poisoned"))?
            .get(slot.id())
            .cloned();
        let Some(monitor) = monitor else {
            return Ok(false);
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
        self.prune_pending(&mut monitor)?;
        if !monitor.pending.is_empty() {
            if !status.ready {
                // A view that lost readiness must not wait behind other views'
                // background work; a queued verification of it then reconciles
                // it as an ordinary pass.
                for id in &monitor.pending {
                    self.jobs.promote(id, JobClass::Foreground);
                }
            }
            return Ok(true);
        }
        let retry = Duration::from_millis(200_u64.saturating_mul(1_u64 << monitor.failures.min(7)));
        let (
            schedule_elapsed,
            reconcile_elapsed,
            verification_elapsed,
            reread_elapsed,
            reread_attempt_elapsed,
        ) = elapsed(&monitor);
        let polling = self.watches.polling(&monitor);
        let stat_due = reconcile_elapsed.min(verification_elapsed)
            >= verification_delay(self.watches.interval, monitor.stat_cost);
        let reread_due = content_verification_due(
            self.watches.interval,
            monitor.reread_cost,
            verification_elapsed,
            reread_elapsed,
            reread_attempt_elapsed,
        );
        let periodic = polling && (stat_due || reread_due);
        // Healthy native views are verified while serving; only a detected
        // difference closes readiness.
        let verify = !periodic
            && status.ready
            && !polling
            && !self.watches.no_watch
            && monitor.watcher.is_some()
            && (stat_due || reread_due);
        if (!status.ready || periodic || verify) && schedule_elapsed >= retry {
            monitor.last_schedule = Instant::now();
            drop(monitor);
            if periodic {
                if reread_due {
                    slot.invalidate(&[], true)?;
                } else {
                    slot.invalidate_tree(&[])?;
                }
            }
            let operation = self.views.accept_reconciliation(
                Token::parse(Id::new()?.to_string())?,
                ReconcileRequest {
                    view: slot.id().clone(),
                    expected_version: status.authoritative.version,
                    allocation_version: self.namespace.allocation()?.version,
                    verify,
                    metadata_only: (periodic || verify) && !reread_due,
                },
            )?;
            self.enqueue(&operation)?;
            return Ok(true);
        } else if status.ready
            && schedule_elapsed >= retry
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
                return Ok(true);
            }
        }
        Ok(!status.ready)
    }

    fn prune_pending(&self, monitor: &mut Monitor) -> Result<()> {
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
        Ok(())
    }

    /// Waits until the view has no running or queued work, so a test timer
    /// is not silently deferred by a concurrent scheduler pass.
    #[cfg(feature = "managed-test-hooks")]
    fn settle_view_work(&self, view: &Id, monitor: &Mutex<Monitor>) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let pending = {
                let mut monitor = monitor
                    .lock()
                    .map_err(|_| Error::corrupt("monitor lock poisoned"))?;
                self.prune_pending(&mut monitor)?;
                !monitor.pending.is_empty()
            };
            if !pending && self.views.status(view)?.work.is_none() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::busy("view-work-active").into());
            }
            thread::sleep(Duration::from_millis(10));
        }
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
            data["instance"] = json!(state.authentication.legacy_token);
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
    files: Vec<(PathBuf, managed::FileIdentity)>,
    _git_lock: File,
}

impl Drop for DaemonRegistration {
    fn drop(&mut self) {
        for (path, identity) in self.files.iter().rev() {
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
    // `accept` blocks; a waker thread connects once `stopping` is set.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let registration = Registration {
        protocol: managed::PROTOCOL_VERSION,
        namespace: namespace.header().namespace.clone(),
        instance: namespace.instance().clone(),
        repository: repository.identity().into(),
        pid: std::process::id(),
        port: listener.local_addr()?.port(),
        storage: namespace.path().into(),
    };
    let authentication = Authentication::new(registration.clone())?;
    let legacy_instance = authentication.legacy_token.clone();
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
                    instance: legacy_instance.to_string(),
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
        let sentinel_identity = managed::publish_private_control_file(&sentinel, &sentinel_value)?;
        let monitor = Arc::new(Mutex::new(Monitor {
            watcher: None,
            watched: HashSet::new(),
            fallback: None,
            last_schedule: Instant::now(),
            watch_failure: Arc::new(Mutex::new(None)),
            watch_counters: Arc::new(WatchCounters::default()),
            last_reconcile: Instant::now(),
            last_verification: Instant::now(),
            last_reread: Instant::now(),
            last_reread_attempt: Instant::now(),
            stat_cost: Duration::ZERO,
            reread_cost: Duration::ZERO,
            verification: None,
            verification_tier: None,
            pending: HashSet::new(),
            failures: 0,
            #[cfg(feature = "managed-test-hooks")]
            suppress_events: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            // Managed views favor availability: queries keep using the last
            // published snapshot while newer changes reconcile.
            stale_reads: true,
            ..WorktreeOptions::default()
        },
        Some(observer),
    )?;
    let state = Arc::new(State {
        registration: registration.clone(),
        authentication,
        namespace,
        views,
        watches,
        monitors,
        jobs: JobQueue::new(policy.work.queue_items as usize),
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
    let mut registration_guard = DaemonRegistration {
        files: Vec::with_capacity(3),
        _git_lock: git_lock,
    };
    let authentication_path = repository.common_dir().join(AUTH_MARKER);
    let authentication_identity = managed::publish_private_control_file(
        &authentication_path,
        &serde_json::to_value(&state.authentication)?,
    )?;
    registration_guard
        .files
        .push((authentication_path, authentication_identity));
    let marker = repository.common_dir().join(MARKER);
    let marker_identity = managed::publish_control_file(&marker, &json!(registration))?;
    registration_guard.files.push((marker, marker_identity));
    let sentinel = repository.common_dir().join(super::MARKER);
    let sentinel_value = if policy.storage == managed::policy::StorageMode::CompatibilityRetainAll {
        json!(state.legacy_registration())
    } else {
        json!({"protocol":2,"namespace":registration.namespace,"instance":registration.instance})
    };
    let sentinel_identity = managed::publish_private_control_file(&sentinel, &sentinel_value)?;
    registration_guard.files.push((sentinel, sentinel_identity));
    eprintln!(
        "shared-v2 listening on {} (namespace {})",
        registration.port, registration.namespace
    );
    // Cleared once the accept loop returns; the waker retries until then.
    let accepting = AtomicBool::new(true);
    thread::scope(|scope| -> Result<()> {
        struct Shutdown<'a>(&'a AtomicBool, &'a AtomicBool);
        impl Drop for Shutdown<'_> {
            fn drop(&mut self) {
                self.1.store(false, Ordering::Release);
                self.0.store(true, Ordering::Release);
            }
        }
        let _shutdown = Shutdown(&state.stopping, &accepting);
        for _ in 0..policy.work.workers {
            let state = Arc::clone(&state);
            scope.spawn(move || {
                while !state.stopping.load(Ordering::Acquire) {
                    let Some(job) = state.jobs.pop(Duration::from_millis(50)) else {
                        continue;
                    };
                    let id = job.id().clone();
                    if let Err(error) = state.job(&id) {
                        eprintln!("managed operation {id}: {error:#}");
                    }
                    // Leave the running set before the registry can admit the
                    // same operation again.
                    drop(job);
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
                        let mut value: Value = serde_json::from_slice(&line)?;
                        let legacy_protocol = value["protocol"] == 1;
                        let authorization = if legacy_protocol {
                            value["instance"].as_str()
                        } else {
                            value["authorization"].as_str()
                        };
                        if !state.authentication.accepts(authorization, legacy_protocol) {
                            return Err(Error::new(
                                ErrorCategory::Permission,
                                "rpc-authentication-failed",
                                "managed RPC requires the current private daemon credential",
                            )
                            .into());
                        }
                        if !legacy_protocol && let Some(object) = value.as_object_mut() {
                            object.remove("authorization");
                        }
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
                                instance: state.registration.instance.clone(),
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
            let mut pacer = IdlePacer::default();
            while !scheduler.stopping.load(Ordering::Acquire) {
                thread::sleep(SCHEDULER_TICK);
                let signature = (|| -> Result<(u64, u64, usize, usize)> {
                    let slots = scheduler.views.slots()?;
                    let epochs = slots.iter().fold(0_u64, |sum, slot| sum.wrapping_add(slot.input_epoch()));
                    let queued = scheduler.queued.lock().map_err(|_| Error::corrupt("job registry poisoned"))?.len();
                    Ok((scheduler.lifecycle_epoch.load(Ordering::Acquire), epochs, slots.len(), queued))
                })();
                let tick_started = Instant::now();
                if let Ok(signature) = signature
                    && !pacer.due(tick_started, signature)
                {
                    continue;
                }
                let mut quiet = signature.is_ok();
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
                        quiet = false;
                        recovery_schedule.request();
                        pending_schedule.request();
                        collection_requested = true;
                        collection_schedule.request();
                    }
                    // Each phase fails independently: unproductive or failing
                    // maintenance must never stop view reconciliation.
                    let mut failures = PhaseFailures::default();
                    if recovery_schedule.due(now, retry) {
                        match scheduler.namespace.recover_pass(recovery.clone()) {
                            Ok(progress) => {
                                recovered_changes |= recovery_productive(&progress);
                                recovery = progress.next;
                                recovery_schedule.completed(Instant::now(), recovered_changes, recovery.is_some());
                                if recovered_changes || recovery.is_some() { quiet = false; }
                                if recovery.is_none() { recovered_changes = false; }
                                for issue in progress.issues {
                                    eprintln!("managed recovery {} {}: {}", issue.kind, issue.identity, issue.error);
                                }
                            }
                            Err(error) => {
                                recovery_schedule.completed(Instant::now(), false, false);
                                failures.record("recovery", error.into());
                            }
                        }
                    }
                    if pending_schedule.due(now, Duration::from_millis(100)) {
                        let mut enqueued = false;
                        let result = (|| -> Result<()> {
                            let pending = scheduler.namespace.pending_operations(pending_after.as_ref())?;
                            for operation in &pending {
                                if operation.instance == *scheduler.namespace.instance() {
                                    let fresh = !scheduler.queued.lock().map_err(|_| Error::corrupt("job registry poisoned"))?
                                        .contains_key(&operation.id);
                                    scheduler.enqueue(operation)?;
                                    enqueued |= fresh;
                                }
                            }
                            pending_after = if pending.len() == policy.policy.work.page_objects as usize {
                                pending.last().map(|operation| operation.id.clone())
                            } else { None };
                            Ok(())
                        })();
                        // Re-enqueueing yielded or dropped work is progress: it
                        // must not grow the backoff for the next continuation.
                        let progress = enqueued || pending_after.is_some();
                        pending_schedule.completed(Instant::now(), progress, pending_after.is_some());
                        if progress { quiet = false; }
                        failures.check("pending", result);
                    }
                    match scheduler.views.drain_released_page(drain_after.as_ref()) {
                        Ok((drained, next)) => {
                            if drained != 0 || next.is_some() { quiet = false; }
                            drain_after = next;
                        }
                        Err(error) => { drain_after = None; failures.record("drain", error.into()); }
                    }
                    match scheduler.views.slots_page(view_after.as_ref()) {
                        Ok((slots, next)) => {
                            for slot in &slots {
                                match scheduler.schedule_view(slot) {
                                    Ok(active) => quiet &= !active,
                                    Err(error) => failures.record("views", error),
                                }
                            }
                            if next.is_some() { quiet = false; }
                            view_after = next;
                        }
                        Err(error) => { view_after = None; failures.record("views", error.into()); }
                    }
                    let mut collection_state = Value::Null;
                    if let Some(id) = collection.take() {
                        match scheduler.namespace.operation(&id) {
                            Err(error) => {
                                // An unreadable receipt must not keep
                                // collection marked in flight forever.
                                collection_schedule.completed(Instant::now(), false, false);
                                failures.record("collection", error.into());
                            }
                            Ok(previous) if !matches!(previous.state, OperationState::Completed | OperationState::Failed | OperationState::Cancelled) => {
                                // A durable receipt dropped by a full job
                                // queue is re-enqueued; enqueue is idempotent.
                                failures.check("collection", scheduler.enqueue(&previous));
                                collection_state = serde_json::to_value(previous.state)?;
                                collection = Some(id);
                            }
                            Ok(previous) => match previous.result.map(serde_json::from_value::<CollectionProgress>) {
                                Some(Ok(progress)) => {
                                    collection_changes |= progress.retired != 0 || progress.removed != 0 || progress.recovered_objects != 0
                                        || progress.logical_bytes_reclaimed != 0 || progress.recovered_logical_bytes != 0;
                                    collection_cursor = progress.next;
                                    collection_schedule.completed(Instant::now(), collection_changes, collection_cursor.is_some());
                                    if collection_cursor.is_none() { collection_changes = false; }
                                }
                                Some(Err(error)) => {
                                    collection_cursor = None;
                                    collection_schedule.completed(Instant::now(), false, false);
                                    failures.record("collection", error.into());
                                }
                                None => {
                                    collection_schedule.completed(Instant::now(), false, false);
                                    if let Some(error) = previous.error { eprintln!("managed collection failed: {error}"); }
                                }
                            },
                        }
                    }
                    let collection_queued = match &collection {
                        Some(id) => scheduler.queued.lock().map_err(|_| Error::corrupt("job registry poisoned"))?.contains_key(id),
                        None => false,
                    };
                    let collection_running = collection.as_ref().is_some_and(|id| scheduler.jobs.running(id));
                    let acknowledged = (|| -> Result<()> {
                        let queued = scheduler.queued.lock().map_err(|_| Error::corrupt("job registry poisoned"))?;
                        let mut protected: Vec<Id> = queued.keys().cloned().collect();
                        protected.extend(collection.iter().cloned());
                        scheduler.namespace.acknowledge_system_operations(&protected)?;
                        Ok(())
                    })();
                    failures.check("acknowledge", acknowledged);
                    let enabled = policy.policy.collection.on_pressure
                        || matches!(policy.policy.collection.schedule, managed::policy::Schedule::Periodic { .. });
                    let mut pressure = false;
                    let started_collection = (|| -> Result<()> {
                        let usage = scheduler.namespace.work_usage()?;
                        pressure = policy.policy.collection.on_pressure
                            && usage.storage.charged_budget_shortfall_bytes.is_some_and(|shortfall| shortfall != 0);
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
                                bounds: CollectionBounds::from_policy(&policy.policy.collection), cursor: collection_cursor.clone() };
                            // A failed acceptance backs off instead of being
                            // retried on every tick.
                            last_collection = Instant::now();
                            let operation = match scheduler.namespace.accept_system_operation(Token::parse(Id::new()?.to_string())?, "collection", serde_json::to_value(request)?) {
                                Ok(operation) => operation,
                                Err(error) => {
                                    collection_schedule.completed(Instant::now(), false, false);
                                    return Err(error.into());
                                }
                            };
                            collection_requested = false;
                            collection_cursor = None;
                            collection = Some(operation.id.clone());
                            scheduler.enqueue(&operation)?;
                        }
                        Ok(())
                    })();
                    failures.check("collection", started_collection);
                    if collection.is_some() || collection_cursor.is_some() || !failures.names().is_empty() { quiet = false; }
                    let phase_failures = failures.names();
                    *scheduler.scheduler_snapshot.lock().map_err(|_| Error::corrupt("scheduler observations poisoned"))? = json!({
                        "initializing":false,"tick_ms":SCHEDULER_TICK.as_millis(),"idle_tick_ms":SCHEDULER_IDLE_TICK.as_millis(),"maximum_backoff_multiplier":64,
                        "automatic_collection_enabled":enabled,"pressure":pressure,
                        "recovery_continuation":recovery.is_some(),"collection_continuation":collection_cursor.is_some(),
                        // In flight spans acceptance to completion, including
                        // time queued. collection_running means a worker is
                        // executing a pass, as maintenance in_flight counts.
                        "collection_in_flight":collection.is_some(),
                        "collection_operation_state":collection_state,
                        "collection_queued":collection_queued,
                        "collection_running":collection_running,
                        "failed_phases":phase_failures,
                        "recovery_retry_ms":recovery_schedule.delay(retry).as_millis(),
                        "collection_retry_ms":collection_schedule.delay(retry).as_millis(),
                        "pending_retry_ms":pending_schedule.delay(Duration::from_millis(100)).as_millis()
                    });
                    failures.into_result()
                })();
                if let Ok(signature) = signature {
                    pacer.completed(tick_started, signature, quiet && tick.is_ok());
                }
                let mut diagnostics = scheduler.scheduler_diagnostics.lock().expect("scheduler diagnostics");
                if diagnostics.observe(started.elapsed(), tick.err().map(|error| format!("{error:#}"))) {
                    eprintln!("managed scheduler: {}", diagnostics.last_error_detail.as_deref().expect("observed error"));
                }
            }
        });
        // `accept` blocks, so once `stopping` is set a loopback connection wakes
        // it. Retry until the accept loop has returned; hold the wake open (write
        // side shut) so the loop's busy reply is not reset and no reader waits.
        let waker = Arc::clone(&state);
        let accepting = &accepting;
        scope.spawn(move || {
            let address = SocketAddr::from((Ipv4Addr::LOCALHOST, waker.registration.port));
            let mut wake = None;
            let mut reported = false;
            while accepting.load(Ordering::Acquire) {
                if wake.is_none() && waker.stopping.load(Ordering::Acquire) {
                    match TcpStream::connect_timeout(&address, Duration::from_secs(1)) {
                        Ok(stream) => {
                            let _ = stream.shutdown(std::net::Shutdown::Write);
                            wake = Some(stream);
                        }
                        Err(error) if !reported => {
                            eprintln!("managed daemon retrying listener wake: {error}");
                            reported = true;
                        }
                        Err(_) => {}
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        let mut accept_failures = super::AcceptRetry::new();
        while !state.stopping.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    accept_failures.accepted();
                    // One unusable socket is dropped; it must not end the daemon.
                    let configured = stream
                        .set_nonblocking(false)
                        .and_then(|()| stream.set_read_timeout(Some(Duration::from_secs(5))))
                        .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(5))));
                    if let Err(error) = configured {
                        eprintln!("managed daemon dropped an unconfigurable connection: {error}");
                        continue;
                    }
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
                Err(error) => {
                    if let Err(error) = accept_failures.failed(error) {
                        state.stopping.store(true, Ordering::Release);
                        return Err(error.into());
                    }
                }
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod watch_tests {
    use super::*;
    use std::fs;

    fn repository() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repository");
        let storage = temp.path().join("storage");
        fs::create_dir_all(root.join(".git").join("refs")).unwrap();
        fs::create_dir_all(root.join(".git").join("info")).unwrap();
        fs::create_dir(&storage).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let storage = fs::canonicalize(storage).unwrap();
        (temp, root, storage)
    }

    #[test]
    fn recursive_roots_do_not_spend_the_budget_on_descendants() {
        let (temp, root, storage) = repository();
        let budget = 8192 / 32;
        for index in 0..budget + 64 {
            fs::create_dir(root.join(format!("directory-{index}"))).unwrap();
        }
        let (directories, metadata) = monitor_directories(
            &root,
            &storage,
            budget,
            notify::RecursiveMode::Recursive,
            Instant::now() + Duration::from_secs(60),
        )
        .expect("recursive roots must not exhaust the descendant count budget");
        assert_eq!(directories, HashSet::from([root.clone()]));
        assert_eq!(metadata, vec![root.join(".git")]);
        temp.close().unwrap();
    }

    #[test]
    fn recursive_roots_do_not_require_a_descendant_traversal_deadline() {
        let (temp, root, storage) = repository();
        let (directories, _) = monitor_directories(
            &root,
            &storage,
            8192 / 32,
            notify::RecursiveMode::Recursive,
            Instant::now(),
        )
        .expect("recursive roots must not depend on the traversal deadline");
        assert_eq!(directories, HashSet::from([root]));
        temp.close().unwrap();
    }

    #[test]
    fn per_directory_planning_retains_count_and_traversal_limits() {
        let (temp, root, storage) = repository();
        let budget = 8192 / 32;
        for index in 0..budget + 64 {
            fs::create_dir(root.join(format!("directory-{index}"))).unwrap();
        }
        let count = monitor_directories(
            &root,
            &storage,
            budget,
            notify::RecursiveMode::NonRecursive,
            Instant::now() + Duration::from_secs(60),
        )
        .unwrap_err();
        assert_eq!(count.to_string(), "native watch count budget; polling");
        let deadline = monitor_directories(
            &root,
            &storage,
            budget,
            notify::RecursiveMode::NonRecursive,
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(
            deadline.to_string(),
            "native watch registration budget; polling"
        );
        temp.close().unwrap();
    }

    #[test]
    fn linked_metadata_is_covered_without_duplicate_recursive_subscriptions() {
        let (temp, root, storage) = repository();
        let linked = temp.path().join("linked");
        let common = root.join(".git");
        let private = common.join("worktrees").join("linked");
        fs::create_dir(&linked).unwrap();
        fs::create_dir_all(&private).unwrap();
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", private.display()),
        )
        .unwrap();
        fs::write(private.join("commondir"), format!("{}\n", common.display())).unwrap();
        let linked = fs::canonicalize(linked).unwrap();
        let (directories, metadata) = monitor_directories(
            &linked,
            &storage,
            2,
            notify::RecursiveMode::Recursive,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(directories, HashSet::from([linked.clone(), common.clone()]));
        assert_eq!(metadata, vec![private, common]);
        let error = monitor_directories(
            &linked,
            &storage,
            1,
            notify::RecursiveMode::Recursive,
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "native watch count budget; polling");
        temp.close().unwrap();
    }

    #[test]
    fn recursive_roots_still_reject_non_directory_git_watch_inputs() {
        let (temp, root, storage) = repository();
        fs::remove_dir(root.join(".git").join("refs")).unwrap();
        fs::write(root.join(".git").join("refs"), "not a directory").unwrap();
        let error = monitor_directories(
            &root,
            &storage,
            8192 / 32,
            notify::RecursiveMode::Recursive,
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "non-directory Git watch input; polling");
        temp.close().unwrap();
    }

    #[test]
    fn native_registration_mode_matches_the_backend_resource_model() {
        let settings = WatchSettings {
            no_watch: false,
            mode: crate::serve::WatchMode::Auto,
            interval: Duration::from_secs(120),
            watches_per_view: 8192 / 32,
        };
        assert_eq!(
            matches!(
                settings.registration_mode(),
                notify::RecursiveMode::Recursive
            ),
            cfg!(any(windows, target_os = "macos"))
        );
    }
}

#[cfg(test)]
mod scheduling_tests {
    use super::*;

    #[test]
    #[cfg(feature = "managed-test-hooks")]
    fn nested_git_metadata_events_use_the_most_specific_root_in_either_order() {
        use std::fs;
        use std::process::{Command, Stdio};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repository");
        let linked = temp.path().join("linked");
        let storage = temp.path().join("storage");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&storage).unwrap();
        let git = |args: &[&str]| {
            let mut command = Command::new("git");
            command
                .current_dir(&root)
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
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .stdin(Stdio::null())
                .stdout(Stdio::null());
            let mut child = managed::SupervisedChild::spawn(&mut command).unwrap();
            assert!(child.wait().unwrap().success(), "git {args:?}");
        };
        git(&["init", "--quiet"]);
        fs::write(root.join("source.txt"), "shared_term\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", "initial"]);
        git(&[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
        ]);
        let repository = Repository::discover(&linked).unwrap();
        let policy: Policy = serde_json::from_value(json!({
            "schema":2,"storage":"managed","retention":{"mode":"retain-all"},"advancement":{"mode":"fixed"},
            "work":{
                "max_views":8,"max_leases":32,"workers":2,"queue_items":16,
                "staging_bytes":67108864,"private_work_bytes":67108864,
                "sort_buffer_bytes":1048576,"blob_bytes":1048576,"operation_timeout_ms":30000,
                "page_objects":16,"max_cursors":8,"cursor_lifetime_ms":30000,"max_receipts":1024,
                "metadata_bytes":16777216
            },
            "collection":{
                "schedule":{"mode":"disabled"},"on_pressure":false,
                "checkpoint_grace_ms":0,"generation_grace_ms":0,
                "max_duration_ms":1000,"max_examined":64,"max_removed":16,"max_delete_bytes":1048576,
                "chunk_bytes":65536,"max_pages":4,"retry_ms":100
            }
        })).unwrap();
        let namespace = Namespace::initialize(&repository, &storage, policy).unwrap();
        let prepared = namespace.prepare_owner().unwrap();
        let owner = managed::OwnerGuard::claim(prepared.claim).unwrap();
        namespace.register_owner(owner.registration()).unwrap();
        let manager = ViewManager::new(
            Arc::clone(&namespace),
            None,
            WorktreeOptions::default(),
            None,
        )
        .unwrap();
        let operation = manager
            .accept_attach(
                OperationToken {
                    scope: owner.registration().owner.clone(),
                    sequence: 1,
                    token: Token::parse("watch-attach").unwrap(),
                },
                AttachRequest {
                    root: linked.clone(),
                    revision: Some("HEAD".into()),
                    profile: IndexingProfile::default(),
                    lease: Token::parse("watch-client").unwrap(),
                    owner: owner.registration().owner.clone(),
                    accept_current: None,
                    migratable: true,
                    allocation_version: 1,
                },
            )
            .unwrap();
        let completed = manager.execute(&operation.id).unwrap();
        assert_eq!(completed.state, OperationState::Completed, "{completed:?}");
        let record: managed::ViewRecord =
            serde_json::from_value(completed.result.unwrap()["current"].clone()).unwrap();
        assert!(record.pin().unwrap().incarnation.is_some());
        assert!(manager.status(&record.id).unwrap().ready);
        let slot = manager.slot(&record.id).unwrap();
        let failure = Mutex::new(None);
        let counters = WatchCounters::default();
        let source = slot.root().join("source.txt");
        let modify =
            |kind| Ok(notify::Event::new(EventKind::Modify(kind)).add_path(source.clone()));
        let before = slot.input_epoch();
        for kind in [
            notify::event::ModifyKind::Any,
            notify::event::ModifyKind::Metadata(notify::event::MetadataKind::Any),
        ] {
            watch_event(&slot, &[], &storage, &failure, &counters, modify(kind)).unwrap();
        }
        assert_eq!(
            slot.input_epoch(),
            before,
            "metadata-only churn on an indexed file must not advance the epoch"
        );
        assert!(manager.status(&record.id).unwrap().ready);
        watch_event(
            &slot,
            &[],
            &storage,
            &failure,
            &counters,
            modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Any,
            )),
        )
        .unwrap();
        assert_eq!(
            slot.input_epoch(),
            before + 1,
            "renames are always rechecked"
        );
        let events = counters.snapshot();
        assert_eq!(events["events"], 3);
        assert_eq!(events["by_kind"]["modify"], 3);
        assert_eq!(events["unchanged_paths_filtered"], 2);
        assert_eq!(events["invalidated_paths"], 1);
        assert_eq!(events["epoch_advances"], 1);
        let (private, common) =
            tgrep_core::git_index::read_repository_dirs_bounded(&linked).unwrap();
        let private = fs::canonicalize(private).unwrap();
        let common = fs::canonicalize(common).unwrap();
        assert_ne!(private, common);
        assert!(private.starts_with(&common));
        for roots in [
            [common.clone(), private.clone()],
            [private.clone(), common.clone()],
        ] {
            for directory in [&private, &common] {
                for name in [
                    "HEAD",
                    "index",
                    "config",
                    "config.worktree",
                    "commondir",
                    "info",
                    "refs/heads/own-branch",
                ] {
                    let before = slot.input_epoch();
                    watch_event(
                        &slot,
                        &roots,
                        &storage,
                        &failure,
                        &counters,
                        Ok(
                            notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                                .add_path(directory.join(name)),
                        ),
                    )
                    .unwrap();
                    assert_eq!(
                        slot.input_epoch(),
                        before + 1,
                        "ignored {directory:?}/{name}; roots {roots:?}"
                    );
                    assert!(!manager.status(&record.id).unwrap().ready);
                }
                for name in [
                    "HEAD.lock",
                    "index.lock",
                    "config.lock",
                    "config.worktree.lock",
                    "objects",
                    "logs",
                    "packed-refs",
                    "refs",
                    "refs/prefetch/heads/main",
                    "FETCH_HEAD",
                    "ORIG_HEAD",
                ] {
                    let before = slot.input_epoch();
                    watch_event(
                        &slot,
                        &roots,
                        &storage,
                        &failure,
                        &counters,
                        Ok(
                            notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                                .add_path(directory.join(name)),
                        ),
                    )
                    .unwrap();
                    assert_eq!(
                        slot.input_epoch(),
                        before,
                        "unexpected invalidation for {name}; roots {roots:?}"
                    );
                }
            }
            let before = slot.input_epoch();
            watch_event(
                &slot,
                &roots,
                &storage,
                &failure,
                &counters,
                Ok(
                    notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                        .add_path(common.join("worktrees").join("unrelated").join("HEAD")),
                ),
            )
            .unwrap();
            assert_eq!(slot.input_epoch(), before);
            watch_event(
                &slot,
                &roots,
                &storage,
                &failure,
                &counters,
                Ok(
                    notify::Event::new(EventKind::Access(notify::event::AccessKind::Any))
                        .add_path(private.join("HEAD")),
                ),
            )
            .unwrap();
            assert_eq!(slot.input_epoch(), before);
            for path in [
                storage.join("checkpoint"),
                slot.root().join(".tgrep").join("index"),
            ] {
                watch_event(
                    &slot,
                    &roots,
                    &storage,
                    &failure,
                    &counters,
                    Ok(
                        notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                            .add_path(path),
                    ),
                )
                .unwrap();
                assert_eq!(slot.input_epoch(), before);
            }
        }
        assert!(failure.lock().unwrap().is_none());
        let before = slot.input_epoch();
        watch_event(
            &slot,
            &[common, private],
            &storage,
            &failure,
            &counters,
            Ok(
                notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                    .add_path(storage.join("checkpoint"))
                    .set_flag(notify::event::Flag::Rescan),
            ),
        )
        .unwrap();
        assert_eq!(slot.input_epoch(), before + 1);
        watch_event(
            &slot,
            &[],
            &storage,
            &failure,
            &counters,
            Err(notify::Error::new(notify::ErrorKind::MaxFilesWatch)),
        )
        .unwrap();
        assert_eq!(slot.input_epoch(), before + 2);
        assert!(
            failure
                .into_inner()
                .unwrap()
                .unwrap()
                .starts_with("native watcher failed:")
        );
        drop((slot, manager, owner, namespace, repository));
        temp.close().unwrap();
    }

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
    fn verification_cadence_bounds_idle_work_by_its_observed_cost() {
        let interval = Duration::from_secs(120);
        assert_eq!(verification_delay(interval, Duration::ZERO), interval);
        assert_eq!(
            verification_delay(interval, Duration::from_secs(10)),
            Duration::from_secs(190)
        );
        let content_interval = interval.saturating_mul(REREAD_INTERVALS);
        assert_eq!(
            verification_delay(content_interval, Duration::from_secs(10)),
            Duration::from_secs(1800)
        );
        let cost = Duration::from_secs(165);
        let idle = verification_delay(content_interval, cost);
        assert_eq!(idle, Duration::from_secs(3135));
        assert_eq!(cost.as_secs() * 20, (idle + cost).as_secs());
        assert_eq!(verification_delay(interval, Duration::MAX), Duration::MAX);
    }

    #[test]
    fn content_verification_retries_wait_since_the_last_terminal_attempt() {
        let interval = Duration::from_secs(120);
        let cost = Duration::from_secs(165);
        let old_success = Duration::from_secs(86_400);
        let retry_delay = verification_delay(interval, cost);
        assert_eq!(retry_delay, Duration::from_secs(3135));
        for attempt_elapsed in [interval, retry_delay - Duration::from_millis(1)] {
            assert!(!content_verification_due(
                interval,
                cost,
                interval,
                old_success,
                attempt_elapsed,
            ));
        }
        assert!(content_verification_due(
            interval,
            cost,
            interval,
            old_success,
            retry_delay,
        ));
        assert!(!content_verification_due(
            interval,
            cost,
            interval - Duration::from_millis(1),
            old_success,
            retry_delay,
        ));

        let cheap = Duration::from_secs(1);
        assert!(content_verification_due(
            interval,
            cheap,
            interval,
            old_success,
            interval,
        ));
        assert!(!content_verification_due(
            interval, cheap, interval, interval, interval,
        ));
        let content_interval = interval.saturating_mul(REREAD_INTERVALS);
        assert!(content_verification_due(
            interval,
            cheap,
            content_interval,
            content_interval,
            content_interval,
        ));
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
            base * 64,
            "unproductive continuations keep the backoff"
        );
        schedule.completed(now + base, true, true);
        assert_eq!(
            schedule.delay(base),
            base,
            "productive bounded traversal continuations should progress"
        );
        schedule.completed(now + base, false, true);
        assert_eq!(
            schedule.delay(base),
            base,
            "a traversal that started productive continues at the base interval"
        );
        schedule.completed(now + base, false, false);
        assert_eq!(schedule.delay(base), base * 2);
        schedule.completed(now + base, false, true);
        assert_eq!(
            schedule.delay(base),
            base * 2,
            "a repeated unproductive traversal is paced at its backoff"
        );
    }

    fn recovery(
        cleanup: managed::CleanupCounts,
        objects_retired: u32,
    ) -> managed::RecoveryProgress {
        serde_json::from_value(json!({
            "examined":100,"owners_reaped":0,"reservations_released":0,"members_refreshed":0,
            "objects_retired":objects_retired,"objects_quarantined":0,"views_retired":0,
            "operations_recovered":0,"cleanup":cleanup,"cancelled":false,
            "measurements_complete":true,"mutation_committed":true,"issues":[],"next":null,
            "elapsed_nanos":1
        }))
        .unwrap()
    }

    #[test]
    fn recovery_that_only_verifies_or_commits_flags_backs_off() {
        let verified = managed::CleanupCounts {
            control_verification_bytes: 4096,
            ..Default::default()
        };
        assert!(!recovery_productive(&recovery(verified.clone(), 0)));
        assert!(recovery_productive(&recovery(verified, 1)));
        assert!(recovery_productive(&recovery(
            managed::CleanupCounts {
                receipts: 1,
                ..Default::default()
            },
            0
        )));
    }

    #[test]
    fn scheduler_phase_failures_do_not_hide_each_other() {
        let mut failures = PhaseFailures::default();
        failures.check("recovery", Ok(()));
        failures.check("recovery", Err(anyhow::anyhow!("stuck")));
        failures.record("collection", anyhow::anyhow!("busy"));
        assert_eq!(failures.names(), ["recovery", "collection"]);
        let error = format!("{:#}", failures.into_result().unwrap_err());
        assert!(error.contains("recovery: stuck"), "{error}");
        assert!(error.contains("collection: busy"), "{error}");
        assert!(PhaseFailures::default().into_result().is_ok());
    }

    #[test]
    fn watch_counters_report_kinds_and_filtered_paths() {
        let counters = WatchCounters::default();
        WatchCounters::add(&counters.events, 3);
        WatchCounters::add(&counters.modify, 2);
        WatchCounters::add(&counters.errors, 1);
        WatchCounters::add(&counters.unchanged_paths, 5);
        let snapshot = counters.snapshot();
        assert_eq!(snapshot["events"], 3);
        assert_eq!(snapshot["by_kind"]["modify"], 2);
        assert_eq!(snapshot["by_kind"]["error"], 1);
        assert_eq!(snapshot["unchanged_paths_filtered"], 5);
        assert_eq!(snapshot["full_invalidations"], 0);
    }

    fn job_ids() -> [Id; 4] {
        std::array::from_fn(|index| Id::parse(format!("{index:032x}")).unwrap())
    }

    #[test]
    fn job_queue_runs_the_earliest_due_job_across_classes() {
        let [verify, collection, client, late] = job_ids();
        let base = Instant::now();
        let mut queue = JobQueueState::default();
        assert!(queue.push(verify.clone(), JobClass::Background, base, 8));
        let second = base + Duration::from_secs(1);
        assert!(queue.push(collection.clone(), JobClass::Maintenance, second, 8));
        let later = base + Duration::from_secs(30);
        assert!(queue.push(client.clone(), JobClass::Foreground, later, 8));
        // Collection became due at 6 s, before the client operation queued at 30 s.
        assert_eq!(queue.pop(), Some((collection, JobClass::Maintenance)));
        assert_eq!(queue.pop(), Some((client, JobClass::Foreground)));
        // Verification became due at 600 s, ahead of foreground work queued later.
        let after = base + Duration::from_secs(601);
        assert!(queue.push(late.clone(), JobClass::Foreground, after, 8));
        assert_eq!(queue.pop(), Some((verify, JobClass::Background)));
        assert_eq!(queue.pop(), Some((late, JobClass::Foreground)));
        assert_eq!(queue.pop(), None);
        assert!(queue.is_empty());
    }

    #[test]
    fn job_queue_breaks_ties_toward_higher_classes_and_bounds_capacity() {
        let [collection, first, second, verify] = job_ids();
        let base = Instant::now();
        let tie = base + JobClass::Maintenance.delay();
        let mut queue = JobQueueState::default();
        assert!(queue.push(collection.clone(), JobClass::Maintenance, base, 4));
        assert!(queue.push(first.clone(), JobClass::Foreground, tie, 4));
        assert!(queue.push(second.clone(), JobClass::Foreground, tie, 4));
        assert!(queue.push(verify.clone(), JobClass::Background, base, 4));
        assert_eq!(queue.len(), 4);
        let overflow = Id::parse("f".repeat(32)).unwrap();
        assert!(!queue.push(overflow, JobClass::Foreground, tie, 4));
        assert_eq!(queue.pop(), Some((first, JobClass::Foreground)));
        assert_eq!(queue.pop(), Some((second, JobClass::Foreground)));
        assert_eq!(queue.pop(), Some((collection, JobClass::Maintenance)));
        assert_eq!(queue.pop(), Some((verify, JobClass::Background)));
        assert!(queue.is_empty());
    }

    #[test]
    fn job_queue_promotion_keeps_the_original_queue_time() {
        let [early, verify, late, unknown] = job_ids();
        let base = Instant::now();
        let mut queue = JobQueueState::default();
        assert!(queue.push(early.clone(), JobClass::Foreground, base, 8));
        let second = base + Duration::from_secs(1);
        assert!(queue.push(verify.clone(), JobClass::Background, second, 8));
        let third = base + Duration::from_secs(2);
        assert!(queue.push(late.clone(), JobClass::Foreground, third, 8));
        assert!(!queue.promote(&unknown, JobClass::Foreground));
        assert!(queue.promote(&verify, JobClass::Foreground));
        assert!(!queue.promote(&verify, JobClass::Foreground));
        assert!(!queue.promote(&early, JobClass::Background));
        assert_eq!(queue.pop(), Some((early, JobClass::Foreground)));
        assert_eq!(queue.pop(), Some((verify, JobClass::Foreground)));
        assert_eq!(queue.pop(), Some((late, JobClass::Foreground)));
        assert!(queue.is_empty());
    }

    #[test]
    fn job_classes_follow_operation_kind_and_origin() {
        let verify = json!({"verify": true});
        let plain = json!({});
        assert_eq!(
            JobClass::classify("reconcile", &verify, true),
            JobClass::Background
        );
        assert_eq!(
            JobClass::classify("reconcile", &plain, true),
            JobClass::Foreground
        );
        assert_eq!(
            JobClass::classify("adaptive", &plain, true),
            JobClass::Background
        );
        assert_eq!(
            JobClass::classify("adaptive", &plain, false),
            JobClass::Foreground
        );
        for automatic in [false, true] {
            assert_eq!(
                JobClass::classify("collection", &plain, automatic),
                JobClass::Maintenance
            );
        }
        for kind in ["attach", "legacy-attach", "migrate", "refresh", "metadata"] {
            assert_eq!(
                JobClass::classify(kind, &verify, false),
                JobClass::Foreground
            );
        }
    }

    #[test]
    fn job_queue_reports_queued_and_running_work() {
        let [first, second, third, absent] = job_ids();
        let queue = JobQueue::new(2);
        assert!(queue.pop(Duration::from_millis(1)).is_none());
        assert!(queue.push(first.clone(), JobClass::Background));
        assert!(queue.push(second.clone(), JobClass::Foreground));
        assert!(!queue.push(third, JobClass::Foreground));
        let running = queue.pop(Duration::from_millis(1)).unwrap();
        assert_eq!(running.id(), &second);
        assert!(queue.running(&second));
        assert!(!queue.running(&first));
        let snapshot = queue.snapshot();
        assert_eq!(snapshot["capacity"], 2);
        assert_eq!(snapshot["queued"]["foreground"]["depth"], 0);
        assert!(snapshot["queued"]["foreground"]["oldest_wait_ms"].is_null());
        assert_eq!(snapshot["queued"]["maintenance"]["depth"], 0);
        assert_eq!(snapshot["queued"]["background"]["depth"], 1);
        assert!(snapshot["queued"]["background"]["oldest_wait_ms"].is_u64());
        assert_eq!(snapshot["running"][0]["operation"], second.to_string());
        assert_eq!(snapshot["running"][0]["class"], "foreground");
        let described = queue.describe(&[&second, &first, &absent]);
        assert_eq!(described[0]["state"], "running");
        assert_eq!(described[0]["class"], "foreground");
        assert_eq!(described[1]["state"], "queued");
        assert_eq!(described[1]["class"], "background");
        assert_eq!(
            described[2],
            json!({"operation": absent, "state": "accepted"})
        );
        drop(running);
        assert!(!queue.running(&second));
        assert_eq!(queue.snapshot()["running"], json!([]));
    }
}

#[cfg(test)]
mod candidate_read_tests {
    use super::*;
    use std::cell::Cell;
    use std::process::Command;

    fn git(root: &Path, arguments: &[&str]) {
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
    }

    /// Call 0 lists candidates, 1 pins their parent, and 2 checks the final
    /// epoch; sibling opens take no snapshot.
    #[derive(Clone, Copy, Debug)]
    enum Inject {
        Nothing,
        SnapshotBeforeRead(usize),
        SnapshotAfterRead(usize),
        /// Fail `check` calls numbered from this one with `query-deadline`.
        CheckFrom(usize),
        /// Swap this root-relative directory for a copy without `shared_term`
        /// after the call completes, so a pinned batch reads the old one.
        #[cfg(unix)]
        ReplaceAfter(usize, &'static str),
    }

    /// Wraps the real managed source, injecting the typed failures that
    /// `ViewQuery` validation reports, and counts view invalidations.
    struct Scripted {
        inner: ManagedQuery,
        inject: Inject,
        snapshots: Cell<usize>,
        checks: Cell<usize>,
        invalidations: Cell<usize>,
        requeues: Cell<usize>,
    }

    impl QuerySource for Scripted {
        fn root(&self) -> &Path {
            self.inner.root()
        }
        fn eligibility(&self) -> Result<ContentEligibility> {
            self.inner.eligibility()
        }
        fn snapshot<T>(&self, read: impl FnOnce(WorktreeSnapshot<'_>) -> T) -> Result<T> {
            let call = self.snapshots.get();
            self.snapshots.set(call + 1);
            match self.inject {
                Inject::SnapshotBeforeRead(at) if at == call => {
                    Err(Error::busy("query-input-changed").into())
                }
                Inject::SnapshotAfterRead(at) if at == call => {
                    let _opened = self.inner.snapshot(read)?;
                    Err(Error::busy("query-snapshot-changed").into())
                }
                #[cfg(unix)]
                Inject::ReplaceAfter(at, directory) if at == call => {
                    let opened = self.inner.snapshot(read)?;
                    let directory = self.root().join(directory);
                    let saved = directory.with_file_name("replaced-original");
                    std::fs::rename(&directory, &saved).unwrap();
                    std::fs::create_dir(&directory).unwrap();
                    for entry in std::fs::read_dir(&saved).unwrap() {
                        let name = entry.unwrap().file_name();
                        std::fs::write(directory.join(name), "replacement\n").unwrap();
                    }
                    Ok(opened)
                }
                _ => self.inner.snapshot(read),
            }
        }
        fn invalidate(&self, error: &anyhow::Error) -> Result<()> {
            self.invalidations.set(self.invalidations.get() + 1);
            self.inner.invalidate(error)
        }
        fn check(&self) -> Result<()> {
            let call = self.checks.get();
            self.checks.set(call + 1);
            if let Inject::CheckFrom(from) = self.inject
                && call >= from
            {
                return Err(Error::new(
                    ErrorCategory::Deadline,
                    "query-deadline",
                    "query time budget exhausted",
                )
                .into());
            }
            self.inner.check()
        }
        fn eventual(&self) -> bool {
            self.inner.eventual()
        }
        fn requeue(&self, relative: &[String]) -> Result<()> {
            self.requeues.set(self.requeues.get() + relative.len());
            self.inner.requeue(relative)
        }
    }

    #[test]
    fn only_candidate_read_failures_close_a_ready_managed_view() {
        candidate_read_failures(false);
    }

    #[test]
    fn eventual_views_skip_and_requeue_failed_candidates() {
        candidate_read_failures(true);
    }

    #[test]
    fn page_deadline_returns_the_searched_prefix_and_a_cursor() {
        let fixture = Fixture::new(false, &["sibling.txt", "source.txt"]);
        let page = |page: Value, page_time: Duration| {
            super::super::server::search_snapshot(
                &fixture.source(Inject::Nothing),
                &json!({"pattern":"shared_term","page":page}),
                false,
                &json!(1),
                json!({}),
                page_time,
            )
            .unwrap()
        };
        let files = |result: &Value| -> Vec<String> {
            result["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["file"].as_str().unwrap().to_owned())
                .collect()
        };
        let whole = page(Value::Null, super::super::server::PAGE_TIME);
        assert_eq!(files(&whole), ["sibling.txt", "source.txt"], "{whole}");
        assert_eq!(whole["next_page"], Value::Null);
        // An expired deadline stops the sibling group at a file boundary;
        // each page still searches one file.
        let first = page(Value::Null, Duration::ZERO);
        assert_eq!(files(&first), ["sibling.txt"], "{first}");
        assert_eq!(first["next_page"], "source.txt");
        let last = page(first["next_page"].clone(), Duration::ZERO);
        assert_eq!(files(&last), ["source.txt"], "{last}");
        assert_eq!(last["next_page"], Value::Null);
        assert!(fixture.ready());
        fixture.close();
    }

    /// Records the threads that take snapshots of the view and check reads.
    struct ThreadRecording {
        inner: ManagedQuery,
        threads: Mutex<Vec<String>>,
        checks: Mutex<Vec<String>>,
    }

    impl QuerySource for ThreadRecording {
        fn root(&self) -> &Path {
            self.inner.root()
        }
        fn eligibility(&self) -> Result<ContentEligibility> {
            self.inner.eligibility()
        }
        fn snapshot<T>(&self, read: impl FnOnce(WorktreeSnapshot<'_>) -> T) -> Result<T> {
            let name = thread::current().name().unwrap_or_default().to_owned();
            self.threads.lock().unwrap().push(name);
            self.inner.snapshot(read)
        }
        fn invalidate(&self, error: &anyhow::Error) -> Result<()> {
            self.inner.invalidate(error)
        }
        fn check(&self) -> Result<()> {
            let name = thread::current().name().unwrap_or_default().to_owned();
            self.checks.lock().unwrap().push(name);
            self.inner.check()
        }
        fn eventual(&self) -> bool {
            self.inner.eventual()
        }
        fn requeue(&self, relative: &[String]) -> Result<()> {
            self.inner.requeue(relative)
        }
    }

    #[test]
    fn parallel_queries_run_on_the_dedicated_search_pool() {
        // Two parent directories make two sibling groups, so the search forks,
        // and two files per group make each group pin its parent first.
        let fixture = Fixture::new(false, &["one/a.txt", "one/b.txt", "two/c.txt", "two/d.txt"]);
        let source = ThreadRecording {
            inner: ManagedQuery {
                query: fixture.slot.query(fixture.record.version).unwrap(),
                slot: Arc::clone(&fixture.slot),
            },
            threads: Mutex::new(Vec::new()),
            checks: Mutex::new(Vec::new()),
        };
        let caller = thread::current().name().unwrap_or_default().to_owned();
        let result = super::super::server::search_snapshot_parallel(
            &source,
            &json!({"pattern":"shared_term"}),
            false,
            &json!(1),
            json!({}),
            super::super::server::PAGE_TIME,
        )
        .unwrap();
        assert_eq!(result["matches"].as_array().unwrap().len(), 4, "{result}");
        let threads = std::mem::take(&mut *source.threads.lock().unwrap());
        let checks = std::mem::take(&mut *source.checks.lock().unwrap());
        // Each group's parent pin and every read check run on the query pool,
        // never on the global pool that reconciliation batches occupy. Sibling
        // opens take no snapshot, so each group records only its pin. The
        // listing and the final epoch check stay on the calling thread,
        // because only each window is installed on the pool.
        let (listing, rest) = threads.split_first().unwrap();
        let (last, groups) = rest.split_last().unwrap();
        assert_eq!((listing, last), (&caller, &caller), "{threads:?}");
        assert!(!caller.starts_with("tgrep-search-"), "{threads:?}");
        assert_eq!(groups.len(), 2, "{threads:?}");
        assert!(checks.len() >= 4, "{checks:?}");
        assert!(
            groups
                .iter()
                .chain(&checks)
                .all(|name| name.starts_with("tgrep-search-")),
            "{threads:?} {checks:?}"
        );
        drop(source);
        fixture.close();
    }

    #[cfg(unix)]
    #[test]
    fn eventual_views_discard_a_group_read_across_a_replaced_parent() {
        let fixture = Fixture::new(true, &["group/one.txt", "group/two.txt"]);
        let epoch = fixture.slot.input_epoch();
        let source = fixture.source(Inject::ReplaceAfter(1, "group"));
        let result = super::super::server::search_snapshot(
            &source,
            &json!({"pattern":"shared_term"}),
            false,
            &json!(1),
            json!({}),
            super::super::server::PAGE_TIME,
        )
        .unwrap();
        // The pinned batch read the original `shared_term` contents, but its
        // parent changed, so those rows are dropped and both paths requeued.
        assert_eq!(result["matches"], json!([]), "{result}");
        assert_eq!((source.invalidations.get(), source.requeues.get()), (0, 2));
        assert_eq!(fixture.slot.input_epoch(), epoch + 1);
        assert!(!fixture.ready());
        drop(source);
        fixture.close();
    }

    /// A managed view over committed `files`, each containing `shared_term`.
    /// Eventual views finish their first verification, so both start ready.
    struct Fixture {
        temp: tempfile::TempDir,
        root: PathBuf,
        repository: Repository,
        namespace: Arc<Namespace>,
        owner: managed::OwnerGuard,
        manager: ViewManager,
        record: managed::ViewRecord,
        slot: Arc<ViewSlot>,
    }

    impl Fixture {
        fn new(stale_reads: bool, files: &[&str]) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("repository");
            let storage = temp.path().join("storage");
            std::fs::create_dir(&root).unwrap();
            std::fs::create_dir(&storage).unwrap();
            git(&root, &["init", "--quiet"]);
            for file in files {
                let path = root.join(file);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, format!("shared_term {file}\n")).unwrap();
            }
            git(&root, &["add", "."]);
            git(&root, &["commit", "--quiet", "-m", "initial"]);
            let repository = Repository::discover(&root).unwrap();
            let policy: Policy = serde_json::from_value(json!({
                "schema":2,"storage":"managed","retention":{"mode":"retain-all"},"advancement":{"mode":"fixed"},
                "work":{
                    "max_views":8,"max_leases":32,"workers":2,"queue_items":16,
                    "staging_bytes":67108864,"private_work_bytes":67108864,
                    "sort_buffer_bytes":1048576,"blob_bytes":1048576,"operation_timeout_ms":30000,
                    "page_objects":16,"max_cursors":8,"cursor_lifetime_ms":30000,"max_receipts":1024,
                    "metadata_bytes":16777216
                },
                "collection":{
                    "schedule":{"mode":"disabled"},"on_pressure":false,
                    "checkpoint_grace_ms":0,"generation_grace_ms":0,
                    "max_duration_ms":1000,"max_examined":64,"max_removed":16,"max_delete_bytes":1048576,
                    "chunk_bytes":65536,"max_pages":4,"retry_ms":100
                }
            }))
            .unwrap();
            let namespace = Namespace::initialize(&repository, &storage, policy).unwrap();
            let prepared = namespace.prepare_owner().unwrap();
            let owner = managed::OwnerGuard::claim(prepared.claim).unwrap();
            namespace.register_owner(owner.registration()).unwrap();
            let manager = ViewManager::new(
                Arc::clone(&namespace),
                None,
                WorktreeOptions {
                    stale_reads,
                    ..WorktreeOptions::default()
                },
                None,
            )
            .unwrap();
            let operation = manager
                .accept_attach(
                    OperationToken {
                        scope: owner.registration().owner.clone(),
                        sequence: 1,
                        token: Token::parse("read-attach").unwrap(),
                    },
                    AttachRequest {
                        root: root.clone(),
                        revision: Some("HEAD".into()),
                        profile: IndexingProfile::default(),
                        lease: Token::parse("read-client").unwrap(),
                        owner: owner.registration().owner.clone(),
                        accept_current: None,
                        migratable: true,
                        allocation_version: 1,
                    },
                )
                .unwrap();
            let completed = manager.execute(&operation.id).unwrap();
            assert_eq!(completed.state, OperationState::Completed, "{completed:?}");
            let result = completed.result.unwrap();
            if stale_reads {
                assert_eq!(result["reconcile"]["provisional_files"], files.len());
            }
            let record: managed::ViewRecord =
                serde_json::from_value(result["current"].clone()).unwrap();
            let slot = manager.slot(&record.id).unwrap();
            if stale_reads {
                let status = manager.status(&record.id).unwrap();
                assert!(!status.ready);
                assert!(status.serving);
                let operation = manager
                    .accept_refresh(
                        OperationToken {
                            scope: owner.registration().owner.clone(),
                            sequence: 2,
                            token: Token::parse("first-verify").unwrap(),
                        },
                        RefreshRequest {
                            view: record.id.clone(),
                            expected_version: record.version,
                            owner: owner.registration().owner.clone(),
                            allocation_version: 1,
                        },
                    )
                    .unwrap();
                assert_eq!(
                    manager.execute(&operation.id).unwrap().state,
                    OperationState::Completed
                );
                assert!(manager.status(&record.id).unwrap().ready);
            }
            Self {
                temp,
                root,
                repository,
                namespace,
                owner,
                manager,
                record,
                slot,
            }
        }

        fn source(&self, inject: Inject) -> Scripted {
            Scripted {
                inner: ManagedQuery {
                    query: self.slot.query(self.record.version).unwrap(),
                    slot: Arc::clone(&self.slot),
                },
                inject,
                snapshots: Cell::new(0),
                checks: Cell::new(0),
                invalidations: Cell::new(0),
                requeues: Cell::new(0),
            }
        }

        fn ready(&self) -> bool {
            self.manager.status(&self.record.id).unwrap().ready
        }

        fn close(self) {
            let Self {
                temp,
                repository,
                namespace,
                owner,
                manager,
                slot,
                ..
            } = self;
            drop((slot, manager, owner, namespace, repository));
            temp.close().unwrap();
        }
    }

    fn candidate_read_failures(stale_reads: bool) {
        let fixture = Fixture::new(stale_reads, &["source.txt", "sibling.txt"]);
        let (root, manager, slot, record) = (
            &fixture.root,
            &fixture.manager,
            &fixture.slot,
            &fixture.record,
        );
        let run = |inject: Inject| {
            let source = fixture.source(inject);
            let result = super::super::server::search_snapshot(
                &source,
                &json!({"pattern":"shared_term"}),
                false,
                &json!(1),
                json!({}),
                super::super::server::PAGE_TIME,
            );
            (
                result,
                source.snapshots.get(),
                source.invalidations.get(),
                source.requeues.get(),
            )
        };

        let (result, snapshots, invalidations, requeues) = run(Inject::Nothing);
        let result = result.unwrap();
        assert_eq!(result["matches"].as_array().unwrap().len(), 2, "{result}");
        // Siblings open through the group's pin, so the query takes only the
        // listing, pin and final snapshots.
        assert_eq!((snapshots, invalidations, requeues), (3, 0, 0));
        let epoch = slot.input_epoch();
        for (inject, reason) in [
            (Inject::SnapshotBeforeRead(1), "query-input-changed"),
            (Inject::SnapshotAfterRead(1), "query-snapshot-changed"),
            (Inject::SnapshotBeforeRead(2), "query-input-changed"),
            (Inject::SnapshotAfterRead(2), "query-snapshot-changed"),
            (Inject::CheckFrom(0), "query-deadline"),
        ] {
            let (result, _, invalidations, requeues) = run(inject);
            let error = result.unwrap_err();
            let typed = error
                .downcast_ref::<Error>()
                .unwrap_or_else(|| panic!("{inject:?}: {error:#}"));
            assert_eq!(typed.reason_code, reason, "{inject:?}");
            assert!(
                !format!("{error:#}").contains("reading shared candidate"),
                "{inject:?}: {error:#}"
            );
            assert_eq!((invalidations, requeues), (0, 0), "{inject:?}");
            assert_eq!(slot.input_epoch(), epoch, "{inject:?}");
            assert!(manager.status(&record.id).unwrap().ready, "{inject:?}");
        }
        let (result, _, invalidations, _) = run(Inject::Nothing);
        assert_eq!(result.unwrap()["matches"].as_array().unwrap().len(), 2);
        assert_eq!(invalidations, 0);

        std::fs::remove_file(root.join("source.txt")).unwrap();
        std::fs::create_dir(root.join("source.txt")).unwrap();
        // Reading `sibling.txt` takes checks 0 and 1. Once check 2 fails, the
        // failed open of `source.txt` is not classified as a filesystem error.
        let (result, _, invalidations, requeues) = run(Inject::CheckFrom(2));
        let error = result.unwrap_err();
        let typed = error
            .downcast_ref::<Error>()
            .unwrap_or_else(|| panic!("{error:#}"));
        assert_eq!(typed.reason_code, "query-deadline");
        assert!(
            !format!("{error:#}").contains("reading shared candidate"),
            "{error:#}"
        );
        assert_eq!((invalidations, requeues), (0, 0));
        assert_eq!(slot.input_epoch(), epoch);
        assert!(manager.status(&record.id).unwrap().ready);
        if stale_reads {
            // The vanished candidate is skipped and queued as a path hint;
            // the view keeps serving its publication while it catches up.
            for round in 1..=2 {
                let (result, _, invalidations, requeues) = run(Inject::Nothing);
                let result = result.unwrap();
                let matches = result["matches"].as_array().unwrap();
                assert_eq!(matches.len(), 1, "{result}");
                assert_eq!((invalidations, requeues), (0, 1));
                assert_eq!(slot.input_epoch(), epoch + round);
                let status = manager.status(&record.id).unwrap();
                assert!(!status.ready && status.serving, "{status:?}");
            }
        } else {
            let (result, _, invalidations, _) = run(Inject::Nothing);
            let error = result.unwrap_err();
            assert!(
                format!("{error:#}").contains("reading shared candidate source.txt"),
                "{error:#}"
            );
            assert_eq!(invalidations, 1);
            assert_eq!(slot.input_epoch(), epoch + 1);
            assert!(!manager.status(&record.id).unwrap().ready);
        }
        fixture.close();
    }
}
