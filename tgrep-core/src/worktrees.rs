//! Pinned committed-tree bases with synchronized, private checkout overlays.
//!
//! Subscribe to changes after construction and before the first reconciliation.
//! Invalidations immediately close the query gate. A successful refresh covers
//! processed notifications, not an atomic filesystem snapshot: missed events
//! require periodic [`WorktreeView::reconcile_full`] calls. No native watcher,
//! CLI routing, content cache, base migration, or generation GC is provided.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Instant;

use crate::generations::{Generation, GenerationError, Repository};
use crate::gitignore::CaseInsensitiveIgnore;
use crate::hybrid::HybridIndex;
use crate::live::LiveIndex;
use crate::meta::{ContentId, FileVersion, file_version};
use crate::query::QueryPlan;
use crate::rooted::{OpenedFile, ParentObservation, RootedBatch, RootedDir};
use crate::trigram::TrigramMaskMap;
use crate::visibility::PathVisibility;
use crate::walker::{self, MetaWalkOptions};

pub type Result<T> = std::result::Result<T, WorktreeError>;

/// Reconciliation failures leave queries unavailable; callers may retry or scan.
#[derive(Debug)]
pub enum WorktreeError {
    Io(std::io::Error),
    Generation(GenerationError),
    Index(crate::Error),
    InvalidInput(String),
    IncompleteWalk(usize),
    UnstableFile(PathBuf),
    NotReady,
    ChangedDuringReconcile,
    Synchronization,
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "worktree I/O error: {error}"),
            Self::Generation(error) => error.fmt(f),
            Self::Index(error) => error.fmt(f),
            Self::InvalidInput(reason) => write!(f, "invalid worktree input: {reason}"),
            Self::IncompleteWalk(count) => write!(f, "worktree discovery had {count} errors"),
            Self::UnstableFile(path) => write!(f, "file changed during read: {}", path.display()),
            Self::NotReady => write!(f, "worktree view is not ready; reconcile or scan"),
            Self::ChangedDuringReconcile => write!(f, "worktree invalidated during reconciliation"),
            Self::Synchronization => write!(f, "worktree synchronization lock is poisoned"),
        }
    }
}

impl std::error::Error for WorktreeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Generation(error) => Some(error),
            Self::Index(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for WorktreeError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<GenerationError> for WorktreeError {
    fn from(error: GenerationError) -> Self {
        Self::Generation(error)
    }
}
impl From<crate::Error> for WorktreeError {
    fn from(error: crate::Error) -> Self {
        Self::Index(error)
    }
}

#[derive(Clone, Debug)]
pub struct WorktreeOptions {
    /// Canonical coverage always includes hidden files. Query visibility is
    /// applied separately. Size is checked against bytes actually read.
    pub walk: MetaWalkOptions,
    /// Maximum distinct queued paths; overflow forces full reconciliation.
    pub hint_capacity: usize,
    /// Optional existing, dedicated private directory. The entire directory
    /// is excluded from discovery, including atomic-checkpoint staging files.
    /// Must not be a worktree ancestor, Git metadata, or an index snapshot.
    pub checkpoint_directory: Option<PathBuf>,
    /// Eventually consistent serving. Once a state has been published,
    /// snapshots keep answering from it while invalidations are pending and
    /// while a refresh runs. A refresh that observes concurrent invalidations
    /// publishes what it reconciled and leaves only the newer hints queued,
    /// rather than discarding the pass. Candidate open failures queue path
    /// hints instead of failing the query. `ready` still means caught up.
    pub stale_reads: bool,
}

impl Default for WorktreeOptions {
    fn default() -> Self {
        Self {
            walk: MetaWalkOptions::default(),
            hint_capacity: 4096,
            checkpoint_directory: None,
            stale_reads: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeStatus {
    /// Published state reflects every invalidation observed so far.
    pub ready: bool,
    /// Snapshots are answered, possibly from a state older than `epoch`.
    #[serde(default)]
    pub serving: bool,
    /// Advances for invalidations and at the start of every reconciliation.
    pub epoch: u64,
    pub published_epoch: Option<u64>,
    pub pending_paths: usize,
    pub full_required: bool,
    #[serde(default)]
    pub tree_required: bool,
}

/// Work performed by one successful refresh (including no-op verification).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReconcileStats {
    pub epoch: u64,
    pub full: bool,
    pub files_discovered: usize,
    /// Searchable files provisionally served from the pinned generation before
    /// checkout content has been verified.
    pub provisional_files: u64,
    pub files_read: u64,
    pub bytes_read: u64,
    pub files_decoded: u64,
    /// Actual calls to masked trigram extraction, including short/empty files.
    pub files_extracted: u64,
    /// Searchable paths using the pinned base, including cached hint-pass paths.
    pub base_reused: u64,
    /// Renames/copies populated from content-identical base postings.
    pub base_files_copied: u64,
    pub postings_copied: u64,
    /// Existing private postings reused without extraction.
    pub overlay_reused: u64,
    /// Prior verified content retained during an event-driven hint pass.
    pub content_reads_avoided: u64,
    /// Prior verified content retained directly from trusted walk metadata.
    pub metadata_reused: u64,
    /// Ordered-set probes for hinted paths/ancestors, bounded by path depth
    /// rather than the number of pending hints. Full passes make no probes.
    pub hint_lookups: u64,
    pub batches: u64,
    pub batch_retries: u64,
    pub parent_batches: u64,
    pub control_checks: u64,
    pub namespace_catalog_calls: u64,
    pub peak_workers: u64,
    pub peak_parent_handles: u64,
    pub peak_read_bytes: u64,
    pub peak_private_bytes: u64,
    pub timings: ReconcileTimings,
}

/// Microseconds. Phase durations are wall time; worker_content_us and read_wait_us
/// are aggregate worker time. Catalog/control checks overlap the enclosing phases.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReconcileTimings {
    pub queue_us: u64,
    pub discovery_us: u64,
    pub planning_us: u64,
    pub content_verification_us: u64,
    pub worker_content_us: u64,
    pub worker_parent_us: u64,
    pub worker_open_us: u64,
    pub worker_read_verify_us: u64,
    pub worker_decode_hash_us: u64,
    pub worker_compare_extract_us: u64,
    pub parent_verify_us: u64,
    pub assembly_us: u64,
    pub checkpoint_us: u64,
    pub publication_prepare_us: u64,
    pub catalog_control_us: u64,
    pub namespace_catalog_us: u64,
    pub base_materialization_us: u64,
    pub worker_wait_us: u64,
    pub parent_wait_us: u64,
    pub read_wait_us: u64,
    pub admitted_us: u64,
}

impl ReconcileStats {
    fn add_verification(&mut self, other: &Self) {
        self.files_read += other.files_read;
        self.bytes_read += other.bytes_read;
        self.files_decoded += other.files_decoded;
        self.files_extracted += other.files_extracted;
        self.base_reused += other.base_reused;
        self.base_files_copied += other.base_files_copied;
        self.overlay_reused += other.overlay_reused;
        self.content_reads_avoided += other.content_reads_avoided;
        self.metadata_reused += other.metadata_reused;
        self.provisional_files += other.provisional_files;
        self.hint_lookups += other.hint_lookups;
        self.parent_batches += other.parent_batches;
    }
}

impl ReconcileTimings {
    fn add_worker(&mut self, other: &Self) {
        self.worker_content_us += other.worker_content_us;
        self.worker_parent_us += other.worker_parent_us;
        self.worker_open_us += other.worker_open_us;
        self.worker_read_verify_us += other.worker_read_verify_us;
        self.worker_decode_hash_us += other.worker_decode_hash_us;
        self.worker_compare_extract_us += other.worker_compare_extract_us;
    }
}

fn timed<T>(micros: &mut u64, work: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let result = work();
    *micros += crate::managed::work::micros(start.elapsed());
    result
}

struct Control {
    epoch: u64,
    published_epoch: Option<u64>,
    ready: bool,
    full: bool,
    tree: bool,
    hints: BTreeSet<String>,
}

impl Control {
    fn invalidate(&mut self) -> Result<u64> {
        self.ready = false;
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or(WorktreeError::Synchronization)?;
        Ok(self.epoch)
    }
}

#[derive(Clone)]
struct Evidence {
    /// Present only when validated around the read that produced this content.
    version: Option<FileVersion>,
    listed: bool,
    content: Option<ContentId>,
}

struct State {
    index: HybridIndex,
    evidence: BTreeMap<String, Evidence>,
    listed: BTreeSet<String>,
    visibility: PathVisibility,
    restored_content: BTreeMap<String, ContentId>,
    memory: Option<crate::managed::memory::RetainedMemory>,
    cost: OverlayCost,
}

/// One canonical worktree identity and an exact, lifetime-pinned generation.
///
/// Construction accepts the worktree root only (not a subdirectory, bare
/// repository, or another clone). No mutable index/flush API is exposed.
pub struct WorktreeView {
    root: PathBuf,
    files: Arc<RootedDir>,
    root_protection: Option<Arc<crate::managed::roots::RootProtection>>,
    repository: Repository,
    generation: Arc<Generation>,
    options: WorktreeOptions,
    control: Mutex<Control>,
    state: RwLock<State>,
    reconcile: Mutex<()>,
    /// Set by a serving verification as soon as it observes any difference
    /// from the published state. Closes readiness without taking `control`
    /// from verification workers, which hold `state` for reading.
    stale: AtomicBool,
}

/// Outcome of a metadata or content verification of a ready, published view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verification {
    /// Not ready, or invalidations/hints are queued; ordinary refresh owns it.
    Ineligible,
    /// Every admitted file matched the selected verification tier. The view
    /// stayed ready throughout and its epoch is unchanged.
    Unchanged(ReconcileStats),
    /// Content matched, but newer hints still need reconciliation. Verified
    /// metadata is retained so that the follow-up need not reopen every file.
    Superseded(ReconcileStats),
    /// A difference closed readiness and the verified state was published.
    Published(ReconcileStats),
}

/// Fails a verification closed if it observed a difference but did not
/// publish the repaired state, including on panic.
struct StaleGuard<'a> {
    view: &'a WorktreeView,
    reread: bool,
}

impl Drop for StaleGuard<'_> {
    fn drop(&mut self) {
        if self.view.stale.load(Ordering::Acquire) {
            let mut control = self
                .view
                .control
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            control.full |= self.reread;
            control.tree = true;
            if control.invalidate().is_err() {
                control.ready = false;
            }
            self.view.stale.store(false, Ordering::Release);
        }
    }
}

/// An eventually consistent pass takes its hints up front; failure or panic
/// before publication must put the original work back beside any newer hints.
struct TakenHints<'a> {
    view: &'a WorktreeView,
    armed: bool,
    full: bool,
    hints: BTreeSet<String>,
}

impl Drop for TakenHints<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut control = self
                .view
                .control
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            control.full |= self.full;
            control.tree = true;
            for hint in &self.hints {
                if !control.full
                    && !queue_hint(
                        &mut control.hints,
                        hint.clone(),
                        self.view.options.hint_capacity,
                    )
                {
                    control.full = true;
                }
            }
            if control.full {
                control.hints.clear();
            }
            control.ready = false;
        }
    }
}

impl WorktreeView {
    pub fn new(root: &Path, generation: Arc<Generation>, options: WorktreeOptions) -> Result<Self> {
        Self::new_inner(root, generation, options, None)
    }

