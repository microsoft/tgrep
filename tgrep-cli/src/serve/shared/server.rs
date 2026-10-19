use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use notify::{Event, EventKind};
use serde::Deserialize;
use serde_json::{Value, json};
use tgrep_core::generations::{
    BuildStats, ContentEligibility, GenerationManager, IndexingProfile, Repository,
};
use tgrep_core::worktrees::{
    ReconcileStats, WorktreeError, WorktreeOptions, WorktreeSnapshot, WorktreeView,
};

use super::protocol::{MAX_REQUEST, MAX_RESPONSE, Registration, Request, ViewRegistration};
use super::{MARKER, PROTOCOL};
use crate::serve::{WatchMode, WatchRegistry};

const CONNECTION_QUEUE: usize = 32;
const QUERY_QUEUE: usize = 16;
const QUERY_WORKERS: usize = 2;

pub struct Options<'a> {
    pub storage: &'a Path,
    pub no_watch: bool,
    pub watch_mode: WatchMode,
    pub poll_interval: Duration,
    pub watch_budget: usize,
    pub hint_budget: usize,
    pub max_views: usize,
    pub max_leases: usize,
}

struct Policy {
    no_watch: bool,
    watch_mode: WatchMode,
    interval: Duration,
    watches_per_view: usize,
    hints_per_view: usize,
    max_views: usize,
    max_leases: usize,
}

struct State {
    registration: Registration,
    repository: Repository,
    bases: PathBuf,
    overlays: PathBuf,
    policy: Policy,
    views: Mutex<HashMap<PathBuf, Arc<Entry>>>,
    sequence: AtomicU64,
}

struct Entry {
    id: String,
    view: WorktreeView,
    leases: Mutex<HashMap<String, Lease>>,
    queued: AtomicBool,
    running: AtomicBool,
    active: AtomicBool,
    monitor: Mutex<Monitor>,
    metrics: Mutex<Metrics>,
    queries: AtomicU64,
}

#[derive(Clone)]
struct Lease {
    revision: String,
    requested_commit: String,
    build: BuildStats,
}

struct Monitor {
    registry: Option<WatchRegistry>,
    fallback: Option<String>,
    // Callback errors cannot take monitor's lock while watch() is running.
    failed: Arc<AtomicBool>,
}

struct Metrics {
    last_attempt: Instant,
    last_success: Option<u64>,
    error: Option<String>,
    last: Option<ReconcileStats>,
    total_reads: u64,
    total_extractions: u64,
    build: BuildStats,
    restored: bool,
    attempts: u64,
    failures: u32,
}

fn retry_delay(failures: u32) -> Duration {
    if failures == 0 {
        Duration::from_millis(200)
    } else {
        Duration::from_secs((1_u64 << failures.saturating_sub(1).min(5)).min(30))
    }
}

type RpcJob = (TcpStream, Request);
enum Work {
    Rpc(RpcJob),
    Reconcile(Arc<Entry>),
}

struct RegistrationGuard {
    path: PathBuf,
    instance: String,
    _lock: File,
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        if let Ok(file) = File::open(&self.path)
            && let Ok(marker) = serde_json::from_reader::<_, Registration>(file)
            && marker.instance == self.instance
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn plain_directory(path: &Path) -> Result<PathBuf> {
    if !path.try_exists()? {
        fs::create_dir(path)?;
    }
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "storage must be a plain directory"
    );
    Ok(fs::canonicalize(path)?)
}

pub(super) fn publish(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let staging = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&staging)?;
    serde_json::to_writer(&mut file, value)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&staging, path)?;
    Ok(())
}

