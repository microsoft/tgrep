//! Pinned committed-tree bases with synchronized, private checkout overlays.
//!
//! Subscribe to changes after construction and before the first reconciliation.
//! Invalidations immediately close the query gate. A successful refresh covers
//! processed notifications, not an atomic filesystem snapshot: missed events
//! require periodic [`WorktreeView::reconcile_full`] calls. No native watcher,
//! CLI routing, content cache, base migration, or generation GC is provided.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use crate::generations::{Generation, GenerationError, Repository};
use crate::gitignore::CaseInsensitiveIgnore;
use crate::hybrid::HybridIndex;
use crate::live::LiveIndex;
use crate::meta::{ContentId, FileVersion, file_version};
use crate::query::QueryPlan;
use crate::rooted::RootedDir;
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
}

impl Default for WorktreeOptions {
    fn default() -> Self {
        Self {
            walk: MetaWalkOptions::default(),
            hint_capacity: 4096,
            checkpoint_directory: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeStatus {
    pub ready: bool,
    /// Advances for invalidations and at the start of every reconciliation.
    pub epoch: u64,
    pub published_epoch: Option<u64>,
    pub pending_paths: usize,
    pub full_required: bool,
}

/// Work performed by one successful refresh (including no-op verification).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileStats {
    pub epoch: u64,
    pub full: bool,
    pub files_discovered: usize,
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
    /// Ordered-set probes for hinted paths/ancestors, bounded by path depth
    /// rather than the number of pending hints. Full passes make no probes.
    pub hint_lookups: u64,
}

struct Control {
    epoch: u64,
    published_epoch: Option<u64>,
    ready: bool,
    full: bool,
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
    version: FileVersion,
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
        Ok(WorktreeStatus {
            ready: control.ready,
            epoch: control.epoch,
            published_epoch: control.published_epoch,
            pending_paths: control.hints.len(),
            full_required: control.full,
        })
    }

    /// Queue a root-relative file or subtree, including missing/renamed paths.
    /// Pass both old and new rename paths. Invalid hints close the gate and
    /// force a full pass before returning an error; never silently discard one.
    /// Accepted repeated/trailing separators and interior `.` components are
    /// normalized without requiring the hinted path to still exist.
    /// Git/ignore configuration changes should use `invalidate_all`. A
    /// successful refresh acknowledges this token with an equal or later epoch.
    pub fn invalidate_path(&self, path: &Path) -> Result<u64> {
        let mut control = self
            .control
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        let epoch = control.invalidate()?;
        let relative = match relative_path(path) {
            Ok(relative) => relative.to_ascii_lowercase(),
            Err(error) => {
                control.full = true;
                control.hints.clear();
                return Err(error);
            }
        };
        if !relative.is_ascii()
            || relative
                .split('/')
                .any(|part| matches!(part, ".git" | ".gitignore" | ".ignore" | ".gitattributes"))
            || control.hints.len() >= self.options.hint_capacity
                && !control.hints.contains(&relative)
        {
            control.full = true;
            control.hints.clear();
        } else if !control.full {
            control.hints.insert(relative);
        }
        Ok(epoch)
    }

    /// Startup, overflow, missed-event repair, Git/config changes and no-watch
    /// refresh all use this invalidation. Safe to call while a pass is running.
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

    /// Rewalk membership/visibility and process pending hints. With no hints,
    /// performs a full content reconciliation, not a success-shaped no-op.
    ///
    /// At most one pass is attempted. A concurrent invalidation returns
    /// `ChangedDuringReconcile`, retaining a full invalidation for the next
    /// call. Churn never causes unbounded retries or stale ready publication.
    /// Discovery/read failures likewise leave the gate closed and require a
    /// full retry. Matching decoded content never re-extracts existing postings.
    pub fn refresh(&self) -> Result<ReconcileStats> {
        self.refresh_inner(|| {}, || {})
    }

    fn refresh_inner(
        &self,
        after_discovery: impl FnOnce(),
        before_publish: impl FnOnce(),
    ) -> Result<ReconcileStats> {
        self.refresh_controlled_inner(after_discovery, before_publish, None)
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
        self.refresh_controlled_inner(|| {}, || {}, Some(permit))
    }

    fn refresh_controlled_inner(
        &self,
        after_discovery: impl FnOnce(),
        before_publish: impl FnOnce(),
        permit: Option<&Arc<crate::managed::WorkPermit>>,
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
        let (epoch, full, hints) = {
            let mut control = self
                .control
                .lock()
                .map_err(|_| WorktreeError::Synchronization)?;
            let epoch = control.invalidate()?;
            let full = control.full || control.hints.is_empty();
            // Any failed/panicking pass leaves a full retry queued.
            control.full = true;
            (epoch, full, control.hints.clone())
        };
        let prepared = self.prepare(full, &hints, after_discovery, permit)?;
        before_publish();
        self.files.verify_root()?;
        let mut control = self
            .control
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        if control.epoch == u64::MAX {
            return Err(WorktreeError::Synchronization);
        }
        if control.epoch != epoch {
            return Err(WorktreeError::ChangedDuringReconcile);
        }
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
                if let Some(memory) = &mut memory {
                    memory
                        .grow(path.len() as u64 * 4 + 256)
                        .map_err(crate::Error::from)?;
                }
                live.delete_file(path);
            }
        }
        for (path, masks) in upserts {
            if let Some(memory) = &mut memory {
                let bytes = (masks.len() as u64)
                    .checked_mul(256)
                    .and_then(|bytes| bytes.checked_add(path.len() as u64 * 4 + 256))
                    .ok_or_else(|| {
                        crate::Error::from(crate::managed::Error::pressure(
                            "overlay-publication-memory-overflow",
                        ))
                    })?;
                memory.grow(bytes).map_err(crate::Error::from)?;
            }
            live.commit_upsert(&path, masks);
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
        stats.epoch = epoch;
        control.published_epoch = Some(epoch);
        control.full = false;
        control.hints.clear();
        control.ready = true;
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
    ) -> Result<Prepared> {
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
            evidence: BTreeMap::new(),
            visibility: walk.visibility,
            upserts: BTreeMap::new(),
            stats: ReconcileStats {
                full,
                files_discovered: walk.listed_files.len(),
                ..ReconcileStats::default()
            },
        };
        let state = self
            .state
            .read()
            .map_err(|_| WorktreeError::Synchronization)?;
        let mut base_contents = None;
        let mut copies: HashMap<u32, Vec<String>> = HashMap::new();
        for path in walk.listed_files {
            if let Some(permit) = permit {
                permit.check().map_err(crate::Error::from)?;
            }
            relative_path(Path::new(&path))?;
            let file = self.files.open_file(Path::new(&path))?;
            let metadata = file.metadata()?;
            let version = file_version(&metadata);
            let previous = state.evidence.get(&path);
            // Case aliases must not lose notifications on case-insensitive
            // filesystems. Extra verification on case-sensitive trees is safe.
            let hinted = !full && is_hinted(&path, hints, &mut prepared.stats.hint_lookups);
            let evidence = if !full
                && !hinted
                && version.is_trusted()
                && previous.is_some_and(|entry| entry.version == version)
            {
                prepared.stats.content_reads_avoided += 1;
                previous.expect("verified previous entry").clone()
            } else {
                let (bytes, version, _content_memory) = read_file(
                    &self.files,
                    Path::new(&path),
                    file,
                    self.options.walk.max_file_size,
                    permit,
                )?;
                prepared.stats.files_read += 1;
                prepared.stats.bytes_read += bytes.len() as u64;
                let listed = self
                    .options
                    .walk
                    .max_file_size
                    .is_none_or(|limit| bytes.len() as u64 <= limit);
                let content = if listed && !walker::is_binary_extension(Path::new(&path)) {
                    let text = crate::encoding::decode_for_index_controlled(&bytes, permit)
                        .map_err(crate::Error::from)?;
                    prepared.stats.files_decoded += 1;
                    if crate::trigram::is_binary(&text) {
                        None
                    } else {
                        let id = ContentId::from_indexed_bytes_controlled(&text, permit)
                            .map_err(crate::Error::from)?;
                        if self.base_content(&path) != Some(id)
                            && !(previous
                                .and_then(|entry| entry.content)
                                .or_else(|| state.restored_content.get(&path).copied())
                                == Some(id)
                                && state.index.live.has_path(&path))
                        {
                            if base_contents.is_none() {
                                if let Some(memory) = &mut prepared.memory {
                                    memory
                                        .grow(
                                            (self.generation.base().reader().all_paths().len()
                                                as u64)
                                                .checked_mul(128)
                                                .ok_or_else(|| {
                                                    crate::Error::from(
                                                        crate::managed::Error::pressure(
                                                            "base-copy-memory-overflow",
                                                        ),
                                                    )
                                                })?,
                                        )
                                        .map_err(crate::Error::from)?;
                                }
                                let mut contents = HashMap::new();
                                for (file_id, path) in self
                                    .generation
                                    .base()
                                    .reader()
                                    .all_paths()
                                    .iter()
                                    .enumerate()
                                {
                                    if let Some(permit) = permit {
                                        permit.check().map_err(crate::Error::from)?;
                                    }
                                    if let Some(id) = self.base_content(path) {
                                        contents.insert(id, file_id as u32);
                                    }
                                }
                                base_contents = Some(contents);
                            }
                            let contents = base_contents.as_ref().expect("base content map");
                            if let Some(&file_id) = contents.get(&id) {
                                copies.entry(file_id).or_default().push(path.clone());
                                prepared.stats.base_files_copied += 1;
                                prepared
                                    .upserts
                                    .insert(path.clone(), TrigramMaskMap::default());
                            } else {
                                let masks = match (permit, &mut prepared.memory) {
                                    (Some(permit), Some(memory)) => {
                                        crate::trigram::extract_merged_masks_controlled(
                                            &text, permit, memory,
                                        )
                                        .map_err(crate::Error::from)?
                                    }
                                    _ => LiveIndex::compute_trigram_masks(&text),
                                };
                                prepared.stats.files_extracted += 1;
                                prepared.upserts.insert(path.clone(), masks);
                            }
                        }
                        Some(id)
                    }
                } else {
                    None
                };
                Evidence {
                    version,
                    listed,
                    content,
                }
            };
            if let Some(id) = evidence.content {
                if self.base_content(&path) == Some(id) {
                    prepared.stats.base_reused += 1;
                } else if !prepared.upserts.contains_key(&path) {
                    prepared.stats.overlay_reused += 1;
                }
            }
            if prepared.evidence.insert(path, evidence).is_some() {
                return Err(WorktreeError::InvalidInput(
                    "duplicate checkout path".into(),
                ));
            }
        }
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
                            if let Some(memory) = &mut prepared.memory {
                                memory.grow(128).map_err(crate::Error::from)?;
                            }
                            prepared
                                .upserts
                                .get_mut(path)
                                .expect("copy destination")
                                .insert(
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
        Ok(prepared)
    }