    pub(crate) fn new_controlled(
        root: &Path,
        generation: Arc<Generation>,
        options: WorktreeOptions,
        permit: &Arc<crate::managed::WorkPermit>,
    ) -> Result<Self> {
        let mut view = Self::new_inner(
            root,
            generation,
            options,
            Some(&crate::managed::process::Control::work(permit)),
        )?;
        if view.root_protection.is_none() {
            view.root_protection = Some(
                crate::managed::roots::RootProtection::in_namespace(
                    &permit.namespace.directory,
                    &view.files,
                    &view.root,
                )
                .map_err(|error| WorktreeError::Index(error.into()))?,
            );
        }
        Ok(view)
    }

    fn new_inner(
        root: &Path,
        generation: Arc<Generation>,
        mut options: WorktreeOptions,
        control: Option<&crate::managed::process::Control>,
    ) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let repository = match control {
            Some(control) => Repository::discover_controlled(&root, control)?,
            None => Repository::discover(&root)?,
        };
        let worktree_root = match control {
            Some(control) => Repository::root_controlled(&root, control)?,
            None => crate::generations::worktree_root(&root)?,
        };
        if repository.identity() != generation.key().repository_identity() || worktree_root != root
        {
            return Err(WorktreeError::InvalidInput(
                "root must be a worktree root in the pinned repository".into(),
            ));
        }
        for path in &mut options.walk.exclude_paths {
            *path = absolute_exclusion(&root, path)?;
        }
        // Exclude actual metadata directories, not a linked worktree's plain
        // .git file: that file follows normal walker visibility and ignore rules.
        options.walk.exclude_paths.extend([
            root.join(".tgrep"),
            repository.common_dir().to_path_buf(),
            repository.git_dir().to_path_buf(),
            generation
                .directory()
                .parent()
                .expect("generation store")
                .to_path_buf(),
        ]);
        if let Some(directory) = &mut options.checkpoint_directory {
            *directory = fs::canonicalize(root.join(&directory))?;
            validate_checkpoint_directory(directory, &root, &repository, &generation)?;
            options.walk.exclude_paths.push(directory.clone());
        }
        let index = generation.base().create_worktree(&root)?;
        let files = Arc::new(RootedDir::open(&root)?);
        let root_protection = generation
            .base()
            .reader()
            .managed_guard()
            .map(|guard| {
                crate::managed::roots::RootProtection::acquire(guard, &files, &root)
                    .map_err(|error| WorktreeError::Index(error.into()))
            })
            .transpose()?;
        Ok(Self {
            root,
            files,
            root_protection,
            repository,
            generation,
            options,
            control: Mutex::new(Control {
                epoch: 0,
                published_epoch: None,
                ready: false,
                full: true,
                tree: false,
                hints: BTreeSet::new(),
            }),
            state: RwLock::new(State {
                index,
                evidence: BTreeMap::new(),
                listed: BTreeSet::new(),
                visibility: PathVisibility::default(),
                restored_content: BTreeMap::new(),
                memory: None,
                cost: OverlayCost::default(),
            }),
            reconcile: Mutex::new(()),
            stale: AtomicBool::new(false),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn repository(&self) -> &Repository {
        &self.repository
    }

    pub fn generation(&self) -> &Arc<Generation> {
        &self.generation
    }

    pub(crate) fn replacement(
        &self,
        generation: Arc<Generation>,
        permit: &Arc<crate::managed::WorkPermit>,
    ) -> Result<Self> {
        self.files.verify_root()?;
        let mut replacement =
            Self::new_controlled(&self.root, generation, self.options.clone(), permit)?;
        self.files.verify_root()?;
        replacement.files = Arc::clone(&self.files);
        replacement.root_protection = self.root_protection.clone();
        Ok(replacement)
    }

    pub fn root_anchor(&self) -> Option<&crate::managed::RootAnchor> {
        self.root_protection
            .as_ref()
            .map(|protection| &protection.anchor)
    }

    pub(crate) fn root_identity(&self) -> crate::managed::Result<crate::managed::FileIdentity> {
        crate::managed::FileIdentity::of(&self.files.directory_handle()?)
    }

    pub fn status(&self) -> Result<WorktreeStatus> {
        let control = self
            .control
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        let stale = self.stale.load(Ordering::Acquire);
        Ok(WorktreeStatus {
            ready: control.ready && !stale,
            serving: self.serving(&control),
            epoch: control.epoch,
            published_epoch: control.published_epoch,
            pending_paths: control.hints.len(),
            full_required: control.full || stale,
            tree_required: control.tree || stale,
        })
    }

    /// Whether this view uses [`WorktreeOptions::stale_reads`].
    pub fn stale_reads(&self) -> bool {
        self.options.stale_reads
    }

    /// Whether the regular file at `relative`, described by `metadata`, has
    /// trusted indexed evidence and recorded hidden status equal to its
    /// current state. Watchers may drop metadata-only notifications for such
    /// files; a content change that preserves size and timestamps is left to
    /// full verification.
    pub fn unchanged_file(&self, relative: &Path, metadata: &std::fs::Metadata) -> bool {
        let version = crate::meta::file_version(metadata);
        if !metadata.is_file() || !version.is_trusted() {
            return false;
        }
        let Ok(relative) = relative_path(relative) else {
            return false;
        };
        let hidden = crate::visibility::is_hidden(Path::new(&relative), Some(metadata));
        self.state.read().is_ok_and(|state| {
            state.visibility.records_hidden(&relative) == hidden
                && state
                    .evidence
                    .get(&relative)
                    .is_some_and(|evidence| evidence.version.as_ref() == Some(&version))
        })
    }

    /// Caught up: the published state reflects every observed invalidation.
    fn current(&self, control: &Control) -> bool {
        control.ready && !self.stale.load(Ordering::Acquire)
    }

    /// Snapshots and checkpoints may use the published state.
    fn serving(&self, control: &Control) -> bool {
        if self.options.stale_reads {
            control.published_epoch.is_some()
        } else {
            self.current(control)
        }
    }

    /// Queue a root-relative file or subtree, including missing/renamed paths.
    /// Pass both old and new rename paths. Invalid hints close the gate and
    /// force a full pass before returning an error; never silently discard one.
    /// Accepted repeated/trailing separators and interior `.` components are
    /// normalized without requiring the hinted path to still exist.
    /// Git/ignore configuration changes also rewalk membership. A
    /// successful refresh acknowledges this token with an equal or later epoch.
    pub fn invalidate_path(&self, path: &Path) -> Result<u64> {
        let mut control = self
            .control
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        let epoch = control.invalidate()?;
        let mut relative = match relative_path(path) {
            Ok(relative) => relative.to_ascii_lowercase(),
            Err(error) => {
                control.full = true;
                control.hints.clear();
                return Err(error);
            }
        };
        if !relative.is_ascii() {
            relative = relative
                .split('/')
                .take_while(|part| part.is_ascii())
                .collect::<Vec<_>>()
                .join("/");
        }
        if relative
            .split('/')
            .any(|part| matches!(part, ".git" | ".gitignore" | ".ignore" | ".gitattributes"))
        {
            control.tree = true;
        }
        if !control.full
            && (relative.is_empty()
                || !queue_hint(&mut control.hints, relative, self.options.hint_capacity))
        {
            control.full = true;
            control.hints.clear();
        }
        Ok(epoch)
    }

    /// Rewalk membership and metadata, retaining explicit content-read hints.
    pub fn invalidate_tree(&self) -> Result<u64> {
        let mut control = self
            .control
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        let epoch = control.invalidate()?;
        control.tree = true;
        Ok(epoch)
    }

    /// Startup, watcher overflow and explicit content repair use this
    /// invalidation. Safe to call while a pass is running.
    pub fn invalidate_all(&self) -> Result<u64> {
        let mut control = self
            .control
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        let epoch = control.invalidate()?;
        control.full = true;
        control.hints.clear();
        Ok(epoch)
    }

    /// Read all admitted checkout bytes, regardless of stat/Git status.
    pub fn reconcile_full(&self) -> Result<ReconcileStats> {
        self.invalidate_all()?;
        self.refresh()
    }

    /// Publish the pinned generation as a provisional first-attach snapshot.
    ///
    /// Membership and visibility come from a metadata walk, but indexed content
    /// still comes from the committed generation until the first full refresh.
    /// Provisional versions are deliberately absent so no later hinted pass can
    /// mistake an observed stat for evidence from an indexed read.
    pub(crate) fn bootstrap_base_controlled(
        &self,
        permit: &Arc<crate::managed::WorkPermit>,
    ) -> Result<ReconcileStats> {
        if !self.options.stale_reads
            || self
                .root_protection
                .as_ref()
                .is_none_or(|root| root.namespace != permit.namespace.header().namespace)
        {
            return Err(WorktreeError::InvalidInput(
                "provisional bootstrap requires an eventual view and its namespace reservation"
                    .into(),
            ));
        }
        let _serial = self
            .reconcile
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        let discovery = Instant::now();
        self.files.verify_root()?;
        let repository = Repository::discover_controlled(
            &self.root,
            &crate::managed::process::Control::work(permit),
        )?;
        let worktree_root = Repository::root_controlled(
            &self.root,
            &crate::managed::process::Control::work(permit),
        )?;
        if fs::canonicalize(&self.root)? != self.root
            || repository != self.repository
            || repository.git_dir() != self.repository.git_dir()
            || worktree_root != self.root
        {
            return Err(WorktreeError::InvalidInput(
                "worktree identity changed".into(),
            ));
        }
        let mut options = self.options.walk.clone();
        options.max_file_size = None;
        let (walk, mut memory) =
            walker::walk_file_metadata_controlled(&self.root, &options, &repository, permit)
                .map_err(crate::Error::from)?;
        if walk.skipped_error != 0 {
            return Err(WorktreeError::IncompleteWalk(walk.skipped_error));
        }
        let assembly = Instant::now();
        let mut sizes: HashMap<&str, u64> = walk
            .files
            .iter()
            .map(|file| (file.relative_path.as_str(), file.size))
            .collect();
        sizes.extend(
            walk.listed_sizes
                .iter()
                .map(|(path, size)| (path.as_str(), *size)),
        );
        memory
            .grow((walk.listed_files.len() as u64).saturating_mul(512))
            .map_err(crate::Error::from)?;
        let mut evidence = BTreeMap::new();
        let mut listed = BTreeSet::new();
        let mut stats = ReconcileStats {
            full: true,
            files_discovered: walk.listed_files.len(),
            timings: ReconcileTimings {
                discovery_us: crate::managed::work::micros(discovery.elapsed()),
                ..ReconcileTimings::default()
            },
            ..ReconcileStats::default()
        };
        for path in walk.listed_files {
            permit.check().map_err(crate::Error::from)?;
            let size = sizes
                .get(path.as_str())
                .copied()
                .ok_or(WorktreeError::IncompleteWalk(1))?;
            let is_listed = self
                .options
                .walk
                .max_file_size
                .is_none_or(|limit| size <= limit);
            let eligible = !self
                .generation
                .key()
                .profile()
                .eligibility
                .excludes_by_extension(Path::new(&path));
            let content = if is_listed && eligible {
                self.base_content(&path)
            } else {
                None
            };
            if is_listed {
                listed.insert(path.clone());
            }
            if content.is_some() {
                stats.provisional_files += 1;
                stats.base_reused += 1;
            }
            evidence.insert(
                path,
                Evidence {
                    version: None,
                    listed: is_listed,
                    content,
                },
            );
        }
        self.files.verify_root()?;
        permit.check_now().map_err(crate::Error::from)?;
        let mut control = self
            .control
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        if control.published_epoch.is_some() {
            return Err(WorktreeError::InvalidInput(
                "worktree already has a published snapshot".into(),
            ));
        }
        let epoch = control.epoch;
        let mut state = self
            .state
            .write()
            .map_err(|_| WorktreeError::Synchronization)?;
        if !state.evidence.is_empty() || !state.listed.is_empty() {
            return Err(WorktreeError::InvalidInput(
                "provisional bootstrap requires an empty worktree state".into(),
            ));
        }
        for path in self.generation.base().reader().all_paths() {
            permit.check().map_err(crate::Error::from)?;
            if evidence.get(path).and_then(|entry| entry.content).is_none() {
                state
                    .index
                    .live
                    .delete_file_controlled(path, permit, &mut memory)
                    .map_err(crate::Error::from)?;
            }
        }
        let bytes =
            retained_view_bytes(&state.index.live, evidence.keys(), &walk.visibility, permit)
                .map_err(crate::Error::from)?;
        memory.resize(bytes).map_err(crate::Error::from)?;
        state.memory = Some(memory.retain(0).map_err(crate::Error::from)?);
        state.listed = listed;
        state.evidence = evidence;
        state.visibility = walk.visibility;
        state.restored_content.clear();
        state.cost = measure_overlay_cost(&state, Some(permit)).map_err(crate::Error::from)?;
        stats.epoch = epoch;
        stats.timings.assembly_us = crate::managed::work::micros(assembly.elapsed());
        control.published_epoch = Some(epoch);
        control.ready = false;
        control.full = true;
        Ok(stats)
    }

    /// Rewalk membership/visibility and process pending hints. With no hints,
    /// performs a full content reconciliation, not a success-shaped no-op.
    ///
    /// At most one pass is attempted. A concurrent invalidation returns
    /// `ChangedDuringReconcile`, retaining the invalidation for the next
    /// call. Churn never causes unbounded retries or stale ready publication.
    /// Discovery/read failures likewise leave the gate closed and require a
    /// retry with its original hints. Matching content never re-extracts postings.
    ///
    /// With [`WorktreeOptions::stale_reads`], a concurrent invalidation does
    /// not discard the pass: it publishes what it reconciled, stays not ready,
    /// and leaves only the invalidations that arrived during the pass queued.
    pub fn refresh(&self) -> Result<ReconcileStats> {
        self.refresh_inner(|| {}, || {})
    }

    fn refresh_inner(
        &self,
        after_discovery: impl FnOnce(),
        before_publish: impl FnOnce(),
    ) -> Result<ReconcileStats> {
        self.refresh_controlled_inner(after_discovery, before_publish, None, false)
    }

    /// Reconcile a managed view under its namespace's bounded work reservation.
    /// Restored views must use this path before serving indexed queries.
    pub fn refresh_controlled(
        &self,
        permit: &Arc<crate::managed::WorkPermit>,
    ) -> Result<ReconcileStats> {
        if self
            .root_protection
            .as_ref()
            .is_none_or(|root| root.namespace != permit.namespace.header().namespace)
        {
            return Err(WorktreeError::InvalidInput(
                "reconciliation reservation belongs to another namespace".into(),
            ));
        }
        self.refresh_controlled_inner(|| {}, || {}, Some(permit), false)
    }

    /// Explicit managed refresh, including when path hints are already queued.
    pub fn reread_controlled(
        &self,
        permit: &Arc<crate::managed::WorkPermit>,
    ) -> Result<ReconcileStats> {
        if self
            .root_protection
            .as_ref()
            .is_none_or(|root| root.namespace != permit.namespace.header().namespace)
        {
            return Err(WorktreeError::InvalidInput(
                "reconciliation reservation belongs to another namespace".into(),
            ));
        }
        self.refresh_controlled_inner(|| {}, || {}, Some(permit), true)
    }

    fn refresh_controlled_inner(
        &self,
        after_discovery: impl FnOnce(),
        before_publish: impl FnOnce(),
        permit: Option<&Arc<crate::managed::WorkPermit>>,
        reread: bool,
    ) -> Result<ReconcileStats> {
        let _serial = self
            .reconcile
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        if permit.is_none()
            && self
                .state
                .read()
                .map_err(|_| WorktreeError::Synchronization)?
                .memory
                .is_some()
        {
            return Err(WorktreeError::InvalidInput(
                "managed views require a reserved reconciliation operation".into(),
            ));
        }
        let stale_reads = self.options.stale_reads;
        let (epoch, full, hints) = {
            let mut control = self
                .control
                .lock()
                .map_err(|_| WorktreeError::Synchronization)?;
            let epoch = control.invalidate()?;
            let full = reread || control.full || (!control.tree && control.hints.is_empty());
            if stale_reads {
                // Invalidations during this pass queue normally for the next.
                control.full = false;
                control.tree = false;
                (epoch, full, std::mem::take(&mut control.hints))
            } else {
                control.full |= full;
                control.tree = true;
                (epoch, full, control.hints.clone())
            }
        };
        let mut taken = TakenHints {
            view: self,
            armed: stale_reads,
            full,
            hints,
        };
        let mut prepared = self.prepare(full, &taken.hints, after_discovery, permit, false)?;
        before_publish();
        let assembly = Instant::now();
        self.check_parents(&mut prepared, permit)?;
        self.files.verify_root()?;
        if let Some(permit) = permit {
            permit.check_now().map_err(crate::Error::from)?;
        }
        let mut control = self
            .control
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        if control.epoch == u64::MAX {
            return Err(WorktreeError::Synchronization);
        }
        let current = control.epoch == epoch;
        if !current && !stale_reads {
            return Err(WorktreeError::ChangedDuringReconcile);
        }
        let stats =
            self.publish_prepared(&mut control, prepared, epoch, permit, assembly, current)?;
        taken.armed = false;
        Ok(stats)
    }

    /// Fully verify a ready view while it keeps serving its published state.
    ///
    /// Reads every admitted checkout file regardless of stat or Git status,
    /// rewalks membership/visibility and rechecks parents. Readiness closes
    /// as soon as any difference is observed; that pass then publishes the
    /// verified state. A concurrent invalidation leaves its hints queued;
    /// an otherwise unchanged pass returns `Verification::Superseded` and
    /// retains verified evidence for the next pass. Failures
    /// after an observed difference, and any failure other than budget
    /// cancellation, deadline or resource pressure, leave a full refresh
    /// required. Budget failures with no observed difference keep serving.
    pub fn verify_controlled(
        &self,
        permit: &Arc<crate::managed::WorkPermit>,
    ) -> Result<Verification> {
        if self
            .root_protection
            .as_ref()
            .is_none_or(|root| root.namespace != permit.namespace.header().namespace)
        {
            return Err(WorktreeError::InvalidInput(
                "verification reservation belongs to another namespace".into(),
            ));
        }
        self.verify_inner(|| {}, || {}, Some(permit))
    }

    /// Unmanaged equivalent of [`Self::verify_controlled`].
    pub fn verify(&self) -> Result<Verification> {
        self.verify_inner(|| {}, || {}, None)
    }

    /// Verify membership and trusted metadata, reading only changed or
    /// unverified files. Same-stat rewrites require the content tier.
    pub fn verify_metadata(&self) -> Result<Verification> {
        self.verify_tier(false, || {}, || {}, None)
    }

    pub fn verify_metadata_controlled(
        &self,
        permit: &Arc<crate::managed::WorkPermit>,
    ) -> Result<Verification> {
        if self
            .root_protection
            .as_ref()
            .is_none_or(|root| root.namespace != permit.namespace.header().namespace)
        {
            return Err(WorktreeError::InvalidInput(
                "verification reservation belongs to another namespace".into(),
            ));
        }
        self.verify_tier(false, || {}, || {}, Some(permit))
    }

    fn verify_inner(
        &self,
        after_discovery: impl FnOnce(),
        before_publish: impl FnOnce(),
        permit: Option<&Arc<crate::managed::WorkPermit>>,
    ) -> Result<Verification> {
        self.verify_tier(true, after_discovery, before_publish, permit)
    }

    fn verify_tier(
        &self,
        reread: bool,
        after_discovery: impl FnOnce(),
        before_publish: impl FnOnce(),
        permit: Option<&Arc<crate::managed::WorkPermit>>,
    ) -> Result<Verification> {
        let _serial = self
            .reconcile
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        if permit.is_none()
            && self
                .state
                .read()
                .map_err(|_| WorktreeError::Synchronization)?
                .memory
                .is_some()
        {
            return Err(WorktreeError::InvalidInput(
                "managed views require a reserved reconciliation operation".into(),
            ));
        }
        let epoch = {
            let control = self
                .control
                .lock()
                .map_err(|_| WorktreeError::Synchronization)?;
            if !self.current(&control)
                || control.full
                || control.tree
                || !control.hints.is_empty()
                || control.published_epoch != Some(control.epoch)
            {
                return Ok(Verification::Ineligible);
            }
            control.epoch
        };
        let guard = StaleGuard { view: self, reread };
        let result = self.verify_serving(epoch, after_discovery, before_publish, permit, reread);
        match &result {
            Ok(_) | Err(WorktreeError::ChangedDuringReconcile) => {}
            Err(error) if is_budget_failure(error) => {}
            Err(_) => self.stale.store(true, Ordering::Release),
        }
        drop(guard);
        result
    }

    fn verify_serving(
        &self,
        epoch: u64,
        after_discovery: impl FnOnce(),
        before_publish: impl FnOnce(),
        permit: Option<&Arc<crate::managed::WorkPermit>>,
        reread: bool,
    ) -> Result<Verification> {
        let mut prepared = self.prepare(reread, &BTreeSet::new(), after_discovery, permit, true)?;
        before_publish();
        let assembly = Instant::now();
        self.check_parents(&mut prepared, permit)?;
        self.files.verify_root()?;
        if let Some(permit) = permit {
            permit.check_now().map_err(crate::Error::from)?;
        }
        let mut control = self
            .control
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        if !self.stale.load(Ordering::Acquire) {
            self.state
                .write()
                .map_err(|_| WorktreeError::Synchronization)?
                .evidence = prepared.evidence;
            let mut stats = prepared.stats;
            if let Some(permit) = permit {
                permit.observe_reconciliation(&mut stats);
            }
            stats.timings.assembly_us += crate::managed::work::micros(assembly.elapsed());
            stats.epoch = epoch;
            return Ok(if control.epoch == epoch {
                Verification::Unchanged(stats)
            } else {
                Verification::Superseded(stats)
            });
        }
        if control.epoch != epoch {
            return Err(WorktreeError::ChangedDuringReconcile);
        }
        control.tree = true;
        control.full |= reread;
        let published = control.invalidate()?;
        self.publish_prepared(&mut control, prepared, published, permit, assembly, true)
            .map(Verification::Published)
    }

    fn check_parents(
        &self,
        prepared: &mut Prepared,
        permit: Option<&Arc<crate::managed::WorkPermit>>,
    ) -> Result<()> {
        timed(
            &mut prepared.stats.timings.parent_verify_us,
            || -> Result<()> {
                if let Some(permit) = permit {
                    let admission = permit
                        .parallelism(
                            rayon::current_num_threads()
                                .min(prepared.parents.len())
                                .max(1),
                        )
                        .map_err(crate::Error::from)?;
                    let checks: Vec<_> = prepared
                        .parents
                        .par_chunks(prepared.parents.len().div_ceil(admission.workers).max(1))
                        .map(|parents| -> Result<()> {
                            for parent in parents {
                                let _pins = permit
                                    .admit_parents(parent.verification_handles())
                                    .map_err(crate::Error::from)?;
                                let _memory = permit
                                    .admit_read(parent.verification_memory_bytes(&self.files))
                                    .map_err(crate::Error::from)?;
                                parent.check_current_ids(&self.files)?;
                            }
                            Ok(())
                        })
                        .collect();
                    let mut failure = None;
                    for check in checks {
                        if let Err(error) = check {
                            record_failure(&mut failure, error);
                        }
                    }
                    if let Some(error) = failure {
                        return Err(error);
                    }
                } else {
                    for parent in &prepared.parents {
                        parent.check_current_ids(&self.files)?;
                    }
                }
                Ok(())
            },
        )
    }

    /// Publish a fully prepared pass. Requires the reconcile mutex and the
    /// control guard. `current` means `epoch` is unchanged since the pass
    /// started; otherwise (stale reads only) newer invalidations stay queued.
    fn publish_prepared(
        &self,
        control: &mut Control,
        prepared: Prepared,
        epoch: u64,
        permit: Option<&Arc<crate::managed::WorkPermit>>,
        assembly: Instant,
        current: bool,
    ) -> Result<ReconcileStats> {
        let mut state = self
            .state
            .write()
            .map_err(|_| WorktreeError::Synchronization)?;
        let Prepared {
            evidence,
            visibility,
            upserts,
            mut stats,
            mut memory,
            parents: _,
        } = prepared;
        let mut replacement = match (permit, &mut memory) {
            (Some(permit), Some(memory)) => Some(
                state
                    .index
                    .live
                    .clone_controlled(permit, memory)
                    .map_err(crate::Error::from)?,
            ),
            _ => None,
        };
        let live = replacement.as_mut().unwrap_or(&mut state.index.live);
        let mut clear = Vec::new();
        for path in live.private_paths() {
            let content = evidence.get(path).and_then(|entry| entry.content);
            let base_content = self.base_content(path);
            if content == base_content {
                if let Some(memory) = &mut memory {
                    memory
                        .grow(path.len() as u64 * 2 + 64)
                        .map_err(crate::Error::from)?;
                }
                clear.push(path.to_owned());
            }
        }
        live.clear_reconciled_paths(&clear);
        for path in self.generation.base().reader().all_paths() {
            if let Some(permit) = permit {
                permit.check().map_err(crate::Error::from)?;
            }
            if evidence.get(path).and_then(|entry| entry.content).is_none()
                && !live.is_deleted(path)
            {
                if let (Some(permit), Some(memory)) = (permit, &mut memory) {
                    live.delete_file_controlled(path, permit, memory)
                        .map_err(crate::Error::from)?;
                } else {
                    live.delete_file(path);
                }
            }
        }
        for (path, upsert) in upserts {
            if let (Some(permit), Some(memory)) = (permit, &mut memory) {
                live.commit_upsert_controlled(&path, upsert.masks, permit, memory)
                    .map_err(crate::Error::from)?;
            } else {
                live.commit_upsert(&path, upsert.masks);
            }
            drop(upsert.memory);
        }
        let listed = evidence
            .iter()
            .filter(|(_, entry)| entry.listed)
            .map(|(path, _)| path.clone())
            .collect();
        let retained = match (permit, memory) {
            (Some(permit), Some(mut memory)) => {
                let bytes = retained_view_bytes(live, evidence.keys(), &visibility, permit)
                    .map_err(crate::Error::from)?;
                memory.resize(bytes).map_err(crate::Error::from)?;
                Some(memory.retain(0).map_err(crate::Error::from)?)
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            state.index.live = replacement;
        }
        state.listed = listed;
        state.evidence = evidence;
        state.visibility = visibility;
        state.restored_content.clear();
        state.memory = retained;
        state.cost = measure_overlay_cost(&state, permit).map_err(crate::Error::from)?;
        if let Some(permit) = permit {
            permit.check_now().map_err(crate::Error::from)?;
            permit.observe_reconciliation(&mut stats);
        }
        stats.timings.assembly_us += crate::managed::work::micros(assembly.elapsed());
        stats.epoch = epoch;
        control.published_epoch = Some(epoch);
        if current {
            control.full = false;
            control.tree = false;
            control.hints.clear();
            control.ready = true;
        }
        self.stale.store(false, Ordering::Release);
        Ok(stats)
    }

    fn base_content(&self, path: &str) -> Option<ContentId> {
        self.generation
            .entry(path)
            .and_then(|entry| entry.content_id())
    }

    fn prepare(
        &self,
        full: bool,
        hints: &BTreeSet<String>,
        after_discovery: impl FnOnce(),
        permit: Option<&Arc<crate::managed::WorkPermit>>,
        verify: bool,
    ) -> Result<Prepared> {
        let discovery = Instant::now();
        self.files.verify_root()?;
        let process = permit.map(crate::managed::process::Control::work);
        let repository = match &process {
            Some(control) => Repository::discover_controlled(&self.root, control)?,
            None => Repository::discover(&self.root)?,
        };
        let worktree_root = match &process {
            Some(control) => Repository::root_controlled(&self.root, control)?,
            None => crate::generations::worktree_root(&self.root)?,
        };
        if fs::canonicalize(&self.root)? != self.root
            || repository != self.repository
            || repository.git_dir() != self.repository.git_dir()
            || worktree_root != self.root
        {
            return Err(WorktreeError::InvalidInput(
                "worktree identity changed".into(),
            ));
        }
        let ignorecase = if self.options.walk.no_ignore || permit.is_some() {
            None
        } else {
            CaseInsensitiveIgnore::try_frozen_snapshot(&self.root)?.map(Arc::new)
        };
        let mut options = self.options.walk.clone();
        // Metadata alone must not permanently exclude a file whose actual
        // bytes have become small enough. Bound actual reads instead.
        options.max_file_size = None;
        let (walk, memory) = match permit {
            Some(permit) => {
                let (walk, memory) = walker::walk_file_metadata_controlled(
                    &self.root,
                    &options,
                    &repository,
                    permit,
                )
                .map_err(crate::Error::from)?;
                (walk, Some(memory))
            }
            None => (
                walker::walk_file_metadata_with_ignorecase(&self.root, &options, ignorecase),
                None,
            ),
        };
        if walk.skipped_error != 0 {
            return Err(WorktreeError::IncompleteWalk(walk.skipped_error));
        }
        after_discovery();
        let mut prepared = Prepared {
            memory,
            parents: Vec::new(),
            evidence: BTreeMap::new(),
            visibility: walk.visibility,
            upserts: BTreeMap::new(),
            stats: ReconcileStats {
                full,
                files_discovered: walk.listed_files.len(),
                timings: ReconcileTimings {
                    discovery_us: crate::managed::work::micros(discovery.elapsed()),
                    ..ReconcileTimings::default()
                },
                ..ReconcileStats::default()
            },
        };
        let planning = Instant::now();
        let state = self
            .state
            .read()
            .map_err(|_| WorktreeError::Synchronization)?;
        let base_contents = BaseContents::default();
        let mut copies: HashMap<u32, Vec<String>> = HashMap::new();
        let mut paths = walk.listed_files;
        if verify
            && (prepared.visibility != state.visibility
                || paths.len() != state.evidence.len()
                || paths.iter().any(|path| !state.evidence.contains_key(path)))
        {
            self.stale.store(true, Ordering::Release);
        }
        if let Some(memory) = &mut prepared.memory {
            memory
                .grow(paths.len() as u64 * 128)
                .map_err(crate::Error::from)?;
        }
        if !full {
            let versions: HashMap<_, _> = walk
                .files
                .iter()
                .filter_map(|file| {
                    file.version
                        .as_ref()
                        .map(|version| (file.relative_path.as_str(), version))
                })
                .chain(
                    walk.listed_versions
                        .iter()
                        .map(|(path, version)| (path.as_str(), version)),
                )
                .collect();
            paths.retain(|path| {
                let Some(previous) = state.evidence.get(path) else {
                    return true;
                };
                if previous
                    .version
                    .as_ref()
                    .is_none_or(|version| versions.get(path.as_str()).copied() != Some(version))
                    || is_hinted(path, hints, &mut prepared.stats.hint_lookups)
                {
                    return true;
                }
                prepared.stats.metadata_reused += 1;
                prepared.stats.content_reads_avoided += 1;
                if let Some(content) = previous.content {
                    if self.base_content(path) == Some(content) {
                        prepared.stats.base_reused += 1;
                    } else {
                        prepared.stats.overlay_reused += 1;
                    }
                }
                prepared.evidence.insert(path.clone(), previous.clone());
                false
            });
        }
        paths.sort_unstable_by(|left, right| {
            parent_path(left)
                .cmp(parent_path(right))
                .then_with(|| left.cmp(right))
        });
        let sizes: HashMap<_, _> = walk
            .files
            .iter()
            .map(|file| (file.relative_path.as_str(), file.size))
            .chain(
                walk.listed_sizes
                    .iter()
                    .map(|(path, size)| (path.as_str(), *size)),
            )
            .collect();
        let charges: Vec<_> = paths
            .iter()
            .map(|path| crate::builder::BatchCharge {
                heap: sizes.get(path.as_str()).map_or(
                    crate::builder::INDEX_BUILD_BATCH_BYTES,
                    |size| {
                        size.saturating_add(1)
                            .saturating_mul(6)
                            .saturating_add(64 * 1024)
                    },
                ),
                mapped: 0,
            })
            .collect();
        let mut ranges = Vec::new();
        for range in crate::builder::batch_ranges(&charges, crate::builder::INDEX_BUILD_BATCH_BYTES)
        {
            let mut start = range.start;
            let mut handles = 0;
            let mut previous = None;
            for index in range.clone() {
                let parent = parent_path(&paths[index]);
                if previous == Some(parent) {
                    continue;
                }
                let required = RootedDir::batch_handles(Path::new(parent));
                if index > start
                    && (permit.is_none()
                        || handles + required > crate::managed::work::RECONCILE_PARENT_HANDLES)
                {
                    ranges.push(start..index);
                    start = index;
                    handles = 0;
                }
                handles += required;
                previous = Some(parent);
            }
            ranges.push(start..range.end);
        }
        prepared.stats.timings.planning_us = crate::managed::work::micros(planning.elapsed());
        let verification = Instant::now();
        for range in ranges {
            let files = &paths[range];
            let mut wanted = rayon::current_num_threads().min(files.len());
            let chunks = loop {
                let admission = permit
                    .map(|permit| permit.parallelism(wanted))
                    .transpose()
                    .map_err(crate::Error::from)?;
                let mut batch = VerificationBatch::new(&self.files, files, &base_contents, permit)?;
                batch.verify = verify;
                let failure = Mutex::new(None);
                let verify = |paths: &[String]| {
                    let start = Instant::now();
                    let mut timings = ReconcileTimings::default();
                    let verified =
                        self.verify_files(paths, &state, full, hints, &batch, &mut timings);
                    timings.worker_content_us = crate::managed::work::micros(start.elapsed());
                    let chunk = match verified {
                        Ok(result) => Some(result),
                        Err(error) => {
                            let retryable = is_private_pressure(&error);
                            let mut failure = failure.lock().expect("verification failure lock");
                            record_failure(&mut failure, error);
                            if !retryable && let Some(permit) = permit {
                                permit.cancel();
                            }
                            None
                        }
                    };
                    (chunk, timings)
                };
                let chunks: Vec<_> = match &admission {
                    Some(admission) => files
                        .par_chunks(files.len().div_ceil(admission.workers))
                        .map(verify)
                        .collect(),
                    None => vec![verify(files)],
                };
                for (_, timings) in &chunks {
                    prepared.stats.timings.add_worker(timings);
                }
                if let Some(error) = failure
                    .into_inner()
                    .map_err(|_| WorktreeError::Synchronization)?
                {
                    // Join and drop all outputs and shared parent pins before
                    // retrying; cancellation and non-memory failures stay fatal.
                    drop(chunks);
                    drop(batch);
                    if is_private_pressure(&error)
                        && admission.as_ref().is_some_and(|guard| guard.workers > 1)
                        && let Some(permit) = permit
                    {
                        permit
                            .namespace
                            .fault(
                                crate::managed::faults::Point::ReconcileRetryDrained,
                                Some(permit.operation_id()),
                            )
                            .map_err(crate::Error::from)?;
                        permit.check_now().map_err(crate::Error::from)?;
                        prepared.stats.batch_retries += 1;
                        wanted = 1;
                        continue;
                    }
                    if let Some(permit) = permit {
                        permit.cancel();
                    }
                    return Err(error);
                }
                let (parents, memory) =
                    timed(&mut prepared.stats.timings.parent_verify_us, || {
                        batch.finish(admission.as_ref().map_or(1, |guard| guard.workers))
                    })?;
                prepared.stats.parent_batches += parents.len() as u64;
                prepared.parents.extend(parents);
                if let (Some(target), Some(memory)) = (&mut prepared.memory, memory) {
                    target.absorb(memory).map_err(crate::Error::from)?;
                }
                break chunks;
            };
            prepared.stats.batches += 1;
            for chunk in chunks.into_iter().filter_map(|(chunk, _)| chunk) {
                prepared.stats.add_verification(&chunk.stats);
                for (path, entry) in chunk.evidence {
                    if prepared.evidence.insert(path, entry).is_some() {
                        return Err(WorktreeError::InvalidInput(
                            "duplicate checkout path".into(),
                        ));
                    }
                }
                for (path, file_id) in chunk.copies {
                    copies.entry(file_id).or_default().push(path.clone());
                    prepared.upserts.insert(path, PreparedUpsert::new(permit)?);
                }
                prepared.upserts.extend(chunk.upserts);
            }
        }
        prepared.stats.timings.content_verification_us =
            crate::managed::work::micros(verification.elapsed());
        let assembly = Instant::now();
        // One streaming pass for all verified renames/copies, not one full
        // posting scan per file. No read/extraction of another checkout.
        if !copies.is_empty() {
            let reader = self.generation.base().reader();
            for index in 0..reader.num_trigrams() {
                if let Some(permit) = permit {
                    permit.check().map_err(crate::Error::from)?;
                }
                let (trigram, postings) = reader.trigram_posting_at(index);
                for posting in postings {
                    if let Some(paths) = copies.get(&posting.file_id) {
                        for path in paths {
                            let upsert = prepared.upserts.get_mut(path).expect("copy destination");
                            if let Some(memory) = &mut upsert.memory {
                                crate::managed::memory::reserve_map(&mut upsert.masks, 1, memory)
                                    .map_err(crate::Error::from)?;
                            }
                            upsert.masks.insert(
                                trigram,
                                crate::trigram::TrigramMasks {
                                    loc_mask: posting.loc_mask,
                                    next_mask: posting.next_mask,
                                },
                            );
                            prepared.stats.postings_copied += 1;
                        }
                    }
                }
            }
        }
        prepared.stats.timings.assembly_us += crate::managed::work::micros(assembly.elapsed());
        Ok(prepared)
    }

    fn verify_files(
        &self,
        paths: &[String],
        state: &State,
        full: bool,
        hints: &BTreeSet<String>,
        batch: &VerificationBatch<'_, '_>,
        timings: &mut ReconcileTimings,
    ) -> Result<VerifiedChunk> {
        let permit = batch.permit;
        let verify = batch.verify;
        let mut chunk = VerifiedChunk::default();
        let mut start_path = 0;
        while start_path < paths.len() {
            let parent = parent_path(&paths[start_path]);
            let end_path = start_path
                + paths[start_path..]
                    .iter()
                    .take_while(|path| parent_path(path) == parent)
                    .count();
            if let Some(permit) = permit {
                permit.check().map_err(crate::Error::from)?;
            }
            let reader = timed(&mut timings.worker_parent_us, || batch.reader(parent))?;
            for path in &paths[start_path..end_path] {
                if let Some(permit) = permit {
                    permit.check().map_err(crate::Error::from)?;
                }
                let relative = Path::new(path);
                relative_path(relative)?;
                let file = timed(&mut timings.worker_open_us, || {
                    reader.open_observed(relative)
                })?;
                let version = file_version(&file.metadata);
                let previous = state.evidence.get(path);
                let hinted = !full && is_hinted(path, hints, &mut chunk.stats.hint_lookups);
                let mut updated = false;
                let evidence = if !full
                    && !hinted
                    && version.is_trusted()
                    && previous.is_some_and(|entry| entry.version.as_ref() == Some(&version))
                {
                    chunk.stats.content_reads_avoided += 1;
                    previous.expect("verified previous entry").clone()
                } else {
                    let read = timed(&mut timings.worker_read_verify_us, || {
                        read_file(
                            || reader.open_observed(relative),
                            relative,
                            file,
                            self.options.walk.max_file_size,
                            permit,
                        )
                    })?;
                    chunk.stats.files_read += 1;
                    chunk.stats.bytes_read += read.bytes.len() as u64;
                    let listed = self
                        .options
                        .walk
                        .max_file_size
                        .is_none_or(|limit| read.bytes.len() as u64 <= limit);
                    let eligible = !self
                        .generation
                        .key()
                        .profile()
                        .eligibility
                        .excludes_by_extension(relative);
                    let content = if listed && eligible {
                        chunk.stats.files_decoded += 1;
                        let (text, id) =
                            timed(&mut timings.worker_decode_hash_us, || -> Result<_> {
                                let text = crate::encoding::decode_for_index_controlled(
                                    &read.bytes,
                                    permit,
                                )
                                .map_err(crate::Error::from)?;
                                let id = if crate::trigram::is_binary(&text) {
                                    None
                                } else {
                                    Some(
                                        ContentId::from_indexed_bytes_controlled(&text, permit)
                                            .map_err(crate::Error::from)?,
                                    )
                                };
                                Ok((text, id))
                            })?;
                        if let Some(id) = id {
                            timed(&mut timings.worker_compare_extract_us, || -> Result<()> {
                                let differs_from_published = self.base_content(path) != Some(id)
                                    && !(previous
                                        .and_then(|entry| entry.content)
                                        .or_else(|| state.restored_content.get(path).copied())
                                        == Some(id)
                                        && state.index.live.has_path(path));
                                if differs_from_published {
                                    if verify {
                                        self.stale.store(true, Ordering::Release);
                                        if let Some(permit) = permit {
                                            let point =
                                                crate::managed::faults::Point::VerificationContentHashed;
                                            permit
                                                .namespace
                                                .fault(point, Some(permit.operation_id()))
                                                .map_err(crate::Error::from)?;
                                        }
                                    }
                                    if let Some(file_id) =
                                        batch.base_contents.find(self, id, permit)?
                                    {
                                        chunk.copies.push((path.clone(), file_id));
                                        chunk.stats.base_files_copied += 1;
                                    } else {
                                        let mut upsert = PreparedUpsert::new(permit)?;
                                        upsert.masks = match (permit, &mut upsert.memory) {
                                            (Some(permit), Some(memory)) => {
                                                crate::trigram::extract_merged_masks_controlled(
                                                    &text, permit, memory,
                                                )
                                                .map_err(crate::Error::from)?
                                            }
                                            _ => LiveIndex::compute_trigram_masks(&text),
                                        };
                                        chunk.upserts.push((path.clone(), upsert));
                                        chunk.stats.files_extracted += 1;
                                    }
                                    updated = true;
                                }
                                Ok(())
                            })?;
                        }
                        id
                    } else {
                        None
                    };
                    Evidence {
                        version: Some(read.version),
                        listed,
                        content,
                    }
                };
                if verify
                    && (updated
                        || previous.is_none_or(|entry| {
                            entry.listed != evidence.listed || entry.content != evidence.content
                        }))
                {
                    self.stale.store(true, Ordering::Release);
                }
                if let Some(id) = evidence.content {
                    if self.base_content(path) == Some(id) {
                        chunk.stats.base_reused += 1;
                    } else if !updated {
                        chunk.stats.overlay_reused += 1;
                    }
                }
                chunk.evidence.push((path.clone(), evidence));
            }
            start_path = end_path;
        }
        Ok(chunk)
    }

    /// Hold the readiness, overlay, visibility and membership guards together.
    /// Owned paths and read-only file handles may escape; live IDs never do.
    /// Do not reenter this view (including invalidation) from the closure.
    /// Final matching must use `snapshot.open_file()` or a private versioned
    /// cache. Root verification brackets the callback, including empty results;
    /// root or candidate-open failure closes readiness and requires full
    /// reconciliation, even if the callback handles the candidate's error.
    ///
    /// With [`WorktreeOptions::stale_reads`], the last published state is
    /// served even while invalidations are pending, the readiness guard is
    /// released before the callback so invalidations never wait for a query,
    /// and a candidate-open failure only queues that path for refresh.
    pub fn with_snapshot<T>(&self, read: impl FnOnce(WorktreeSnapshot<'_>) -> T) -> Result<T> {
        if self.options.stale_reads {
            return self.with_published_snapshot(read);
        }
        let mut control = self
            .control
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        if !self.serving(&control) {
            return Err(WorktreeError::NotReady);
        }
        self.verify_snapshot_root(&mut control)?;
        let state = self
            .state
            .read()
            .map_err(|_| WorktreeError::Synchronization)?;
        let open_error = Mutex::new(None);
        let result = read(WorktreeSnapshot {
            root: &self.root,
            rooted: &self.files,
            generation: &self.generation,
            protection: &self.root_protection,
            state: &state,
            epoch: control.epoch,
            open_error: &open_error,
            failed_paths: None,
        });
        self.verify_snapshot_root(&mut control)?;
        let failure = match open_error.into_inner() {
            Ok(error) => error.map(WorktreeError::Io),
            Err(_) => Some(WorktreeError::Synchronization),
        };
        if let Some(error) = failure {
            control.full = true;
            control.hints.clear();
            control.invalidate()?;
            return Err(error);
        }
        Ok(result)
    }

    fn with_published_snapshot<T>(
        &self,
        read: impl FnOnce(WorktreeSnapshot<'_>) -> T,
    ) -> Result<T> {
        // Lock order is control then state; publication holds control while
        // it waits for readers, so control is never reacquired under state.
        let (state, epoch) = {
            let mut control = self
                .control
                .lock()
                .map_err(|_| WorktreeError::Synchronization)?;
            let epoch = control.published_epoch.ok_or(WorktreeError::NotReady)?;
            self.verify_snapshot_root(&mut control)?;
            let state = self
                .state
                .read()
                .map_err(|_| WorktreeError::Synchronization)?;
            (state, epoch)
        };
        let open_error = Mutex::new(None);
        let failed_paths = Mutex::new(BTreeSet::new());
        let result = read(WorktreeSnapshot {
            root: &self.root,
            rooted: &self.files,
            generation: &self.generation,
            protection: &self.root_protection,
            state: &state,
            epoch,
            open_error: &open_error,
            failed_paths: Some(&failed_paths),
        });
        drop(state);
        if let Err(error) = self.files.verify_root() {
            self.invalidate_all()?;
            return Err(error.into());
        }
        let failed_paths = failed_paths
            .into_inner()
            .map_err(|_| WorktreeError::Synchronization)?;
        for path in failed_paths {
            self.invalidate_path(Path::new(&path))?;
        }
        Ok(result)
    }

    fn verify_snapshot_root(&self, control: &mut Control) -> Result<()> {
        if let Err(error) = self.files.verify_root() {
            control.full = true;
            control.hints.clear();
            control.invalidate()?;
            return Err(error.into());
        }
        Ok(())
    }

    /// Save only the private delta, binding it to the exact generation key.
    /// A dedicated checkpoint directory must have been configured at creation.
    /// Saving requires readiness; no base or ordinary HybridIndex flush occurs.
    pub fn save_checkpoint(&self) -> Result<()> {
        let path = self.checkpoint_path()?;
        validate_checkpoint_directory(
            path.parent().expect("checkpoint parent"),
            &self.root,
            &self.repository,
            &self.generation,
        )?;
        let control = self
            .control
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        if !self.current(&control) {
            return Err(WorktreeError::NotReady);
        }
        let state = self
            .state
            .read()
            .map_err(|_| WorktreeError::Synchronization)?;
        self.generation.base().save_overlay_with_generation(
            &state.index,
            &path,
            Some(self.generation.key()),
        )?;
        Ok(())
    }

    /// Restore a delta against the supplied exact pin. Missing/stale/invalid
    /// checkpoints are errors. A successful restore is still NOT READY and
    /// must receive a full refresh after the caller subscribes to changes.
    pub fn restore(
        root: &Path,
        generation: Arc<Generation>,
        options: WorktreeOptions,
    ) -> Result<Self> {
        let view = Self::new(root, generation, options)?;
        let index = view.generation.base().restore_worktree_with_generation(
            &view.root,
            &view.checkpoint_path()?,
            Some(view.generation.key()),
        )?;
        {
            let mut state = view
                .state
                .write()
                .map_err(|_| WorktreeError::Synchronization)?;
            state.index = index;
            state.cost = measure_overlay_cost(&state, None).map_err(crate::Error::from)?;
        }
        Ok(view)
    }

    fn checkpoint_path(&self) -> Result<PathBuf> {
        self.options
            .checkpoint_directory
            .as_ref()
            .map(|directory| directory.join("overlay.json"))
            .ok_or_else(|| {
                WorktreeError::InvalidInput("no private checkpoint directory configured".into())
            })
    }

    pub(crate) fn capture_checkpoint(
        &self,
        permit: &Arc<crate::managed::WorkPermit>,
    ) -> crate::managed::Result<CapturedCheckpoint> {
        let control = self
            .control
            .lock()
            .map_err(|_| crate::managed::Error::corrupt("view lock poisoned"))?;
        if !self.serving(&control) {
            return Err(crate::managed::Error::busy("view-not-ready"));
        }
        self.files.verify_root()?;
        let state = self
            .state
            .read()
            .map_err(|_| crate::managed::Error::corrupt("view state poisoned"))?;
        let epoch = control
            .published_epoch
            .ok_or_else(|| crate::managed::Error::busy("view-not-ready"))?;
        // The state guard alone pins the published state; eventual views
        // must not block invalidations for the duration of the capture.
        let _control = if self.options.stale_reads {
            drop(control);
            None
        } else {
            Some(control)
        };
        let cost = &state.cost;
        let amount = cost
            .bytes
            .checked_mul(16)
            .and_then(|bytes| {
                bytes.checked_add(cost.files.checked_add(cost.tombstones)?.checked_mul(256)?)
            })
            .ok_or_else(|| crate::managed::Error::pressure("checkpoint-memory-overflow"))?;
        let charge = permit.memory(amount)?;
        let checkpoint = self.generation.base().capture_checkpoint_controlled(
            &state.index,
            Some(self.generation.key()),
            Some(permit),
        )?;
        let mut contents = Vec::new();
        for path in state.index.live.active_paths() {
            permit.check()?;
            let content = state
                .evidence
                .get(path)
                .and_then(|entry| entry.content)
                .ok_or_else(|| {
                    crate::managed::Error::corrupt("ready overlay has no verified content identity")
                })?;
            contents.push((path.to_owned(), content));
        }
        self.files.verify_root()?;
        Ok(CapturedCheckpoint {
            checkpoint,
            contents,
            epoch,
            _charge: charge,
        })
    }

    pub(crate) fn restore_checkpoint_value(
        &self,
        checkpoint: crate::shared::OverlayCheckpoint,
        contents: Vec<(String, ContentId)>,
        permit: &Arc<crate::managed::WorkPermit>,
        mut memory: crate::managed::work::MemoryCharge,
    ) -> Result<()> {
        let _serial = self
            .reconcile
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        self.invalidate_all()?;
        let index = self.generation.base().restore_checkpoint_controlled(
            &self.root,
            checkpoint,
            Some(self.generation.key()),
            Some(permit),
        )?;
        let mut restored_content = BTreeMap::new();
        for (path, content) in contents {
            permit.check().map_err(crate::Error::from)?;
            if !index.live.has_path(&path) || restored_content.insert(path, content).is_some() {
                return Err(WorktreeError::InvalidInput(
                    "checkpoint content evidence does not match the private overlay".into(),
                ));
            }
        }
        let bytes = retained_view_bytes(
            &index.live,
            restored_content.keys(),
            &PathVisibility::default(),
            permit,
        )
        .map_err(crate::Error::from)?;
        memory.resize(bytes).map_err(crate::Error::from)?;
        let retained = memory.retain(0).map_err(crate::Error::from)?;
        let mut state = self
            .state
            .write()
            .map_err(|_| WorktreeError::Synchronization)?;
        state.index = index;
        state.restored_content = restored_content;
        state.memory = Some(retained);
        state.cost = measure_overlay_cost(&state, Some(permit)).map_err(crate::Error::from)?;
        Ok(())
    }

    pub fn overlay_cost(&self) -> crate::managed::Result<OverlayCost> {
        let state = self
            .state
            .read()
            .map_err(|_| crate::managed::Error::corrupt("view state poisoned"))?;
        Ok(state.cost.clone())
    }

    pub(crate) fn private_evidence(
        &self,
        max_paths: u32,
        permit: &Arc<crate::managed::WorkPermit>,
    ) -> crate::managed::Result<Vec<PrivateEvidence>> {
        let control = self
            .control
            .lock()
            .map_err(|_| crate::managed::Error::corrupt("view lock poisoned"))?;
        if !self.current(&control) {
            return Err(crate::managed::Error::busy("view-not-ready"));
        }
        let state = self
            .state
            .read()
            .map_err(|_| crate::managed::Error::corrupt("view state poisoned"))?;
        let mut paths: BTreeMap<String, PrivateEvidence> = BTreeMap::new();
        for path in state.index.live.private_paths() {
            permit.check()?;
            if paths.len() >= max_paths as usize {
                return Err(crate::managed::Error::pressure(
                    "adaptive-private-path-limit",
                ));
            }
            let content = state.evidence.get(path).and_then(|entry| entry.content);
            paths.insert(
                path.to_owned(),
                PrivateEvidence {
                    bytes: path.len() as u64,
                    path: path.to_owned(),
                    content,
                },
            );
        }
        for ids in state.index.live.inverted_index().values() {
            permit.check()?;
            for &id in ids {
                permit.check()?;
                if let Some(path) = state.index.live.file_path(id) {
                    let evidence = paths.get_mut(path).ok_or_else(|| {
                        crate::managed::Error::corrupt("overlay evidence mismatch")
                    })?;
                    evidence.bytes += crate::ondisk::POSTING_ENTRY_SIZE as u64;
                }
            }
        }
        Ok(paths.into_values().collect())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OverlayCost {
    pub files: u64,
    pub tombstones: u64,
    pub postings: u64,
    /// Exact path and posting payload bytes, not an RSS estimate.
    pub bytes: u64,
}

#[derive(Serialize)]
pub(crate) struct PrivateEvidence {
    pub(crate) path: String,
    pub(crate) content: Option<ContentId>,
    pub(crate) bytes: u64,
}

pub(crate) struct CapturedCheckpoint {
    pub(crate) checkpoint: crate::shared::OverlayCheckpoint,
    pub(crate) contents: Vec<(String, ContentId)>,
    pub(crate) epoch: u64,
    _charge: crate::managed::work::MemoryCharge,
}

fn measure_overlay_cost(
    state: &State,
    permit: Option<&Arc<crate::managed::WorkPermit>>,
) -> crate::managed::Result<OverlayCost> {
    let mut files = 0_u64;
    let mut tombstones = 0_u64;
    let mut path_bytes = 0_u64;
    for (path, deleted) in state
        .index
        .live
        .active_paths()
        .map(|path| (path, false))
        .chain(state.index.live.deleted_paths().map(|path| (path, true)))
    {
        if let Some(permit) = permit {
            permit.check()?;
        }
        path_bytes = path_bytes
            .checked_add(path.len() as u64)
            .ok_or_else(|| crate::managed::Error::corrupt("overlay path accounting overflow"))?;
        if deleted {
            tombstones += 1;
        } else {
            files += 1;
        }
    }
    let mut postings = 0_u64;
    for (position, ids) in state.index.live.inverted_index().values().enumerate() {
        if position % 512 == 0
            && let Some(permit) = permit
        {
            permit.check()?;
        }
        postings = postings
            .checked_add(ids.len() as u64)
            .ok_or_else(|| crate::managed::Error::corrupt("overlay posting accounting overflow"))?;
    }
    let bytes = postings
        .checked_mul(crate::ondisk::POSTING_ENTRY_SIZE as u64)
        .and_then(|bytes| bytes.checked_add(path_bytes))
        .ok_or_else(|| crate::managed::Error::corrupt("overlay accounting overflow"))?;
    Ok(OverlayCost {
        files,
        tombstones,
        postings,
        bytes,
    })
}

fn retained_view_bytes<'a>(
    live: &LiveIndex,
    paths: impl Iterator<Item = &'a String>,
    visibility: &PathVisibility,
    permit: &Arc<crate::managed::WorkPermit>,
) -> crate::managed::Result<u64> {
    let mut bytes = live
        .private_memory_estimate(permit)?
        .checked_add(visibility.private_memory_estimate(permit)?)
        .and_then(|bytes| bytes.checked_add(64 * 1024))
        .ok_or_else(|| crate::managed::Error::pressure("view-memory-account-overflow"))?;
    for path in paths {
        permit.check()?;
        bytes = (path.capacity() as u64)
            .checked_mul(2)
            .and_then(|path| path.checked_add(512))
            .and_then(|path| bytes.checked_add(path))
            .ok_or_else(|| crate::managed::Error::pressure("view-memory-account-overflow"))?;
    }
    Ok(bytes)
}

/// An escaped managed candidate retains both the base and the physical root.
/// There is deliberately no conversion or Deref to an unguarded File.
pub struct CandidateFile {
    file: File,
    _root: Arc<RootedDir>,
    _generation: Arc<Generation>,
    _protection: Option<Arc<crate::managed::roots::RootProtection>>,
}

/// A bounded sibling-read group retaining the same root and generation pins
/// as escaped candidates. Pin it through a snapshot, open siblings with
/// [`CandidateBatch::open`], and finish before publishing.
pub struct CandidateBatch {
    batch: RootedBatch<Arc<RootedDir>>,
    generation: Arc<Generation>,
    _protection: Option<Arc<crate::managed::roots::RootProtection>>,
}

impl CandidateBatch {
    /// Open a sibling through the group's pinned handles without a snapshot.
    /// The pinning snapshot verified the root and ancestors, and `finish`
    /// verifies them again: discard the group's reads if it fails. A failed
    /// open is not recorded on the view, so check the query, then requeue the
    /// path or invalidate the view.
    pub fn open(&self, relative: &str) -> std::io::Result<CandidateFile> {
        let opened = self.batch.open_observed(Path::new(relative))?;
        Ok(CandidateFile {
            file: opened.file,
            _root: Arc::clone(self.batch.pinned_root()),
            _generation: Arc::clone(&self.generation),
            _protection: self._protection.clone(),
        })
    }

    pub fn finish(self) -> std::io::Result<()> {
        self.batch.finish().map(|_| ())
    }
}

impl CandidateFile {
    pub fn metadata(&self) -> std::io::Result<fs::Metadata> {
        self.file.metadata()
    }
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            _root: Arc::clone(&self._root),
            _generation: Arc::clone(&self._generation),
            _protection: self._protection.clone(),
        })
    }
}

impl Read for CandidateFile {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(bytes)
    }
}

impl std::io::Seek for CandidateFile {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        std::io::Seek::seek(&mut self.file, position)
    }
}

/// Borrowed, query-only atomic view. Does not expose the mutable HybridIndex.
pub struct WorktreeSnapshot<'a> {
    root: &'a Path,
    rooted: &'a Arc<RootedDir>,
    generation: &'a Arc<Generation>,
    protection: &'a Option<Arc<crate::managed::roots::RootProtection>>,
    state: &'a State,
    epoch: u64,
    open_error: &'a Mutex<Option<std::io::Error>>,
    /// Stale-read snapshots queue failed candidates as path hints instead.
    failed_paths: Option<&'a Mutex<BTreeSet<String>>>,
}