pub fn run(root: &Path, options: Options<'_>) -> Result<()> {
    ensure!(
        options.max_views > 0 && options.max_leases > 0,
        "shared limits must be positive"
    );
    ensure!(
        options.hint_budget >= options.max_views,
        "--watcher-queue-cap must be at least --shared-max-views"
    );
    let repository = Repository::discover(root)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(repository.common_dir().join("tgrep-daemon-v1.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("a shared daemon already owns this repository")?;
    let storage =
        fs::canonicalize(options.storage).context("--shared-storage must already exist")?;
    let bases = plain_directory(&storage.join("bases"))?;
    let manager = GenerationManager::with_storage(repository.clone(), &bases)?;
    let overlays = plain_directory(&storage.join("overlays"))?;
    let overlays = plain_directory(&overlays.join(repository.identity()))?;
    ensure!(
        !storage.starts_with(repository.common_dir()) && !overlays.starts_with(manager.directory()),
        "shared storage must be outside Git metadata and generation snapshots"
    );
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let instance = blake3::hash(
        format!(
            "{}:{:?}:{}",
            std::process::id(),
            SystemTime::now(),
            repository.identity()
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string();
    let registration = Registration {
        protocol: PROTOCOL,
        instance: instance.clone(),
        repository: repository.identity().to_string(),
        pid: std::process::id(),
        port: listener.local_addr()?.port(),
        storage,
    };
    let marker = repository.common_dir().join(MARKER);
    let _guard = RegistrationGuard {
        path: marker.clone(),
        instance,
        _lock: lock,
    };
    let state = Arc::new(State {
        registration,
        repository,
        bases,
        overlays,
        policy: Policy {
            no_watch: options.no_watch,
            watch_mode: options.watch_mode,
            interval: options.poll_interval,
            watches_per_view: options.watch_budget / options.max_views,
            hints_per_view: options.hint_budget / options.max_views,
            max_views: options.max_views,
            max_leases: options.max_leases,
        },
        views: Mutex::new(HashMap::new()),
        sequence: AtomicU64::new(1),
    });
    let (work_tx, work_rx) = mpsc::sync_channel::<Work>(options.max_views);
    let worker_state = Arc::clone(&state);
    thread::Builder::new()
        .name("shared-reconcile".into())
        .spawn(move || {
            for job in work_rx {
                match job {
                    Work::Rpc((stream, request)) => {
                        respond(stream, &request, &worker_state, || {
                            worker_state.lifecycle(&request)
                        })
                    }
                    Work::Reconcile(entry) => {
                        let retry_ready = {
                            let metrics = entry.metrics.lock().expect("metrics");
                            metrics.failures == 0
                                || metrics.last_attempt.elapsed() >= retry_delay(metrics.failures)
                        };
                        if entry.active.load(Ordering::SeqCst)
                            && retry_ready
                            && !entry.view.status().is_ok_and(|status| status.ready)
                            && let Err(error) = worker_state.reconcile(&entry)
                        {
                            eprintln!("shared reconcile {}: {error}", entry.view.root().display());
                        }
                        entry.queued.store(false, Ordering::SeqCst);
                    }
                }
            }
        })?;
    let (query_tx, query_rx) = mpsc::sync_channel::<RpcJob>(QUERY_QUEUE);
    let query_rx = Arc::new(Mutex::new(query_rx));
    for index in 0..QUERY_WORKERS {
        let state = Arc::clone(&state);
        let receiver = Arc::clone(&query_rx);
        thread::Builder::new()
            .name(format!("shared-query-{index}"))
            .spawn(move || {
                loop {
                    let Ok((stream, request)) = receiver.lock().expect("query queue").recv() else {
                        break;
                    };
                    respond(stream, &request, &state, || state.query(&request));
                }
            })?;
    }
    let schedule_state = Arc::clone(&state);
    let schedule_tx = work_tx.clone();
    thread::Builder::new()
        .name("shared-scheduler".into())
        .spawn(move || {
            loop {
                thread::sleep(Duration::from_millis(100));
                let entries = schedule_state.entries();
                for entry in entries {
                    let (elapsed, retry) = {
                        let metrics = entry.metrics.lock().expect("metrics");
                        (
                            metrics.last_attempt.elapsed(),
                            retry_delay(metrics.failures),
                        )
                    };
                    let ready = entry.view.status().is_ok_and(|status| status.ready);
                    let periodic = ready
                        && !schedule_state.policy.no_watch
                        && elapsed >= schedule_state.policy.interval;
                    if ((!ready && elapsed >= retry) || periodic)
                        && !entry.running.load(Ordering::SeqCst)
                        && !entry.queued.swap(true, Ordering::SeqCst)
                    {
                        if periodic && let Err(error) = entry.view.invalidate_all() {
                            eprintln!("shared invalidation: {error}");
                        }
                        if schedule_tx
                            .try_send(Work::Reconcile(Arc::clone(&entry)))
                            .is_err()
                        {
                            entry.queued.store(false, Ordering::SeqCst);
                        }
                    }
                }
            }
        })?;
    let (incoming_tx, incoming_rx) = mpsc::sync_channel::<TcpStream>(CONNECTION_QUEUE);
    let incoming_rx = Arc::new(Mutex::new(incoming_rx));
    for index in 0..2 {
        let receiver = Arc::clone(&incoming_rx);
        let state = Arc::clone(&state);
        let work = work_tx.clone();
        let queries = query_tx.clone();
        thread::Builder::new()
            .name(format!("shared-router-{index}"))
            .spawn(move || {
                loop {
                    let Ok(mut stream) = receiver.lock().expect("incoming queue").recv() else {
                        break;
                    };
                    let request = (|| -> Result<Request> {
                        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                        let bytes = super::protocol::read_line(&mut stream, MAX_REQUEST)?;
                        let request: Request = serde_json::from_slice(&bytes)?;
                        state.validate(&request)?;
                        if request.method == "refresh" {
                            state.invalidate(&request.params)?;
                        }
                        Ok(request)
                    })();
                    match request {
                        Err(error) => send_error(&mut stream, Value::Null, &error),
                        Ok(request) => {
                            let lifecycle =
                                matches!(request.method.as_str(), "attach" | "detach" | "refresh");
                            let rejected = if lifecycle {
                                work.try_send(Work::Rpc((stream, request)))
                                    .err()
                                    .map(|error| {
                                        let Work::Rpc(job) = unsent(error) else {
                                            unreachable!()
                                        };
                                        job
                                    })
                            } else {
                                queries.try_send((stream, request)).err().map(unsent)
                            };
                            if let Some((mut stream, request)) = rejected {
                                send_error(
                                    &mut stream,
                                    request.id,
                                    &anyhow::anyhow!("shared request queue full; retry or scan"),
                                );
                            }
                        }
                    }
                }
            })?;
    }
    // Listener and every bounded worker queue are usable before discovery.
    publish(&marker, &state.registration)?;
    eprintln!(
        "warning: shared repository daemon v1 is deprecated as of tgrep 1.2.1 but is still supported; use managed v2 with --shared-policy <FILE> for new integrations."
    );
    eprintln!(
        "Shared repository daemon v{PROTOCOL} listening on 127.0.0.1:{}",
        state.registration.port
    );
    let mut accept_failures = super::AcceptRetry::new();
    for connection in listener.incoming() {
        let stream = match connection {
            Ok(stream) => {
                accept_failures.accepted();
                stream
            }
            Err(error) => {
                accept_failures.failed(error)?;
                continue;
            }
        };
        if let Err(error) = incoming_tx.try_send(stream) {
            let mut stream = unsent(error);
            // Without a write timeout the reply could stall the accept loop.
            if stream
                .set_write_timeout(Some(Duration::from_millis(100)))
                .is_ok()
            {
                send_error(
                    &mut stream,
                    Value::Null,
                    &anyhow::anyhow!("shared connection queue full"),
                );
            }
        }
    }
    Ok(())
}

fn send_error(stream: &mut TcpStream, id: Value, error: &anyhow::Error) {
    let response =
        json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32001,"message":format!("{error:#}")}});
    if let Err(error) = writeln!(stream, "{response}") {
        eprintln!("shared RPC error response failed: {error}");
    }
}

fn unsent<T>(error: mpsc::TrySendError<T>) -> T {
    match error {
        mpsc::TrySendError::Full(value) | mpsc::TrySendError::Disconnected(value) => value,
    }
}

fn respond(
    mut stream: TcpStream,
    request: &Request,
    state: &State,
    action: impl FnOnce() -> Result<Value>,
) {
    match action().and_then(|mut result| {
        result["protocol"] = json!(PROTOCOL);
        result["instance"] = json!(state.registration.instance);
        result["repository"] = json!(state.registration.repository);
        bounded_response(
            &json!({"jsonrpc":"2.0","id":request.id,"result":result}),
            MAX_RESPONSE as usize - 1,
        )
    }) {
        Ok(bytes) => {
            if let Err(error) = stream
                .write_all(&bytes)
                .and_then(|()| stream.write_all(b"\n"))
            {
                eprintln!("shared RPC response failed: {error}");
            }
        }
        Err(error) => send_error(&mut stream, request.id.clone(), &error),
    }
}

fn bounded_response(value: &Value, limit: usize) -> Result<Vec<u8>> {
    struct Buffer {
        bytes: Vec<u8>,
        budget: crate::serve::JsonBudget,
    }
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.budget.write_all(bytes)?;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Buffer {
        bytes: Vec::new(),
        budget: crate::serve::JsonBudget { remaining: limit },
    };
    serde_json::to_writer(&mut buffer, value)?;
    Ok(buffer.bytes)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RootParams {
    root: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachParams {
    root: PathBuf,
    revision: String,
    profile: IndexingProfile,
    lease: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewParams {
    root: PathBuf,
    view: String,
    query: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LeaseParams {
    root: PathBuf,
    view: String,
    lease: String,
    #[serde(default)]
    changed: Vec<PathBuf>,
    #[serde(default)]
    full: bool,
}

impl State {
    fn entries(&self) -> Vec<Arc<Entry>> {
        self.views
            .lock()
            .expect("views")
            .values()
            .cloned()
            .collect()
    }

    fn validate(&self, request: &Request) -> Result<()> {
        ensure!(
            request.jsonrpc == "2.0"
                && request.protocol == PROTOCOL
                && request.instance == self.registration.instance
                && request.repository == self.registration.repository
                && (request.id.is_number() || request.id.is_string()),
            "incompatible protocol, stale daemon instance or repository"
        );
        ensure!(request.params.is_object(), "params must be an object");
        Ok(())
    }

    fn root(&self, root: &Path) -> Result<(PathBuf, Repository)> {
        let root = super::canonical_root(root)?;
        let repository = Repository::discover(&root)?;
        ensure!(
            super::worktree_root(&root)? == root
                && repository.identity() == self.repository.identity(),
            "root is not a worktree of this repository"
        );
        ensure!(
            !self.registration.storage.starts_with(&root)
                && !root.starts_with(&self.registration.storage),
            "worktree overlaps shared storage"
        );
        Ok((root, repository))
    }

    fn entry(&self, root: &Path, id: Option<&str>) -> Result<Arc<Entry>> {
        let root = super::canonical_root(root)?;
        let entry = self
            .views
            .lock()
            .expect("views")
            .get(&root)
            .cloned()
            .context("worktree is not attached; attach explicitly")?;
        super::validate_repository(&root, entry.view.repository())?;
        ensure!(
            id.is_none_or(|id| id == entry.id) && entry.active.load(Ordering::SeqCst),
            "stale shared view identity"
        );
        Ok(entry)
    }

    fn token(&self) -> String {
        format!(
            "{}-{}",
            self.registration.instance,
            self.sequence.fetch_add(1, Ordering::SeqCst)
        )
    }

    fn lease(&self, params: &LeaseParams) -> Result<Arc<Entry>> {
        let entry = self.entry(&params.root, Some(&params.view))?;
        ensure!(
            entry
                .leases
                .lock()
                .expect("leases")
                .contains_key(&params.lease),
            "invalid or released lease"
        );
        Ok(entry)
    }

    fn invalidate(&self, value: &Value) -> Result<()> {
        let params: LeaseParams = serde_json::from_value(value.clone())?;
        let entry = self.lease(&params)?;
        entry.metrics.lock().expect("metrics").failures = 0;
        if params.full || params.changed.is_empty() {
            entry.view.invalidate_all()?;
        } else {
            if params.changed.len() > self.policy.hints_per_view {
                entry.view.invalidate_all()?;
                bail!("too many change hints; full repair scheduled");
            }
            for path in &params.changed {
                entry.view.invalidate_path(path)?;
            }
        }
        Ok(())
    }

    fn lifecycle(&self, request: &Request) -> Result<Value> {
        match request.method.as_str() {
            "attach" => self.attach(serde_json::from_value(request.params.clone())?),
            "refresh" => {
                let params: LeaseParams = serde_json::from_value(request.params.clone())?;
                let entry = self.lease(&params)?;
                let epoch = self.reconcile(&entry)?;
                let mut value = self.describe(&entry)?;
                value["processed_epoch"] = json!(epoch);
                Ok(value)
            }
            "detach" => {
                let params: LeaseParams = serde_json::from_value(request.params.clone())?;
                ensure!(
                    params.changed.is_empty() && !params.full,
                    "detach does not accept refresh hints"
                );
                let entry = self.lease(&params)?;
                let leases = entry.leases.lock().expect("leases");
                ensure!(leases.contains_key(&params.lease), "lease already released");
                let remaining = leases.len() - 1;
                drop(leases);
                let mut registration_warning = None;
                if remaining == 0 {
                    if let Err(error) = self.remove_view_marker(&entry) {
                        let warning = format!("shared view registration cleanup failed: {error:#}");
                        eprintln!("{warning}");
                        registration_warning = Some(warning);
                    }
                    entry.active.store(false, Ordering::SeqCst);
                    entry.monitor.lock().expect("monitor").registry = None;
                    self.views.lock().expect("views").remove(entry.view.root());
                }
                entry.leases.lock().expect("leases").remove(&params.lease);
                Ok(
                    json!({"remaining_leases":remaining, "view":entry.id, "detached":true,
                    "registration_warning":registration_warning}),
                )
            }
            _ => bail!("unknown shared lifecycle method"),
        }
    }

    fn remove_view_marker(&self, entry: &Entry) -> Result<()> {
        let marker = entry
            .view
            .repository()
            .git_dir()
            .join(super::protocol::VIEW_MARKER);
        let file = match File::open(&marker) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if super::protocol::marker_present(&marker)? {
                    return Err(error.into());
                }
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let registered: ViewRegistration = serde_json::from_reader(file.take(MAX_REQUEST))?;
        if registered.daemon.instance == self.registration.instance && registered.view == entry.id {
            match fs::remove_file(marker) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn attach(&self, params: AttachParams) -> Result<Value> {
        ensure!(
            !params.lease.is_empty()
                && params.lease.len() <= 128
                && params
                    .lease
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)),
            "lease must contain 1-128 ASCII letters, digits, hyphens or underscores"
        );
        ensure!(params.revision.len() <= 4096, "revision is too long");
        ensure!(
            params.profile == IndexingProfile::default(),
            "unsupported indexing profile; v1 uses raw-auto tracked regular files, 64 MiB"
        );
        let (root, repository) = self.root(&params.root)?;
        let entries = self.entries();
        for entry in &entries {
            let lease = entry
                .leases
                .lock()
                .expect("leases")
                .get(&params.lease)
                .cloned();
            if let Some(lease) = lease {
                ensure!(
                    entry.view.root() == root && lease.revision == params.revision,
                    "lease already belongs to a different root or revision"
                );
                super::validate_repository(&root, entry.view.repository())?;
                self.publish_view(entry)?;
                return self.attach_result(entry, &params.lease, &lease);
            }
        }
        let total_leases: usize = entries
            .iter()
            .map(|entry| entry.leases.lock().expect("leases").len())
            .sum();
        ensure!(
            total_leases < self.policy.max_leases,
            "shared lease limit reached"
        );
        let existing = entries
            .iter()
            .find(|entry| entry.view.root() == root)
            .cloned();
        ensure!(
            existing.is_some() || entries.len() < self.policy.max_views,
            "shared view limit reached"
        );
        let (requested_commit, tree) = repository.resolve_commit_tree(&params.revision)?;
        let (entry, build) = if let Some(entry) = existing {
            super::validate_repository(&root, entry.view.repository())?;
            ensure!(
                entry.view.generation().key().tree_oid() == tree
                    && *entry.view.generation().key().profile() == params.profile,
                "worktree already pins a different base; release all leases before selecting another"
            );
            let tracked_entries = entry.view.generation().entries().len();
            (
                entry,
                BuildStats {
                    reused_generation: true,
                    tracked_entries,
                    ..Default::default()
                },
            )
        } else {
            let root_id = blake3::hash(
                root.to_str()
                    .context("shared CLI requires a UTF-8 root")?
                    .as_bytes(),
            )
            .to_hex()
            .to_string();
            let manager = GenerationManager::with_storage(repository, &self.bases)?;
            let predecessor = entries.iter().map(|entry| entry.view.generation()).next();
            let ensured = manager.ensure(&requested_commit, params.profile, predecessor)?;
            let directory = plain_directory(&self.overlays.join(root_id))?;
            let directory =
                plain_directory(&directory.join(ensured.generation.key().storage_name()))?;
            let options = WorktreeOptions {
                checkpoint_directory: Some(directory.clone()),
                hint_capacity: self.policy.hints_per_view,
                walk: tgrep_core::walker::MetaWalkOptions {
                    exclude_paths: vec![self.registration.storage.clone()],
                    ..Default::default()
                },
                stale_reads: false,
            };
            let restored = directory.join("overlay.json").try_exists()?;
            let view = if restored {
                WorktreeView::restore(&root, Arc::clone(&ensured.generation), options)?
            } else {
                WorktreeView::new(&root, Arc::clone(&ensured.generation), options)?
            };
            let entry = Arc::new(Entry {
                id: self.token(),
                view,
                leases: Mutex::new(HashMap::new()),
                queued: AtomicBool::new(false),
                running: AtomicBool::new(false),
                active: AtomicBool::new(true),
                monitor: Mutex::new(Monitor {
                    registry: None,
                    fallback: None,
                    failed: Arc::new(AtomicBool::new(false)),
                }),
                metrics: Mutex::new(Metrics {
                    last_attempt: Instant::now(),
                    last_success: None,
                    error: None,
                    last: None,
                    total_reads: 0,
                    total_extractions: 0,
                    build: ensured.stats.clone(),
                    restored,
                    attempts: 0,
                    failures: 0,
                }),
                queries: AtomicU64::new(0),
            });
            // Capture native invalidations before the very first reconciliation.
            self.watch(&entry)?;
            self.views
                .lock()
                .expect("views")
                .insert(root.clone(), Arc::clone(&entry));
            (entry, ensured.stats)
        };
        let lease = Lease {
            revision: params.revision,
            requested_commit,
            build,
        };
        if let Err(error) = self.publish_view(&entry) {
            if entry.leases.lock().expect("leases").is_empty() {
                entry.active.store(false, Ordering::SeqCst);
                entry.monitor.lock().expect("monitor").registry = None;
                self.views.lock().expect("views").remove(entry.view.root());
            }
            return Err(error);
        }
        entry
            .leases
            .lock()
            .expect("leases")
            .insert(params.lease.clone(), lease.clone());
        self.attach_result(&entry, &params.lease, &lease)
    }

    fn publish_view(&self, entry: &Entry) -> Result<()> {
        let marker = ViewRegistration {
            daemon: self.registration.clone(),
            root: entry.view.root().to_path_buf(),
            view: entry.id.clone(),
            generation: entry.view.generation().key().clone(),
        };
        publish(
            &entry
                .view
                .repository()
                .git_dir()
                .join(super::protocol::VIEW_MARKER),
            &marker,
        )
    }

    fn attach_result(&self, entry: &Entry, token: &str, lease: &Lease) -> Result<Value> {
        let mut value = self.describe(entry)?;
        value["lease"] = json!(token);
        value["requested_commit"] = json!(lease.requested_commit);
        value["attach_build"] = build_stats(&lease.build);
        Ok(value)
    }

    fn reconcile(&self, entry: &Arc<Entry>) -> Result<u64> {
        entry.running.store(true, Ordering::SeqCst);
        entry.metrics.lock().expect("metrics").attempts += 1;
        let result = (|| {
            self.watch(entry)?;
            let stats = entry.view.refresh()?;
            let epoch = stats.epoch;
            entry.view.save_checkpoint()?;
            Ok((stats, epoch))
        })();
        let result = (|| {
            let mut metrics = entry.metrics.lock().expect("metrics");
            metrics.last_attempt = Instant::now();
            match result {
                Ok((stats, epoch)) => {
                    metrics.total_reads += stats.files_read;
                    metrics.total_extractions += stats.files_extracted;
                    metrics.last = Some(stats);
                    metrics.error = None;
                    metrics.failures = 0;
                    metrics.last_success =
                        Some(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs());
                    Ok(epoch)
                }
                Err(error) => {
                    entry.view.invalidate_all()?;
                    metrics.error = Some(format!("{error:#}"));
                    metrics.failures = metrics.failures.saturating_add(1);
                    Err(error)
                }
            }
        })();
        entry.running.store(false, Ordering::SeqCst);
        result
    }

    fn describe(&self, entry: &Entry) -> Result<Value> {
        let snapshot_valid = match entry.view.with_snapshot(|_| ()) {
            Ok(()) => true,
            Err(WorktreeError::NotReady) => false,
            Err(error) => {
                entry.metrics.lock().expect("metrics").error =
                    Some(format!("validating shared snapshot: {error}"));
                false
            }
        };
        let status = entry.view.status()?;
        let running = entry.running.load(Ordering::SeqCst);
        let metrics = entry.metrics.lock().expect("metrics");
        let monitor = entry.monitor.lock().expect("monitor");
        let checkpoint_epoch = metrics.last.as_ref().map(|stats| stats.epoch);
        Ok(json!({
            "root":entry.view.root(), "view":entry.id, "generation":entry.view.generation().key(),
            "ready":snapshot_valid && status.ready && !running
                && checkpoint_epoch == Some(status.epoch),
            "epoch":status.epoch, "published_epoch":status.published_epoch,
            "pending_paths":status.pending_paths, "full_required":status.full_required,
            "queued":entry.queued.load(Ordering::SeqCst), "last_error":metrics.error,
            "reconcile_running":running,
            "reconcile_attempts":metrics.attempts, "consecutive_failures":metrics.failures,
            "retry_delay_ms":retry_delay(metrics.failures).as_millis(),
            "last_success":metrics.last_success, "leases":entry.leases.lock().expect("leases").len(),
            "watch_mode":if self.policy.no_watch {"disabled"} else if monitor.registry.is_some() {"native"} else {"poll"},
            "watch_fallback":monitor.fallback, "watch_count":monitor.registry.as_ref().map_or(0, |r| r.watched.len()),
            "poll_interval_seconds":self.policy.interval.as_secs(), "checkpoint_restored":metrics.restored,
            "queries":entry.queries.load(Ordering::SeqCst),
            "total_reads":metrics.total_reads, "total_extractions":metrics.total_extractions,
            "last_reconcile":metrics.last.as_ref().map(reconcile_stats), "generation_build":build_stats(&metrics.build),
            "base_sharing_views":self.entries().iter().filter(|other| Arc::ptr_eq(other.view.generation().base(), entry.view.generation().base())).count()
        }))
    }

    fn query(&self, request: &Request) -> Result<Value> {
        match request.method.as_str() {
            "hello" => {
                ensure!(request.params == json!({}), "hello params must be empty");
                Ok(json!({
                    "capabilities":["leases","recoverable-attach","worktree-overlays","refresh","search","files"],
                    "profile":IndexingProfile::default(), "retention":"retain-all", "paged_results":true,
                    "limits":{"views":self.policy.max_views, "leases":self.policy.max_leases,
                        "watches_per_view":self.policy.watches_per_view, "hints_per_view":self.policy.hints_per_view,
                        "reconcile_workers":1, "query_workers":QUERY_WORKERS, "query_queue":QUERY_QUEUE,
                        "work_queue":self.policy.max_views, "connections":CONNECTION_QUEUE,
                        "request_bytes":MAX_REQUEST, "response_bytes":MAX_RESPONSE, "content_cache_bytes":0}
                }))
            }
            "lookup" => {
                let params: RootParams = serde_json::from_value(request.params.clone())?;
                self.describe(self.entry(&params.root, None)?.as_ref())
            }
            "status" | "search" | "files" => {
                let params: ViewParams = serde_json::from_value(request.params.clone())?;
                let entry = self.entry(&params.root, Some(&params.view))?;
                if request.method == "status" {
                    ensure!(params.query == json!({}), "status query must be empty");
                    return self.describe(&entry);
                }
                self.search(
                    &entry,
                    &params.query,
                    request.method == "files",
                    &request.id,
                )
            }
            _ => bail!("unknown shared RPC method"),
        }
    }

    fn watch(&self, entry: &Arc<Entry>) -> Result<()> {
        if self.policy.no_watch || self.policy.watch_mode == WatchMode::Poll {
            return Ok(());
        }
        let mut monitor = entry.monitor.lock().expect("monitor");
        if monitor.failed.load(Ordering::SeqCst) {
            monitor.registry = None;
            monitor.fallback = Some("native notification failure; polling repairs coverage".into());
        }
        if monitor.fallback.is_some() {
            return Ok(());
        }
        let desired = watch_directories(
            entry.view.root(),
            &self.registration.storage,
            self.policy.watches_per_view,
        );
        let desired = match desired {
            Ok(dirs) => dirs,
            Err(error) => {
                monitor.registry = None;
                monitor.fallback = Some(error.to_string());
                entry.view.invalidate_all()?;
                eprintln!("shared native watch fallback: {error}");
                return Ok(());
            }
        };
        if monitor.registry.is_none() {
            let weak = Arc::downgrade(entry);
            let failed = Arc::clone(&monitor.failed);
            let watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
                if let Some(entry) = weak.upgrade() {
                    match event {
                        Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                            let result = invalidate_event(&entry, &event);
                            if let Err(error) = result {
                                failed.store(true, Ordering::SeqCst);
                                eprintln!("shared watcher invalidation: {error}");
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            failed.store(true, Ordering::SeqCst);
                            if let Err(invalidation) = entry.view.invalidate_all() {
                                eprintln!("shared watcher invalidation: {invalidation}");
                            }
                            eprintln!("shared watcher: {error}");
                        }
                    }
                }
            });
            match watcher {
                Ok(watcher) => {
                    monitor.registry = Some(WatchRegistry {
                        watcher,
                        root: entry.view.root().to_path_buf(),
                        watched: HashSet::new(),
                        budget: self.policy.watches_per_view,
                        failure: None,
                        #[cfg(test)]
                        fail_after: None,
                        polling: Arc::new(AtomicBool::new(false)),
                    })
                }
                Err(error) => {
                    monitor.fallback = Some(format!("native registration failed: {error}"));
                    entry.view.invalidate_all()?;
                    return Ok(());
                }
            }
        }
        let registry = monitor.registry.as_mut().expect("watch registry");
        registry.sync(
            &desired,
            crate::serve::TraversalCompleteness::Complete,
            true,
        );
        if let Some(error) = registry.failure.take() {
            monitor.registry = None;
            monitor.fallback = Some(error.clone());
            entry.view.invalidate_all()?;
            eprintln!("shared native watch fallback: {error}");
        }
        Ok(())
    }
}

fn watch_directories(root: &Path, storage: &Path, budget: usize) -> Result<HashSet<PathBuf>> {
    let mut dirs = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        ensure!(
            dirs.len() < budget,
            "native watch budget exceeded; using polling"
        );
        dirs.insert(dir.clone());
        for child in fs::read_dir(&dir)? {
            let child = child?;
            if child.file_type()?.is_dir() {
                let path = child.path();
                if child.file_name() == ".git"
                    || path == root.join(".tgrep")
                    || path.starts_with(storage)
                {
                    continue;
                }
                ensure!(
                    dirs.len() + stack.len() < budget,
                    "native watch budget exceeded; using polling"
                );
                stack.push(path);
            }
        }
    }
    Ok(dirs)
}

fn invalidate_event(entry: &Entry, event: &Event) -> Result<()> {
    if event_requires_full(event) {
        entry.view.invalidate_all()?;
        return Ok(());
    }
    for path in &event.paths {
        let Ok(relative) = path.strip_prefix(entry.view.root()) else {
            entry.view.invalidate_all()?;
            continue;
        };
        // Our own metadata publication must not create an endless repair loop.
        // A linked worktree's ordinary gitfile is searchable, not a directory
        // containing our registration/checkpoint metadata.
        let gitfile = relative == Path::new(".git") && !path.is_dir();
        if relative.starts_with(".tgrep")
            || (!gitfile && relative.components().any(|part| part.as_os_str() == ".git"))
        {
            continue;
        }
        if relative.as_os_str().is_empty() {
            entry.view.invalidate_all()?;
        } else {
            entry.view.invalidate_path(relative)?;
        }
    }
    Ok(())
}

fn event_requires_full(event: &Event) -> bool {
    event.need_rescan()
        || event.paths.is_empty()
        || matches!(event.kind, EventKind::Any | EventKind::Other)
}

fn reconcile_stats(stats: &ReconcileStats) -> Value {
    json!({
        "epoch":stats.epoch, "full":stats.full, "files_discovered":stats.files_discovered,
        "files_read":stats.files_read, "bytes_read":stats.bytes_read, "files_decoded":stats.files_decoded,
        "files_extracted":stats.files_extracted, "base_reused":stats.base_reused,
        "base_files_copied":stats.base_files_copied, "postings_copied":stats.postings_copied,
        "overlay_reused":stats.overlay_reused, "content_reads_avoided":stats.content_reads_avoided,
        "hint_lookups":stats.hint_lookups
    })
}

fn build_stats(stats: &BuildStats) -> Value {
    json!({
        "published":stats.published, "reused_generation":stats.reused_generation,
        "tracked_entries":stats.tracked_entries, "blobs_read":stats.blobs_read,
        "blob_bytes_read":stats.blob_bytes_read, "blobs_extracted":stats.blobs_extracted,
        "reused_indexed_files":stats.reused_indexed_files, "postings_reused":stats.postings_reused,
        "predecessor_posting_lists_read":stats.predecessor_posting_lists_read
    })
}

fn validate_query(query: &Value, files: bool) -> Result<()> {
    let object = query.as_object().context("query must be an object")?;
    for (key, value) in object {
        let valid = match key.as_str() {
            "scope" => value.is_string(),
            "hidden" => value.is_boolean(),
            "max_depth" => value.is_null() || value.is_u64(),
            "page" => value.is_null() || value.is_string(),
            _ if files => false,
            "pattern" | "encoding" | "engine" => value.is_string(),
            "replace" => value.is_null() || value.is_string(),
            "extra_patterns" | "glob" | "iglob" | "types" | "types_not" | "type_add"
            | "type_clear" => value
                .as_array()
                .is_some_and(|array| array.iter().all(Value::is_string)),
            "type_ops" => crate::search::type_ops_from_json(value).is_ok(),
            "max_count" | "after_context" | "before_context" | "max_filesize"
            | "regex_size_limit" | "dfa_size_limit" => value.is_null() || value.is_u64(),
            "case_insensitive"
            | "fixed_string"
            | "files_only"
            | "word_boundary"
            | "glob_case_insensitive"
            | "invert_match"
            | "only_matching"
            | "multiline"
            | "multiline_dotall"
            | "text"
            | "binary_lines"
            | "line_regexp"
            | "no_unicode"
            | "passthru"
            | "stop_on_nonmatch"
            | "vimgrep"
            | "detail"
            | "positions"
            | "count"
            | "count_matches"
            | "ordered_filters"
            | "multiline_replace"
            | "report_replacements"
            | "search_stats"
            | "stats" => value.is_boolean(),
            "content_eligibility" => {
                serde_json::from_value::<ContentEligibility>(value.clone()).is_ok()
            }
            _ => false,
        };
        ensure!(valid, "unknown or malformed shared query option: {key}");
    }
    if !files {
        ensure!(query["pattern"].is_string(), "pattern is required");
        ensure!(
            query.get("passthru").is_none_or(|value| value == false),
            "passthru requires a filesystem scan"
        );
        ensure!(
            query.get("encoding").is_none_or(|v| v == "auto")
                && query.get("text").is_none_or(|v| v == false)
                && query
                    .get("max_filesize")
                    .is_none_or(|v| v == 64 * 1024 * 1024),
            "query decoding or size profile is incompatible; scan instead"
        );
    }
    if let Some(scope) = query.get("scope").and_then(Value::as_str) {
        ensure!(
            !scope.contains('\\')
                && !scope.contains(':')
                && scope
                    .trim_end_matches('/')
                    .split('/')
                    .all(|part| scope.is_empty()
                        || (!part.is_empty() && part != "." && part != ".."))
                && Path::new(scope)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
            "scope must be a relative directory"
        );
    }
    Ok(())
}

impl State {
    fn query_snapshot<T>(
        &self,
        entry: &Entry,
        read: impl FnOnce(WorktreeSnapshot<'_>) -> T,
    ) -> Result<T> {
        ensure!(
            !entry.running.load(Ordering::SeqCst),
            "shared reconciliation in progress; retry or scan"
        );
        let (epoch, result) = entry
            .view
            .with_snapshot(|snapshot| (snapshot.epoch(), read(snapshot)))
            .inspect_err(|error| {
                if !matches!(error, WorktreeError::NotReady) {
                    entry.metrics.lock().expect("metrics").error =
                        Some(format!("validating shared snapshot: {error}"));
                }
            })?;
        let checkpoint_epoch = entry
            .metrics
            .lock()
            .expect("metrics")
            .last
            .as_ref()
            .map(|stats| stats.epoch);
        ensure!(
            !entry.running.load(Ordering::SeqCst) && checkpoint_epoch == Some(epoch),
            "shared reconciliation or checkpoint publication incomplete; retry or scan"
        );
        Ok(result)
    }

    fn search(&self, entry: &Entry, query: &Value, files: bool, id: &Value) -> Result<Value> {
        let metadata = json!({
            "root":entry.view.root(), "view":entry.id, "generation":entry.view.generation().key(),
            "ready":true, "hidden_complete":true, "ordered_filters":true,
            "multiline_replace":true, "report_replacements":true, "backend":"shared-v1",
            "protocol":PROTOCOL, "instance":self.registration.instance,
            "repository":self.registration.repository
        });
        let result = search_snapshot_parallel(
            &LegacyQuery { state: self, entry },
            query,
            files,
            id,
            metadata,
            PAGE_TIME,
        )?;
        entry.queries.fetch_add(1, Ordering::SeqCst);
        Ok(result)
    }
}

pub(super) trait QuerySource {
    fn root(&self) -> &Path;
    /// Content eligibility of the exact generation profile being served.
    fn eligibility(&self) -> Result<ContentEligibility>;
    fn snapshot<T>(&self, read: impl FnOnce(WorktreeSnapshot<'_>) -> T) -> Result<T>;
    fn invalidate(&self, error: &anyhow::Error) -> Result<()>;
    fn check(&self) -> Result<()> {
        self.snapshot(|_| ())
    }
    /// Eventual sources serve their last publication, so a candidate that
    /// changed or vanished since is skipped and requeued, not a query failure.
    fn eventual(&self) -> bool {
        false
    }
    /// Queue root-relative candidates for an incremental eventual refresh.
    fn requeue(&self, _relative: &[String]) -> Result<()> {
        Ok(())
    }
}

struct LegacyQuery<'a> {
    state: &'a State,
    entry: &'a Entry,
}

impl QuerySource for LegacyQuery<'_> {
    fn root(&self) -> &Path {
        self.entry.view.root()
    }
    fn eligibility(&self) -> Result<ContentEligibility> {
        Ok(self.entry.view.generation().key().profile().eligibility)
    }
    fn snapshot<T>(&self, read: impl FnOnce(WorktreeSnapshot<'_>) -> T) -> Result<T> {
        self.state.query_snapshot(self.entry, read)
    }
    fn invalidate(&self, error: &anyhow::Error) -> Result<()> {
        self.entry.view.invalidate_all()?;
        self.entry.metrics.lock().expect("metrics").error = Some(format!("{error:#}"));
        Ok(())
    }
}

/// How long a paged query searches before returning a partial page.
pub(super) const PAGE_TIME: Duration = Duration::from_secs(60);

/// Serial search for sources that cannot be shared between threads.
#[cfg(test)]
pub(super) fn search_snapshot(
    source: &impl QuerySource,
    query: &Value,
    files: bool,
    id: &Value,
    metadata: Value,
) -> Result<Value> {
    search_snapshot_with(
        source,
        query,
        files,
        id,
        metadata,
        PAGE_TIME,
        |groups, search| {
            groups
                .iter()
                .map(|siblings| search.search(source, siblings))
                .collect()
        },
    )
}

/// Searches bounded windows of sibling groups on the rayon pool.
pub(super) fn search_snapshot_parallel(
    source: &(impl QuerySource + Sync),
    query: &Value,
    files: bool,
    id: &Value,
    metadata: Value,
    page_time: Duration,
) -> Result<Value> {
    use rayon::prelude::*;
    search_snapshot_with(
        source,
        query,
        files,
        id,
        metadata,
        page_time,
        |groups, search| {
            groups
                .par_iter()
                .map(|siblings| search.search(source, siblings))
                .collect()
        },
    )
}

type Searched = Vec<(crate::serve::FileSearchResult, usize)>;

/// One window's shared response budget. A worker that cannot charge a whole
/// file stops the window, which then ends the page or fails the query.
struct GroupSearch<'a> {
    matcher: &'a crate::matching::SearchMatcher,
    opts: &'a crate::serve::SearchOpts,
    encoding: tgrep_core::encoding::EncodingMode,
    remaining: std::sync::atomic::AtomicUsize,
    stopped: AtomicBool,
}

impl<'a> GroupSearch<'a> {
    fn new(request: &'a crate::serve::SearchRequest, remaining: usize) -> Self {
        Self {
            matcher: &request.matcher,
            opts: &request.opts,
            encoding: request.encoding,
            remaining: remaining.into(),
            stopped: AtomicBool::new(false),
        }
    }

    /// Searched files in order; fewer than `siblings` means the rest did not
    /// fit or were not searched.
    fn search(&self, source: &impl QuerySource, siblings: &[String]) -> Result<Searched> {
        let mut files = Vec::new();
        if self.stopped.load(Ordering::Relaxed) {
            return Ok(files);
        }
        let result = (|| {
            // An eventual source whose shared parent cannot be pinned falls
            // back to per-file opens, which skip and requeue what is missing.
            let batch = (siblings.len() > 1)
                .then(|| {
                    candidate_snapshot(source, &siblings[0], |snapshot| {
                        snapshot.candidate_batch(&siblings[0])
                    })
                })
                .transpose()?
                .flatten();
            for relative in siblings {
                if self.stopped.load(Ordering::Relaxed) {
                    break;
                }
                let Some(bytes) = read_candidate(source, batch.as_ref(), relative)? else {
                    files.push(Default::default());
                    continue;
                };
                if bytes.len() > 64 * 1024 * 1024 {
                    files.push(Default::default());
                    continue;
                }
                let decoded = crate::serve::DecodedFile::new(bytes, self.encoding);
                let cap = self.remaining.load(Ordering::Relaxed);
                let mut budget = crate::serve::JsonBudget { remaining: cap };
                match crate::serve::search_file_matches_bounded(
                    relative,
                    &decoded,
                    self.matcher,
                    self.opts,
                    Some(&mut budget),
                ) {
                    Ok(found) if self.claim(cap - budget.remaining) => {
                        files.push((found, cap - budget.remaining));
                    }
                    Ok(_) => break,
                    Err(error) if error.is::<crate::serve::ResponseLimit>() => break,
                    Err(error) => return Err(error),
                }
            }
            if let Some(batch) = batch
                && let Err(error) = batch.finish()
            {
                if !source.eventual() {
                    return candidate_failed(source, &siblings[0], error.into())
                        .map(|_: Option<()>| ());
                }
                // The group's reads may have crossed a replaced parent.
                source.requeue(siblings)?;
            }
            Ok(())
        })();
        if result.is_err() || files.len() < siblings.len() {
            self.stopped.store(true, Ordering::Relaxed);
        }
        result.map(|()| files)
    }

    fn claim(&self, bytes: usize) -> bool {
        let mut left = self.remaining.load(Ordering::Acquire);
        loop {
            let Some(next) = left.checked_sub(bytes) else {
                return false;
            };
            match self.remaining.compare_exchange_weak(
                left,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => left = actual,
            }
        }
    }
}

/// `page` absent keeps the whole-result contract. Otherwise a page ends at a
/// file boundary when the response budget or `page_time` runs out, and
/// `next_page` names the first path it did not search.
fn search_snapshot_with<S: QuerySource>(
    source: &S,
    query: &Value,
    files: bool,
    id: &Value,
    metadata: Value,
    page_time: Duration,
    run: impl for<'a> Fn(&[&[String]], &GroupSearch<'a>) -> Vec<Result<Searched>>,
) -> Result<Value> {
    validate_query(query, files)?;
    if !files {
        // Filename membership is identical across eligibilities; content is not.
        let requested = query
            .get("content_eligibility")
            .map(|value| serde_json::from_value::<ContentEligibility>(value.clone()))
            .transpose()?
            .unwrap_or_default();
        ensure!(
            requested == source.eligibility()?,
            "view content eligibility differs from the query; scan instead"
        );
    }
    let scope = crate::serve::SearchScope::parse(query).map_err(anyhow::Error::msg)?;
    let scoped_root = source.root().join(&scope.prefix);
    ensure!(scoped_root.is_dir(), "scope must be an existing directory");
    ensure!(
        super::worktree_root(&scoped_root)? == source.root()
            && fs::canonicalize(&scoped_root)?.starts_with(source.root()),
        "scope crosses a repository boundary"
    );
    let start = Instant::now();
    let mut budget = crate::serve::JsonBudget {
        remaining: MAX_RESPONSE as usize - 1024,
    };
    budget.charge(&json!({"jsonrpc":"2.0","id":id,"result":metadata}))?;
    let page = query.get("page");
    let paged = page.is_some();
    let cursor = page.and_then(Value::as_str);
    let (mut result, epoch) = if files {
        let (paths, next, epoch) = source.snapshot(|snapshot| -> Result<_> {
            let listed = snapshot.files(&scope.prefix, scope.hidden);
            if paged {
                reserve_cursor(&mut budget, &listed)?;
            }
            let mut paths = Vec::new();
            let mut next = None;
            for path in listed {
                if cursor.is_some_and(|cursor| path.as_str() < cursor)
                    || scope.relative(&path).is_none()
                {
                    continue;
                }
                match budget.charge(&path) {
                    Ok(()) => paths.push(path),
                    Err(error)
                        if paged
                            && !paths.is_empty()
                            && error.is::<crate::serve::ResponseLimit>() =>
                    {
                        next = Some(path);
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok((paths, next, snapshot.epoch()))
        })??;
        let mut result = json!({"files":paths, "epoch":epoch});
        if paged {
            result["next_page"] = json!(next);
        }
        (result, epoch)
    } else {
        let request = crate::serve::parse_search_params(query).map_err(anyhow::Error::msg)?;
        let (paths, total, epoch) = source.snapshot(|snapshot| {
            (
                snapshot.candidates(&request.plan, &scope.prefix, scope.hidden),
                request.opts.stats.then(|| snapshot.files("", true).len()),
                snapshot.epoch(),
            )
        })?;
        let raw_count = paths.len();
        let paths: Vec<_> = paths
            .into_iter()
            .filter(|path| {
                scope.relative(path).is_some_and(|relative| {
                    crate::search::passes_filters(
                        relative,
                        &request.glob_filter,
                        &request.type_filter,
                        request.ordered_filters,
                    )
                })
            })
            .collect();
        let index_stats = json!({"query_plan":crate::search::plan_summary(&request.plan),
                "raw_candidates":raw_count,"candidates":paths.len(),"total_files":total});
        budget.charge(&index_stats)?;
        let first = cursor.map_or(0, |cursor| {
            paths.partition_point(|path| path.as_str() < cursor)
        });
        let paths = &paths[first..];
        if paged {
            reserve_cursor(&mut budget, paths)?;
        }
        let groups: Vec<_> = tgrep_core::rooted::sibling_batches(paths).collect();
        let mut rows = Vec::new();
        let mut stats = Vec::new();
        let mut search_stats = Vec::new();
        let mut searched = 0_usize;
        let mut next = None;
        let width = rayon::current_num_threads().max(1) * 4;
        'page: for (number, window) in groups.chunks(width).enumerate() {
            if paged && number > 0 && start.elapsed() >= page_time {
                next = Some(&window[0][0]);
                break;
            }
            let search = GroupSearch::new(&request, budget.remaining);
            let results = run(window, &search)
                .into_iter()
                .collect::<Result<Vec<_>>>()?;
            for (siblings, found) in window.iter().zip(results) {
                let mut found = found.into_iter();
                for relative in siblings.iter() {
                    let found = match found.next() {
                        Some(found) => Some(found),
                        // A page always makes progress unless one file alone
                        // exceeds the response cap.
                        None if paged && searched == 0 => {
                            GroupSearch::new(&request, budget.remaining)
                                .search(source, std::slice::from_ref(relative))?
                                .pop()
                        }
                        None => None,
                    };
                    let Some((found, bytes)) =
                        found.filter(|(_, bytes)| *bytes <= budget.remaining)
                    else {
                        if !paged || searched == 0 {
                            return Err(crate::serve::ResponseLimit.into());
                        }
                        next = Some(relative);
                        break 'page;
                    };
                    budget.remaining -= bytes;
                    searched += 1;
                    rows.extend(found.rows);
                    stats.extend(found.stats);
                    search_stats.extend(found.search_stats);
                }
            }
        }
        let mut result = json!({
            "matches":rows, "file_stats":stats, "epoch":epoch,
            "elapsed_ms":start.elapsed().as_secs_f64()*1000.0,
            "index_stats":index_stats
        });
        if request.opts.search_stats {
            result["search_stats"] = json!(search_stats);
        }
        if paged {
            result["next_page"] = json!(next);
        }
        (result, epoch)
    };
    // The final snapshot re-verifies the pinned root. Candidates come from one
    // publication either way; eventual sources read live bytes and requeue
    // misses, so only strict sources reject a newer publication.
    let current = source.snapshot(|snapshot| snapshot.epoch())?;
    ensure!(
        source.eventual() || current == epoch,
        "view changed during query; retry or scan"
    );
    for (key, value) in metadata.as_object().expect("metadata object") {
        result[key] = value.clone();
    }
    Ok(result)
}

/// Keep room for the `next_page` cursor that may end this page.
fn reserve_cursor(budget: &mut crate::serve::JsonBudget, paths: &[String]) -> Result<()> {
    let longest = paths.iter().map(String::len).max().unwrap_or(0);
    budget.remaining = budget
        .remaining
        .checked_sub(longest.saturating_mul(6).saturating_add(32))
        .ok_or(crate::serve::ResponseLimit)?;
    Ok(())
}

/// Only an open, stat or read failure of the candidate itself closes the view
/// for full repair. Snapshot validation, deadline and cancellation failures say
/// nothing about the file, so they propagate unchanged and leave the view ready.
/// Eventual sources instead skip the candidate (`None`) and requeue its path.
fn candidate_failed<T>(
    source: &impl QuerySource,
    relative: &str,
    error: anyhow::Error,
) -> Result<Option<T>> {
    if source.eventual() {
        source.requeue(&[relative.to_owned()])?;
        return Ok(None);
    }
    let error = error.context(format!("reading shared candidate {relative}"));
    source.invalidate(&error)?;
    Err(error)
}

fn candidate_snapshot<T>(
    source: &impl QuerySource,
    relative: &str,
    open: impl FnOnce(WorktreeSnapshot<'_>) -> std::io::Result<T>,
) -> Result<Option<T>> {
    let mut opened = None;
    let snapshot = source.snapshot(|snapshot| opened = Some(open(snapshot)));
    match opened {
        // A failed open also fails the snapshot; keep the snapshot's typed error.
        Some(Err(error)) => candidate_failed(
            source,
            relative,
            snapshot.err().unwrap_or_else(|| error.into()),
        ),
        Some(Ok(file)) => {
            snapshot?;
            Ok(Some(file))
        }
        None => {
            snapshot?;
            bail!("shared snapshot did not open candidate {relative}");
        }
    }
}

fn read_candidate(
    source: &impl QuerySource,
    batch: Option<&tgrep_core::worktrees::CandidateBatch>,
    relative: &str,
) -> Result<Option<Vec<u8>>> {
    let failed = |error| candidate_failed(source, relative, error);
    let Some(file) = candidate_snapshot(source, relative, |snapshot| match batch {
        Some(batch) => snapshot.open_candidate_in(batch, relative),
        None => snapshot.open_candidate(relative),
    })?
    else {
        return Ok(None);
    };
    match file.metadata() {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return failed(anyhow::anyhow!(
                "candidate is no longer a regular file: {relative}"
            ));
        }
        Err(error) => return failed(error.into()),
    }
    let mut file = file.take(64 * 1024 * 1024 + 1);
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        source.check()?;
        match file.read(&mut chunk) {
            Ok(0) => return Ok(Some(bytes)),
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return failed(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_projection_query_fields_require_booleans() {
        for field in ["count", "count_matches", "search_stats"] {
            for value in [json!(true), json!(false)] {
                let query = json!({"pattern":"return",field:value});
                validate_query(&query, false).unwrap();
                assert!(validate_query(&query, true).is_err());
            }
            for value in [Value::Null, json!(1), json!("true"), json!([])] {
                assert!(validate_query(&json!({"pattern":"return",field:value}), false).is_err());
            }
        }
    }

    #[test]
    fn response_budgets_count_escaped_rows_paths_and_serialized_envelopes() {
        let request = crate::serve::parse_search_params(&json!({
            "pattern":"x", "detail":true, "positions":true, "stats":true, "search_stats":true,
            "before_context":1, "after_context":1
        }))
        .unwrap();
        let file = crate::serve::DecodedFile::new(
            "context\t\\\"\nx x\u{1} \"\\\nlast context\n"
                .repeat(20)
                .into_bytes(),
            request.encoding,
        );
        let path = "src/\"escaped\\path\t.rs";
        let expected =
            crate::serve::search_file_matches(path, &file, &request.matcher, &request.opts)
                .unwrap();
        let required = expected
            .rows
            .iter()
            .map(|row| serde_json::to_vec(row).unwrap().len() + 1)
            .sum::<usize>()
            + serde_json::to_vec(expected.stats.as_ref().unwrap())
                .unwrap()
                .len()
            + 1
            + serde_json::to_vec(expected.search_stats.as_ref().unwrap())
                .unwrap()
                .len()
            + 1;
        let mut budget = crate::serve::JsonBudget {
            remaining: required,
        };
        let result = crate::serve::search_file_matches_bounded(
            path,
            &file,
            &request.matcher,
            &request.opts,
            Some(&mut budget),
        )
        .unwrap();
        assert_eq!(result.rows, expected.rows);
        assert_eq!(budget.remaining, 0);
        let mut short = crate::serve::JsonBudget {
            remaining: required - 1,
        };
        assert!(
            crate::serve::search_file_matches_bounded(
                path,
                &file,
                &request.matcher,
                &request.opts,
                Some(&mut short)
            )
            .is_err()
        );
        let found = crate::matching::FileMatches::find(
            &file.text,
            &request.matcher,
            &request.opts.match_options(),
        )
        .unwrap();
        let mut rows = Vec::new();
        let mut tiny = crate::serve::JsonBudget {
            remaining: serde_json::to_vec(&expected.rows[0]).unwrap().len() + 1,
        };
        assert!(
            crate::serve::collect_match_rows_bounded(
                &found,
                &request.opts.match_options(),
                &request.matcher,
                path,
                &file.text,
                &file.fixups,
                true,
                true,
                &mut rows,
                Some(&mut tiny)
            )
            .is_err()
        );
        assert_eq!(
            rows.len(),
            1,
            "stop within one file rather than building every row first"
        );

        let paths = vec![path, "other/\ncontrol\u{1}.txt"];
        let required = paths
            .iter()
            .map(|path| serde_json::to_vec(path).unwrap().len() + 1)
            .sum();
        let mut budget = crate::serve::JsonBudget {
            remaining: required,
        };
        for path in &paths {
            budget.charge(path).unwrap();
        }
        assert_eq!(budget.remaining, 0);
        assert!(budget.charge(&"").is_err());
        for value in [
            json!({"files":paths}),
            json!({"matches":expected.rows,"file_stats":expected.stats,"search_stats":expected.search_stats}),
        ] {
            let envelope = json!({"jsonrpc":"2.0","id":"\"\\\n","result":value});
            let expected = serde_json::to_vec(&envelope).unwrap();
            assert_eq!(
                bounded_response(&envelope, expected.len()).unwrap(),
                expected
            );
            assert!(bounded_response(&envelope, expected.len() - 1).is_err());
        }
    }

    #[test]
    fn json_search_stats_charge_no_output_candidates_to_response_budget() {
        for bytes in [b"absent\n".as_slice(), b"absent\0\n".as_slice()] {
            let request = crate::serve::parse_search_params(&json!({
                "pattern":"needle", "search_stats":true
            }))
            .unwrap();
            let file = crate::serve::DecodedFile::new(bytes.to_vec(), request.encoding);
            let result = crate::serve::search_file_matches(
                "no-output.txt",
                &file,
                &request.matcher,
                &request.opts,
            )
            .unwrap();
            assert!(result.rows.is_empty());
            assert!(result.stats.is_none());
            let work = result.search_stats.unwrap();
            assert_eq!(work.file, "no-output.txt");
            let binary_offset = bytes.iter().position(|&b| b == 0).map(|n| n as u64);
            assert_eq!(work.binary_offset, binary_offset);
            assert_eq!(
                work.bytes_searched,
                binary_offset.unwrap_or(bytes.len() as u64)
            );
            let required = serde_json::to_vec(&work).unwrap().len() + 1;
            let mut exact = crate::serve::JsonBudget {
                remaining: required,
            };
            crate::serve::search_file_matches_bounded(
                "no-output.txt",
                &file,
                &request.matcher,
                &request.opts,
                Some(&mut exact),
            )
            .unwrap();
            assert_eq!(exact.remaining, 0);
            let mut short = crate::serve::JsonBudget {
                remaining: required - 1,
            };
            assert!(
                crate::serve::search_file_matches_bounded(
                    "no-output.txt",
                    &file,
                    &request.matcher,
                    &request.opts,
                    Some(&mut short),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn page_cursors_are_optional_strings_and_reserve_their_reply_space() {
        for (files, query) in [(false, json!({"pattern":"x"})), (true, json!({}))] {
            for page in [Value::Null, json!("src/a.rs")] {
                let mut query = query.clone();
                query["page"] = page;
                validate_query(&query, files).unwrap();
            }
            for page in [json!(1), json!([]), json!(true)] {
                let mut query = query.clone();
                query["page"] = page;
                assert!(validate_query(&query, files).is_err());
            }
        }

        let paths = vec!["a".to_string(), "dir/\u{1}".to_string()];
        let longest = serde_json::to_vec(&paths[1]).unwrap().len();
        let mut budget = crate::serve::JsonBudget { remaining: 1000 };
        reserve_cursor(&mut budget, &paths).unwrap();
        assert!(1000 - budget.remaining >= longest + 16);
        let mut short = crate::serve::JsonBudget { remaining: 8 };
        let error = reserve_cursor(&mut short, &paths).unwrap_err();
        assert!(error.is::<crate::serve::ResponseLimit>());

        let mut budget = crate::serve::JsonBudget { remaining: 5 };
        let error = budget.charge(&"too long").unwrap_err();
        assert!(error.is::<crate::serve::ResponseLimit>());
        assert!(error.to_string().contains("response exceeds 64 MiB"));
        assert_eq!(budget.remaining, 5, "a failed charge reserves nothing");
        budget.charge(&"ab").unwrap();
        assert_eq!(budget.remaining, 0);
    }

    #[test]
    fn failed_reconcile_backoff_is_capped_and_success_resets_it() {
        assert_eq!(retry_delay(0), Duration::from_millis(200));
        for (failures, seconds) in [
            (1, 1),
            (2, 2),
            (3, 4),
            (4, 8),
            (5, 16),
            (6, 30),
            (u32::MAX, 30),
        ] {
            assert_eq!(retry_delay(failures), Duration::from_secs(seconds));
        }
    }

    #[test]
    fn native_overflow_and_unknown_notifications_require_full_repair() {
        let changed = Event::new(EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Content,
        )))
        .add_path(PathBuf::from("file.txt"));
        assert!(!event_requires_full(&changed));
        assert!(event_requires_full(
            &changed.clone().set_flag(notify::event::Flag::Rescan)
        ));
        assert!(event_requires_full(
            &Event::new(EventKind::Any).add_path(PathBuf::from("file.txt"))
        ));
        assert!(event_requires_full(&Event::new(EventKind::Other)));
    }
}