    /// Hold the readiness, overlay, visibility and membership guards together.
    /// Owned paths and read-only file handles may escape; live IDs never do.
    /// Do not reenter this view (including invalidation) from the closure.
    /// Final matching must use `snapshot.open_file()` or a private versioned
    /// cache. Root verification brackets the callback, including empty results;
    /// root or candidate-open failure closes readiness and requires full
    /// reconciliation, even if the callback handles the candidate's error.
    pub fn with_snapshot<T>(&self, read: impl FnOnce(WorktreeSnapshot<'_>) -> T) -> Result<T> {
        let mut control = self
            .control
            .lock()
            .map_err(|_| WorktreeError::Synchronization)?;
        if !control.ready {
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
        if !control.ready {
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
        if !control.ready {
            return Err(crate::managed::Error::busy("view-not-ready"));
        }
        self.files.verify_root()?;
        let state = self
            .state
            .read()
            .map_err(|_| crate::managed::Error::corrupt("view state poisoned"))?;
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
            epoch: control.epoch,
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
        if !control.ready {
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
}

impl WorktreeSnapshot<'_> {
    pub fn root(&self) -> &Path {
        self.root
    }

    /// Open a root-relative candidate through the view's retained root handle.
    /// The returned read-only regular-file handle may outlive this guard, but
    /// does not freeze file contents. Bound reads and check the final snapshot
    /// epoch before publishing a result assembled outside the guard.
    /// An open failure also makes `with_snapshot` fail and invalidate readiness.
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

    fn open_inner(&self, relative: &str) -> std::io::Result<File> {
        let result = self.rooted.open_file(Path::new(relative));
        if let Err(error) = &result {
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

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn visibility(&self) -> &PathVisibility {
        &self.state.visibility
    }

    /// Root-relative candidates, resolved while the live-overlay guard is held.
    /// `prefix` is empty or a root-relative directory with a trailing slash.
    pub fn candidates(&self, plan: &QueryPlan, prefix: &str, include_hidden: bool) -> Vec<String> {
        let (ids, reader) = self.state.index.execute_query_with_masks(plan);
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
        self.state
            .listed
            .iter()
            .filter(|path| {
                self.state
                    .visibility
                    .is_visible(path, prefix, include_hidden)
            })
            .cloned()
            .collect()
    }
}

struct Prepared {
    memory: Option<crate::managed::work::MemoryCharge>,
    evidence: BTreeMap<String, Evidence>,
    visibility: PathVisibility,
    upserts: BTreeMap<String, TrigramMaskMap>,
    stats: ReconcileStats,
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

fn read_file(
    root: &RootedDir,
    path: &Path,
    mut file: File,
    limit: Option<u64>,
    permit: Option<&Arc<crate::managed::WorkPermit>>,
) -> Result<(
    Vec<u8>,
    FileVersion,
    Option<crate::managed::work::MemoryCharge>,
)> {
    let opened = file.metadata()?;
    let version = file_version(&opened);
    let mut bytes = Vec::new();
    let mut memory = permit
        .map(|permit| permit.memory(64 * 1024))
        .transpose()
        .map_err(crate::Error::from)?;
    let work_limit = permit
        .map(|permit| {
            permit
                .namespace
                .policy()
                .map(|policy| policy.policy.work.blob_bytes)
        })
        .transpose()
        .map_err(crate::Error::from)?;
    let read_limit = limit
        .map_or(u64::MAX, |limit| limit.saturating_add(1))
        .min(work_limit.map_or(u64::MAX, |limit| limit.saturating_add(1)));
    let mut input = (&mut file).take(read_limit);
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        if let Some(permit) = permit {
            permit.check().map_err(crate::Error::from)?;
        }
        let count = input.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        if work_limit.is_some_and(|limit| bytes.len() as u64 + count as u64 > limit) {
            return Err(crate::Error::from(crate::managed::Error::pressure(
                "checkout-blob-read-limit",
            ))
            .into());
        }
        if let Some(memory) = &mut memory {
            memory.grow(count as u64 * 6).map_err(crate::Error::from)?;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let current = root.open_file(path)?;
    if version != file_version(&file.metadata()?)
        || version != file_version(&current.metadata()?)
        || same_file::Handle::from_file(file)? != same_file::Handle::from_file(current)?
        || (limit.is_none_or(|limit| bytes.len() as u64 <= limit)
            && bytes.len() as u64 != opened.len())
    {
        return Err(WorktreeError::UnstableFile(path.into()));
    }
    Ok((bytes, version, memory))
}

#[cfg(test)]
mod tests;