impl WorktreeSnapshot<'_> {
    pub fn root(&self) -> &Path {
        self.root
    }

    /// Whether this snapshot may lag pending invalidations; see
    /// [`WorktreeOptions::stale_reads`].
    pub fn eventual(&self) -> bool {
        self.failed_paths.is_some()
    }

    /// Open a root-relative candidate through the view's retained root handle.
    /// The returned read-only regular-file handle may outlive this guard, but
    /// does not freeze file contents. Bound reads and check the final snapshot
    /// epoch before publishing a result assembled outside the guard.
    /// An open failure also makes `with_snapshot` fail and invalidate readiness
    /// (stale-read views instead queue the path for refresh).
    /// Report later handle-read failures through the view after leaving the guard.
    pub fn open_file(&self, relative: &str) -> std::io::Result<File> {
        if self.generation.base().reader().managed_identity().is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "managed snapshots require open_candidate",
            ));
        }
        self.open_inner(relative)
    }

    pub fn open_candidate(&self, relative: &str) -> std::io::Result<CandidateFile> {
        Ok(CandidateFile {
            file: self.open_inner(relative)?,
            _root: Arc::clone(self.rooted),
            _generation: Arc::clone(self.generation),
            _protection: self.protection.clone(),
        })
    }

    /// Pin the ancestors of one candidate for a bounded sibling group.
    pub fn candidate_batch(&self, relative: &str) -> std::io::Result<CandidateBatch> {
        crate::rooted::validate_index_path(relative)?;
        let parent = Path::new(relative)
            .parent()
            .expect("validated relative path");
        let batch = self.record_open(relative, self.rooted.owned_batch(parent))?;
        Ok(CandidateBatch {
            batch,
            generation: Arc::clone(self.generation),
            _protection: self.protection.clone(),
        })
    }

    pub fn open_candidate_in(
        &self,
        batch: &CandidateBatch,
        relative: &str,
    ) -> std::io::Result<CandidateFile> {
        if !std::ptr::eq(batch.batch.root(), self.rooted.as_ref())
            || !Arc::ptr_eq(&batch.generation, self.generation)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "candidate batch belongs to a different snapshot",
            ));
        }
        let opened = self.record_open(relative, batch.batch.open_observed(Path::new(relative)))?;
        Ok(CandidateFile {
            file: opened.file,
            _root: Arc::clone(self.rooted),
            _generation: Arc::clone(self.generation),
            _protection: self.protection.clone(),
        })
    }

    fn open_inner(&self, relative: &str) -> std::io::Result<File> {
        self.record_open(relative, self.rooted.open_file(Path::new(relative)))
    }

    fn record_open<T>(&self, relative: &str, result: std::io::Result<T>) -> std::io::Result<T> {
        if let Err(error) = &result {
            if let Some(failed) = self.failed_paths {
                failed
                    .lock()
                    .map_err(|_| std::io::Error::other("snapshot error lock is poisoned"))?
                    .insert(relative.to_owned());
                return result;
            }
            let mut first_error = self
                .open_error
                .lock()
                .map_err(|_| std::io::Error::other("snapshot error lock is poisoned"))?;
            if first_error.is_none() {
                *first_error = Some(std::io::Error::new(
                    error.kind(),
                    format!("cannot open worktree candidate {relative:?}: {error}"),
                ));
            }
        }
        result
    }

    /// Strict views: the input epoch the snapshot was taken at. Stale-read
    /// views: the epoch of the published state being served.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn visibility(&self) -> &PathVisibility {
        &self.state.visibility
    }

    /// Root-relative candidates, resolved while the live-overlay guard is held.
    /// `prefix` is empty or a root-relative directory with a trailing slash;
    /// postings outside it are skipped before the plan is evaluated.
    pub fn candidates(&self, plan: &QueryPlan, prefix: &str, include_hidden: bool) -> Vec<String> {
        let (ids, reader) = self.state.index.execute_query_with_masks_in(plan, prefix);
        let mut paths: Vec<_> = ids
            .into_iter()
            .filter_map(|id| self.state.index.resolve_path(id, &reader))
            .filter(|path| {
                self.state
                    .visibility
                    .is_visible(path, prefix, include_hidden)
            })
            .collect();
        paths.sort_unstable();
        paths
    }

    /// Complete filename membership, including extension-filtered/NUL files.
    pub fn files(&self, prefix: &str, include_hidden: bool) -> Vec<String> {
        self.listed_from(prefix, include_hidden, "")
            .map(str::to_owned)
            .collect()
    }

    /// The [`files`](Self::files) entries that sort at or after `from`, in
    /// path order. Only the paths under `prefix` are visited.
    pub fn listed_from(
        &self,
        prefix: &str,
        include_hidden: bool,
        from: &str,
    ) -> impl Iterator<Item = &str> {
        let start = std::cmp::max(prefix, from);
        self.state
            .listed
            .range::<str, _>((std::ops::Bound::Included(start), std::ops::Bound::Unbounded))
            .map(String::as_str)
            .take_while(move |path| path.starts_with(prefix))
            .filter(move |path| {
                self.state
                    .visibility
                    .is_visible(path, prefix, include_hidden)
            })
    }

    /// Number of listed paths, hidden ones included.
    pub fn file_count(&self) -> usize {
        self.state.listed.len()
    }
}

struct Prepared {
    memory: Option<crate::managed::work::MemoryCharge>,
    parents: Vec<ParentObservation>,
    evidence: BTreeMap<String, Evidence>,
    visibility: PathVisibility,
    upserts: BTreeMap<String, PreparedUpsert>,
    stats: ReconcileStats,
}

#[derive(Default)]
struct VerifiedChunk {
    evidence: Vec<(String, Evidence)>,
    upserts: Vec<(String, PreparedUpsert)>,
    copies: Vec<(String, u32)>,
    stats: ReconcileStats,
}

struct PreparedUpsert {
    masks: TrigramMaskMap,
    memory: Option<crate::managed::work::MemoryCharge>,
}

impl PreparedUpsert {
    fn new(permit: Option<&Arc<crate::managed::WorkPermit>>) -> Result<Self> {
        Ok(Self {
            masks: TrigramMaskMap::default(),
            memory: permit
                .map(|permit| permit.memory(0))
                .transpose()
                .map_err(crate::Error::from)?,
        })
    }
}

struct ParentBatch<'a> {
    reader: OnceLock<RootedBatch<&'a RootedDir>>,
    initialize: Mutex<()>,
}

struct VerificationBatch<'root, 'inputs> {
    root: &'root RootedDir,
    parents: HashMap<&'inputs str, ParentBatch<'root>>,
    base_contents: &'inputs BaseContents,
    permit: Option<&'inputs Arc<crate::managed::WorkPermit>>,
    verify: bool,
    memory: Option<crate::managed::work::MemoryCharge>,
    _pins: Option<crate::managed::work::ParentAdmission<'inputs>>,
}

impl<'root, 'inputs> VerificationBatch<'root, 'inputs> {
    fn new(
        root: &'root RootedDir,
        paths: &'inputs [String],
        base_contents: &'inputs BaseContents,
        permit: Option<&'inputs Arc<crate::managed::WorkPermit>>,
    ) -> Result<Self> {
        let mut memory = permit
            .map(|permit| permit.memory(0))
            .transpose()
            .map_err(crate::Error::from)?;
        let mut parents = HashMap::new();
        let mut handles = 0;
        for path in paths {
            let parent = parent_path(path);
            if let std::collections::hash_map::Entry::Vacant(entry) = parents.entry(parent) {
                handles += RootedDir::batch_handles(Path::new(parent));
                if let Some(memory) = &mut memory {
                    memory
                        .grow(
                            root.batch_memory_bytes(Path::new(parent))
                                + std::mem::size_of::<ParentBatch<'_>>() as u64 * 2,
                        )
                        .map_err(crate::Error::from)?;
                }
                entry.insert(ParentBatch {
                    reader: OnceLock::new(),
                    initialize: Mutex::new(()),
                });
            }
        }
        let pins = permit
            .map(|permit| permit.admit_parents(handles))
            .transpose()
            .map_err(crate::Error::from)?;
        Ok(Self {
            root,
            parents,
            base_contents,
            permit,
            verify: false,
            memory,
            _pins: pins,
        })
    }

    fn reader(&self, parent: &str) -> Result<&RootedBatch<&'root RootedDir>> {
        let entry = self.parents.get(parent).expect("planned parent batch");
        if entry.reader.get().is_none() {
            let _initialize = entry
                .initialize
                .lock()
                .map_err(|_| WorktreeError::Synchronization)?;
            if entry.reader.get().is_none() {
                if let Some(permit) = self.permit {
                    permit.check().map_err(crate::Error::from)?;
                }
                let reader = self.root.batch(Path::new(parent))?;
                if entry.reader.set(reader).is_err() {
                    return Err(WorktreeError::Synchronization);
                }
                if let Some(permit) = self.permit {
                    permit
                        .namespace
                        .fault(
                            crate::managed::faults::Point::ReconcileBatchOpened,
                            Some(permit.operation_id()),
                        )
                        .map_err(crate::Error::from)?;
                }
            }
        }
        Ok(entry.reader.get().expect("initialized parent batch"))
    }

    fn finish(
        mut self,
        workers: usize,
    ) -> Result<(
        Vec<ParentObservation>,
        Option<crate::managed::work::MemoryCharge>,
    )> {
        let mut groups: Vec<_> = self.parents.into_values().collect();
        let verify = |groups: &mut [ParentBatch<'_>]| -> Result<Vec<ParentObservation>> {
            let mut parents = Vec::with_capacity(groups.len());
            for group in groups {
                if let Some(permit) = self.permit {
                    permit.check().map_err(crate::Error::from)?;
                }
                parents.push(
                    group
                        .reader
                        .take()
                        .ok_or(WorktreeError::Synchronization)?
                        .finish()?,
                );
            }
            Ok(parents)
        };
        let mut parents = Vec::with_capacity(groups.len());
        let checks: Vec<_> = if workers > 1 {
            let chunk_size = groups.len().div_ceil(workers).max(1);
            groups.par_chunks_mut(chunk_size).map(verify).collect()
        } else {
            vec![verify(&mut groups)]
        };
        drop(groups);
        let mut failure = None;
        for check in checks {
            match check {
                Ok(verified) => parents.extend(verified),
                Err(error) => record_failure(&mut failure, error),
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        let mut bytes = parents.capacity() as u64 * std::mem::size_of::<ParentObservation>() as u64;
        for observation in &parents {
            bytes += observation.memory_bytes();
        }
        if let Some(memory) = &mut self.memory {
            memory.resize(bytes).map_err(crate::Error::from)?;
        }
        Ok((parents, self.memory))
    }
}

#[derive(Default)]
struct BaseContents {
    entries: OnceLock<BaseContentMap>,
    initialize: Mutex<()>,
}

struct BaseContentMap {
    entries: HashMap<ContentId, u32>,
    _memory: Option<crate::managed::work::MemoryCharge>,
}

impl BaseContents {
    fn find(
        &self,
        view: &WorktreeView,
        id: ContentId,
        permit: Option<&Arc<crate::managed::WorkPermit>>,
    ) -> Result<Option<u32>> {
        if self.entries.get().is_none() {
            let _initialize = self
                .initialize
                .lock()
                .map_err(|_| WorktreeError::Synchronization)?;
            if self.entries.get().is_none() {
                let paths = view.generation.base().reader().all_paths();
                let memory = permit
                    .map(|permit| permit.memory(paths.len() as u64 * 128))
                    .transpose()
                    .map_err(crate::Error::from)?;
                let mut entries = HashMap::new();
                for (file_id, path) in paths.iter().enumerate() {
                    if let Some(permit) = permit {
                        permit.check().map_err(crate::Error::from)?;
                    }
                    if let Some(content) = view.base_content(path) {
                        entries.insert(content, file_id as u32);
                    }
                }
                if self
                    .entries
                    .set(BaseContentMap {
                        entries,
                        _memory: memory,
                    })
                    .is_err()
                {
                    return Err(WorktreeError::Synchronization);
                }
            }
        }
        Ok(self
            .entries
            .get()
            .expect("initialized base contents")
            .entries
            .get(&id)
            .copied())
    }
}

fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

pub(crate) fn relative_path(path: &Path) -> Result<String> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(WorktreeError::InvalidInput(
            "hint must be nonempty and root-relative, without '..' or a leading '.' component"
                .into(),
        ));
    }
    let path: PathBuf = path.components().collect();
    let path = path
        .to_str()
        .ok_or_else(|| WorktreeError::InvalidInput("non-Unicode checkout path".into()))?;
    #[cfg(unix)]
    if path.contains('\\') {
        return Err(WorktreeError::InvalidInput(
            "backslash checkout path is not representable".into(),
        ));
    }
    Ok(path.replace('\\', "/"))
}

fn absolute_exclusion(root: &Path, path: &Path) -> Result<PathBuf> {
    let path = root.join(path);
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(WorktreeError::InvalidInput(
            "exclusion contains a parent component".into(),
        ));
    }
    match fs::canonicalize(&path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| WorktreeError::InvalidInput("invalid exclusion path".into()))?;
            Ok(absolute_exclusion(root, parent)?
                .join(path.file_name().expect("exclusion filename")))
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_checkpoint_directory(
    directory: &Path,
    root: &Path,
    repository: &Repository,
    generation: &Generation,
) -> Result<()> {
    let directory = fs::canonicalize(directory)?;
    if !fs::metadata(&directory)?.is_dir() {
        return Err(WorktreeError::InvalidInput(
            "checkpoint_directory must be an existing directory".into(),
        ));
    }
    if root.starts_with(&directory)
        || directory.starts_with(repository.common_dir())
        || directory.starts_with(repository.git_dir())
        || directory.starts_with(generation.directory().parent().expect("generation store"))
    {
        return Err(WorktreeError::InvalidInput("checkpoint must use a private directory outside Git/base storage and worktree ancestors".into()));
    }
    for ancestor in directory.ancestors() {
        if ancestor.join("generation.json").try_exists()?
            || (ancestor.join("files.bin").try_exists()?
                && ancestor.join("index.bin").try_exists()?)
        {
            return Err(WorktreeError::InvalidInput(
                "checkpoint directory is inside an index snapshot".into(),
            ));
        }
    }
    Ok(())
}

fn is_hinted(path: &str, hints: &BTreeSet<String>, lookups: &mut u64) -> bool {
    if hints.is_empty() {
        return false;
    }
    let path = path.to_ascii_lowercase();
    let mut prefix = path.as_str();
    loop {
        *lookups += 1;
        if hints.contains(prefix) {
            return true;
        }
        let Some((parent, _)) = prefix.rsplit_once('/') else {
            return false;
        };
        prefix = parent;
    }
}

fn queue_hint(hints: &mut BTreeSet<String>, path: String, capacity: usize) -> bool {
    if is_hinted(&path, hints, &mut 0) {
        return true;
    }
    hints.insert(path);
    if hints.len() <= capacity {
        return true;
    }
    let target = (capacity / 2).max(capacity.min(1));
    while hints.len() > target {
        let depth = hints
            .iter()
            .map(|path| path.split('/').count())
            .max()
            .unwrap_or(0);
        if depth <= 1 {
            return false;
        }
        let coarsened: BTreeSet<_> = hints
            .iter()
            .map(|path| {
                if path.split('/').count() == depth {
                    parent_path(path).to_owned()
                } else {
                    path.clone()
                }
            })
            .collect();
        *hints = coarsened
            .iter()
            .filter(|path| !is_hinted(parent_path(path), &coarsened, &mut 0))
            .cloned()
            .collect();
    }
    true
}

struct VerifiedRead<'a> {
    bytes: Vec<u8>,
    version: FileVersion,
    _admission: Option<crate::managed::work::ReadAdmission<'a>>,
}

fn is_private_pressure(error: &WorktreeError) -> bool {
    matches!(error, WorktreeError::Index(crate::Error::Managed(error))
        if error.category == crate::managed::ErrorCategory::ResourcePressure
            && error.reason_code == "reserved-private-memory-exhausted")
}

fn record_failure(failure: &mut Option<WorktreeError>, error: WorktreeError) {
    let substantive = !is_private_pressure(&error) && !is_peer_cancellation(&error);
    if failure.as_ref().is_none_or(|previous| {
        substantive && (is_private_pressure(previous) || is_peer_cancellation(previous))
    }) {
        *failure = Some(error);
    }
}

fn is_peer_cancellation(error: &WorktreeError) -> bool {
    matches!(error, WorktreeError::Index(crate::Error::Managed(error))
        if error.category == crate::managed::ErrorCategory::Cancelled)
}

/// Operation budget exhaustion carries no evidence about checkout content.
fn is_budget_failure(error: &WorktreeError) -> bool {
    matches!(error, WorktreeError::Index(crate::Error::Managed(error))
    if matches!(
        error.category,
        crate::managed::ErrorCategory::Cancelled
            | crate::managed::ErrorCategory::Deadline
            | crate::managed::ErrorCategory::ResourcePressure
    ))
}

fn read_file<'a>(
    reopen: impl FnOnce() -> std::io::Result<OpenedFile>,
    path: &Path,
    opened: OpenedFile,
    limit: Option<u64>,
    permit: Option<&'a Arc<crate::managed::WorkPermit>>,
) -> Result<VerifiedRead<'a>> {
    let OpenedFile {
        mut file,
        metadata: opened,
    } = opened;
    let version = file_version(&opened);
    let change = permit
        .map(|_| crate::managed::storage::file_change(&file))
        .transpose()
        .map_err(crate::Error::from)?;
    let work_limit = permit.map(|permit| permit.read_limits().blob_bytes);
    let read_limit = limit
        .map_or(u64::MAX, |limit| limit.saturating_add(1))
        .min(work_limit.map_or(u64::MAX, |limit| limit.saturating_add(1)));
    let expected = opened.len().min(read_limit);
    let charged = expected
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(64 * 1024))
        .ok_or_else(|| {
            crate::Error::from(crate::managed::Error::pressure(
                "checkout-read-memory-overflow",
            ))
        })?;
    let admission = permit
        .map(|permit| permit.admit_read(charged))
        .transpose()
        .map_err(crate::Error::from)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(expected).map_err(|_| {
            crate::Error::from(crate::managed::Error::pressure(
                "checkout-read-address-space",
            ))
        })?)
        .map_err(|_| {
            crate::Error::from(crate::managed::Error::pressure("checkout-read-allocation"))
        })?;
    let mut input = (&mut file).take(read_limit.min(opened.len().saturating_add(1)));
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        if let Some(permit) = permit {
            permit.check().map_err(crate::Error::from)?;
        }
        let count = input.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        // A growing file is unstable, not a request to wait for additional
        // capacity while retaining an already admitted read buffer.
        if bytes.len() as u64 + count as u64 > expected {
            return Err(WorktreeError::UnstableFile(path.into()));
        }
        if work_limit.is_some_and(|limit| bytes.len() as u64 + count as u64 > limit) {
            return Err(crate::Error::from(crate::managed::Error::pressure(
                "checkout-blob-read-limit",
            ))
            .into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let current = reopen()?;
    let native_changed = match change {
        Some(change) => {
            change != crate::managed::storage::file_change(&file).map_err(crate::Error::from)?
                || change
                    != crate::managed::storage::file_change(&current.file)
                        .map_err(crate::Error::from)?
        }
        None => false,
    };
    if native_changed
        || version != file_version(&file.metadata()?)
        || version != file_version(&current.metadata)
        || same_file::Handle::from_file(file)? != same_file::Handle::from_file(current.file)?
        || (limit.is_none_or(|limit| bytes.len() as u64 <= limit)
            && bytes.len() as u64 != opened.len())
    {
        return Err(WorktreeError::UnstableFile(path.into()));
    }
    Ok(VerifiedRead {
        bytes,
        version,
        _admission: admission,
    })
}

#[cfg(test)]
mod tests;
