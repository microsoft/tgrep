//! Immutable, repository-scoped indexes of committed Git trees.
//!
//! This is not worktree synchronization. A generation indexes a tracked-path
//! superset using **raw Git blobs**, not checkout-filtered bytes. An agent runtime
//! must reconcile content and membership before searching a worktree view.
//! Published generations are retained indefinitely; there is no online GC.

pub(crate) mod git;

pub(crate) fn worktree_root(root: &Path) -> Result<PathBuf> {
    git::worktree_root(root)
}

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde::{Deserialize, Serialize};

use crate::builder;
use crate::external::{ExternalSorter, TrigramPosting};
use crate::meta::{ContentId, INDEX_FORMAT_VERSION};
use crate::shared::SharedBase;
use crate::{PostingEntry, encoding, trigram};

const SCHEMA_VERSION: u32 = 1;
const STORE_NAME: &str = "tgrep-bases-v1";
const MANIFEST: &str = "generation.json";
const INDEX_FILES: [&str; 4] = ["files.bin", "lookup.bin", "index.bin", "meta.json"];

/// Failures are explicit: callers may reconcile/scan, never use an empty base.
#[derive(Debug)]
pub enum GenerationError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Index(crate::Error),
    Git {
        operation: &'static str,
        code: Option<i32>,
        stderr: String,
    },
    Unsupported(String),
    InvalidMetadata(String),
    Incompatible(String),
    Synchronization,
}

impl fmt::Display for GenerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "generation I/O error: {error}"),
            Self::Json(error) => write!(f, "generation JSON error: {error}"),
            Self::Index(error) => write!(f, "generation index error: {error}"),
            Self::Git {
                operation,
                code,
                stderr,
            } => write!(f, "Git {operation} failed ({code:?}): {stderr}"),
            Self::Unsupported(reason) => write!(f, "unsupported shared generation: {reason}"),
            Self::InvalidMetadata(reason) => write!(f, "invalid generation metadata: {reason}"),
            Self::Incompatible(reason) => write!(f, "incompatible generation: {reason}"),
            Self::Synchronization => write!(f, "generation cache mutex is poisoned"),
        }
    }
}

impl std::error::Error for GenerationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Index(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for GenerationError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<serde_json::Error> for GenerationError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
impl From<crate::Error> for GenerationError {
    fn from(error: crate::Error) -> Self {
        Self::Index(error)
    }
}

impl From<crate::managed::Error> for GenerationError {
    fn from(error: crate::managed::Error) -> Self {
        Self::Index(error.into())
    }
}

pub type Result<T> = std::result::Result<T, GenerationError>;

/// Identity is the canonical native Git common-directory path, not a remote.
#[derive(Clone, Debug)]
pub struct Repository {
    common_dir: PathBuf,
    git_dir: PathBuf,
    identity: String,
    oid_length: usize,
}

impl Repository {
    /// Discover from a worktree, a subdirectory, or a bare Git repository.
    /// Git must be installed. Ambient `GIT_*` overrides are not inherited.
    pub fn discover(root: &Path) -> Result<Self> {
        Self::discover_inner(root, None)
    }

    pub(crate) fn discover_controlled(
        root: &Path,
        control: &crate::managed::process::Control,
    ) -> Result<Self> {
        Self::discover_inner(root, Some(control))
    }

    fn discover_inner(
        root: &Path,
        control: Option<&crate::managed::process::Control>,
    ) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let common_dir = git::discover_directory(&root, "--git-common-dir", control)?;
        let git_dir = git::discover_directory(&root, "--absolute-git-dir", control)?;
        let identity = Self::identity_of(&common_dir)?;
        let mut repository = Self {
            common_dir,
            git_dir,
            identity,
            oid_length: 0,
        };
        repository.oid_length = git::object_format_controlled(&repository, control)?;
        Ok(repository)
    }

    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }
    /// Metadata for the discovered worktree; symbolic revisions (not identity)
    /// resolve here, so `HEAD` means this worktree's HEAD.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// The identity [`Repository::discover`] derives for a repository whose
    /// canonical native Git common directory is `common_dir`. Computing it
    /// validates nothing and grants no authority.
    pub fn identity_of(common_dir: &Path) -> Result<String> {
        let mut hash = blake3::Hasher::new();
        hash.update(b"tgrep-repository-v1\0");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            hash.update(b"unix\0");
            hash.update(common_dir.as_os_str().as_bytes());
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            hash.update(b"windows\0");
            for unit in common_dir.as_os_str().encode_wide() {
                hash.update(&unit.to_le_bytes());
            }
        }
        #[cfg(not(any(unix, windows)))]
        return Err(GenerationError::Unsupported(
            "repository identity on this platform".into(),
        ));
        Ok(hash.finalize().to_hex().to_string())
    }

    /// Resolve a revision to its exact (commit, tree) IDs without building or
    /// publishing a generation. Symbolic revisions use this worktree's HEAD.
    pub fn resolve_commit_tree(&self, revision: &str) -> Result<(String, String)> {
        git::commit_tree(self, revision)
    }

    pub(crate) fn resolve_controlled(
        &self,
        revision: &str,
        control: &crate::managed::process::Control,
    ) -> Result<(String, String)> {
        git::commit_tree_controlled(self, revision, Some(control))
    }

    pub(crate) fn root_controlled(
        root: &Path,
        control: &crate::managed::process::Control,
    ) -> Result<PathBuf> {
        git::discover_directory(root, "--show-toplevel", Some(control))
    }

    pub(crate) fn validate_managed_storage(&self, storage: &Path) -> Result<()> {
        GenerationManager::validate_external_storage(
            self,
            storage,
            &git::worktrees_controlled(self, Some(&crate::managed::process::Control::bootstrap()))?,
        )
    }
}

impl PartialEq for Repository {
    fn eq(&self, other: &Self) -> bool {
        self.common_dir == other.common_dir
    }
}

impl Eq for Repository {}

/// Content and extraction compatibility, independently versioned from disk layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentSemantics {
    /// Raw blobs -> `decode_for_index` -> NUL binary classification -> masked trigrams.
    /// Never applies CRLF, working-tree-encoding, LFS, or clean/smudge filters.
    RawGitBlobAutoV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Coverage {
    /// All tracked regular files, including hidden/ignored/extension-filtered paths.
    /// Symlinks and gitlinks are membership records only, never followed.
    TrackedRegularFilesV1,
}

/// Which listed worktree files a view may expose for content search.
///
/// Membership, size limits and filename coverage are identical in both modes;
/// only the filename-extension heuristic differs. NUL-classified content is
/// excluded either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentEligibility {
    /// Legacy default: skip `walker::BINARY_EXTENSIONS` paths, then classify content.
    #[default]
    ExtensionAndContent,
    /// Classify content only, as ripgrep does, regardless of filename extension.
    ContentOnly,
}

impl ContentEligibility {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Whether the extension heuristic alone excludes `relative`'s content.
    pub fn excludes_by_extension(self, relative: &Path) -> bool {
        self == Self::ExtensionAndContent && crate::walker::is_binary_extension(relative)
    }
}

/// Exact, serializable compatibility profile. Budgets are not part of identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexingProfile {
    pub content: ContentSemantics,
    pub coverage: Coverage,
    /// Limit on raw blob size; `None` removes the cap (one blob is held in memory).
    pub max_blob_bytes: Option<u64>,
    /// Omitted when default so legacy keys, storage names and peers are unchanged;
    /// peers that predate the field reject a non-default value as unknown.
    #[serde(default, skip_serializing_if = "ContentEligibility::is_default")]
    pub eligibility: ContentEligibility,
}

impl Default for IndexingProfile {
    fn default() -> Self {
        Self {
            content: ContentSemantics::RawGitBlobAutoV1,
            coverage: Coverage::TrackedRegularFilesV1,
            max_blob_bytes: Some(64 * 1024 * 1024),
            eligibility: ContentEligibility::ExtensionAndContent,
        }
    }
}

/// Persist this key beside an overlay checkpoint to reopen its exact base.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationKey {
    repository: String,
    tree: String,
    profile: IndexingProfile,
    index_format: u32,
    schema: u32,
}

impl GenerationKey {
    pub(crate) fn legacy(repository: &Repository, tree: String, profile: IndexingProfile) -> Self {
        Self {
            repository: repository.identity.clone(),
            tree,
            profile,
            index_format: INDEX_FORMAT_VERSION,
            schema: SCHEMA_VERSION,
        }
    }

    pub(crate) fn managed(
        repository: &Repository,
        tree: String,
        profile: IndexingProfile,
    ) -> Result<Self> {
        if !git::valid_oid(&tree, repository.oid_length) {
            return Err(GenerationError::InvalidMetadata(
                "invalid managed tree OID".into(),
            ));
        }
        Ok(Self {
            repository: repository.identity.clone(),
            tree,
            profile,
            index_format: crate::ondisk::MANAGED_FILE_TABLE_VERSION,
            schema: crate::managed::STORAGE_VERSION,
        })
    }
    pub fn repository_identity(&self) -> &str {
        &self.repository
    }
    pub fn tree_oid(&self) -> &str {
        &self.tree
    }
    pub fn profile(&self) -> &IndexingProfile {
        &self.profile
    }
    pub fn index_format(&self) -> u32 {
        self.index_format
    }

    pub fn storage_name(&self) -> String {
        // Fixed, infallibly serializable fields; never use a Git path as a filename.
        let bytes = serde_json::to_vec(self).expect("serializable generation key");
        format!("gen-{}", blake3::hash(&bytes).to_hex())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryMode {
    File,
    Executable,
    Symlink,
    Gitlink,
}

impl EntryMode {
    pub fn is_regular(self) -> bool {
        matches!(self, Self::File | Self::Executable)
    }
}

/// Indexed empty/short files have an identity even when they have no postings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum EntryContent {
    Indexed { content_id: ContentId },
    Binary,
    TooLarge,
    NotRegular,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackedEntry {
    pub path: String,
    pub oid: String,
    pub mode: EntryMode,
    pub size: Option<u64>,
    pub content: EntryContent,
}

impl TrackedEntry {
    pub fn content_id(&self) -> Option<ContentId> {
        match self.content {
            EntryContent::Indexed { content_id } => Some(content_id),
            _ => None,
        }
    }

    /// Prove decoded-content equality, not filesystem freshness or eligibility.
    /// The caller must read a stable file version and apply worktree membership.
    pub fn matches_worktree_bytes(&self, bytes: &[u8]) -> bool {
        self.content_id().is_some_and(|id| {
            let text = encoding::decode_for_index(bytes);
            !trigram::is_binary(&text) && ContentId::from_indexed_bytes(&text) == id
        })
    }
}

/// Actual work performed by this request, not the generation's historical build.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildStats {
    pub published: bool,
    pub reused_generation: bool,
    pub tracked_entries: usize,
    pub blobs_read: u64,
    pub blob_bytes_read: u64,
    /// Calls to trigram extraction; binary/oversized/nonregular entries excluded.
    pub blobs_extracted: u64,
    pub reused_indexed_files: u64,
    pub postings_reused: u64,
    /// Predecessor posting lists decoded for reuse; zero when none can contribute.
    pub predecessor_posting_lists_read: u64,
}

/// Conservative retention protects even escaped raw readers and disk checkpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionPolicy {
    /// No published directory is ever removed, including after all pins drop.
    /// Offline reclamation requires stopping all users and discarding checkpoints.
    RetainAll,
    /// Managed references and OS reader guards determine physical eligibility.
    Managed,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    key: GenerationKey,
    commit: String,
    base_id: [u8; 32],
    entries: Vec<TrackedEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedManifest {
    checksum: [u8; 32],
    manifest: Manifest,
}

impl SealedManifest {
    fn seal(manifest: Manifest) -> Result<Self> {
        Self::seal_controlled(manifest, None)
    }

    fn seal_controlled(
        manifest: Manifest,
        permit: Option<&Arc<crate::managed::WorkPermit>>,
    ) -> Result<Self> {
        let checksum = crate::output::checksum(&manifest, permit)?;
        Ok(Self { checksum, manifest })
    }
}

/// An `Arc<Generation>` is the lifecycle pin handed to a worktree registration.
/// Keep it with the view and persist `key()` with checkpoints. Retain-all also
/// protects views/readers that outlive this pin; dropping it never deletes files.
pub struct Generation {
    directory: PathBuf,
    manifest: Manifest,
    base: Arc<SharedBase>,
}

impl Generation {
    pub fn key(&self) -> &GenerationKey {
        &self.manifest.key
    }
    /// The first publishing commit; different commits can have this same tree.
    pub fn commit_oid(&self) -> &str {
        &self.manifest.commit
    }
    pub fn entries(&self) -> &[TrackedEntry] {
        &self.manifest.entries
    }
    pub fn entry(&self, path: &str) -> Option<&TrackedEntry> {
        self.entries()
            .binary_search_by(|entry| entry.path.as_str().cmp(path))
            .ok()
            .map(|index| &self.entries()[index])
    }
    pub fn base(&self) -> &Arc<SharedBase> {
        &self.base
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn retention(&self) -> RetentionPolicy {
        if self.base.reader().managed_identity().is_some() {
            RetentionPolicy::Managed
        } else {
            RetentionPolicy::RetainAll
        }
    }

    pub(crate) fn load_managed(
        pin: Arc<crate::managed::lifetime::ObjectGuard>,
        key: &GenerationKey,
        manifest_limit: u64,
        permit: Option<&Arc<crate::managed::WorkPermit>>,
        limits: &crate::reader::SnapshotLimits,
    ) -> Result<Self> {
        let sealed: SealedManifest = crate::managed::inputs::read_json(
            pin.directory.open_file("generation.tgm", false)?,
            manifest_limit,
            permit,
        )?;
        if crate::output::checksum(&sealed.manifest, permit)? != sealed.checksum {
            return Err(GenerationError::InvalidMetadata(
                "managed manifest checksum mismatch".into(),
            ));
        }
        let manifest = sealed.manifest;
        let length = manifest.commit.len();
        if &manifest.key != key
            || !matches!(length, 40 | 64)
            || !git::valid_oid(&manifest.commit, length)
            || !git::valid_oid(&key.tree, length)
            || key.schema != crate::managed::STORAGE_VERSION
            || key.index_format != crate::ondisk::MANAGED_FILE_TABLE_VERSION
        {
            return Err(GenerationError::InvalidMetadata(
                "managed manifest identity differs".into(),
            ));
        }
        let directory = pin.directory.path().to_path_buf();
        let base = Arc::new(SharedBase::open_managed(pin, permit, limits)?);
        if base.snapshot_id() != manifest.base_id {
            return Err(GenerationError::InvalidMetadata(
                "managed manifest fingerprint differs".into(),
            ));
        }
        validate_entries_controlled(&manifest, &base, length, permit)?;
        Ok(Self {
            directory,
            manifest,
            base,
        })
    }
}

pub struct EnsureResult {
    pub generation: Arc<Generation>,
    pub stats: BuildStats,
    /// The requested commit, including on reuse of a tree from another commit.
    pub requested_commit: String,
}

type Cache = HashMap<(PathBuf, Option<crate::managed::Id>), Weak<Generation>>;
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

fn cached_generation(
    directory: PathBuf,
    namespace: Option<crate::managed::Id>,
    load: impl FnOnce() -> Result<Generation>,
) -> Result<Arc<Generation>> {
    let identity = (directory, namespace);
    let cache = CACHE.get_or_init(Default::default);
    {
        let cache = cache.lock().map_err(|_| GenerationError::Synchronization)?;
        if let Some(generation) = cache.get(&identity).and_then(Weak::upgrade) {
            return Ok(generation);
        }
    }
    let generation = Arc::new(load()?);
    let mut cache = cache.lock().map_err(|_| GenerationError::Synchronization)?;
    if let Some(existing) = cache.get(&identity).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    cache.retain(|_, value| value.strong_count() > 0);
    cache.insert(identity, Arc::downgrade(&generation));
    Ok(generation)
}

/// Additive core API; ordinary CLI/server index paths never use this manager.
///
/// Cooperating processes serialize publication with an OS file lock (released
/// on crash). The repository store and its ancestors must be trusted and must
/// not be externally renamed, rewritten, or deleted while in use. Published
/// generation files must never be passed to a mutable index builder.
#[derive(Clone)]
pub struct GenerationManager {
    repository: Repository,
    directory: PathBuf,
}

impl GenerationManager {
    /// Repository-owned common-dir storage, outside linked worktree directories.
    pub fn new(repository: Repository) -> Result<Self> {
        let directory = repository.common_dir().join(STORE_NAME);
        create_plain_directory(&directory)?;
        Self::initialize(repository, &directory)
    }

    /// An existing, trusted storage directory outside all registered worktrees
    /// and Git metadata. Unrelated repositories get separate identity subdirs.
    /// Both the supplied parent and the effective subdirectory are validated.
    pub fn with_storage(repository: Repository, storage: &Path) -> Result<Self> {
        let storage = fs::canonicalize(storage)?;
        let worktrees = git::worktrees(&repository)?;
        Self::validate_external_storage(&repository, &storage, &worktrees)?;
        let manager = Self::initialize(repository, &storage)?;
        Self::validate_external_storage(&manager.repository, &manager.directory, &worktrees)?;
        Ok(manager)
    }

    fn validate_external_storage(
        repository: &Repository,
        storage: &Path,
        worktrees: &[PathBuf],
    ) -> Result<()> {
        if storage.starts_with(repository.common_dir()) {
            return Err(GenerationError::Unsupported(
                "use new() for Git common-directory storage".into(),
            ));
        }
        for worktree in worktrees {
            // Missing/stale worktree registrations do not make a live storage
            // directory unsafe; existing prefixes are checked canonically.
            match fs::canonicalize(worktree) {
                Ok(root) if storage.starts_with(&root) => {
                    return Err(GenerationError::Unsupported(
                        "generation storage is inside a worktree".into(),
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if storage.starts_with(worktree) {
                        return Err(GenerationError::Unsupported(
                            "storage is inside a registered worktree".into(),
                        ));
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn initialize(repository: Repository, storage: &Path) -> Result<Self> {
        let storage = fs::canonicalize(storage)?;
        Self::validate_snapshot_boundary(&storage)?;
        let directory = storage.join(repository.identity());
        create_plain_directory(&directory)?;
        let directory = fs::canonicalize(directory)?;
        Self::validate_snapshot_boundary(&directory)?;
        Ok(Self {
            repository,
            directory,
        })
    }

    fn validate_snapshot_boundary(storage: &Path) -> Result<()> {
        crate::managed::reject_unguarded(storage)?;
        for ancestor in storage.ancestors() {
            if ancestor.join(MANIFEST).try_exists()?
                || (ancestor.join("files.bin").try_exists()?
                    && ancestor.join("index.bin").try_exists()?)
            {
                return Err(GenerationError::Unsupported(
                    "storage must not be inside an index snapshot".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn repository(&self) -> &Repository {
        &self.repository
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn retention(&self) -> RetentionPolicy {
        RetentionPolicy::RetainAll
    }

    /// Build or reuse an exact committed tree/profile. A supplied predecessor
    /// must have the same repository/profile/format; incompatibility is an error.
    /// Without one, initial extraction is complete, not automatically incremental.
    pub fn ensure(
        &self,
        revision: &str,
        profile: IndexingProfile,
        predecessor: Option<&Arc<Generation>>,
    ) -> Result<EnsureResult> {
        self.ensure_inner(revision, profile, predecessor, None)
    }

    pub(crate) fn ensure_controlled(
        &self,
        revision: &str,
        profile: IndexingProfile,
        predecessor: Option<&Arc<Generation>>,
        permit: &Arc<crate::managed::WorkPermit>,
    ) -> Result<EnsureResult> {
        self.validate_compatibility_permit(permit)?;
        self.ensure_inner(revision, profile, predecessor, Some(permit))
    }

    fn validate_compatibility_permit(
        &self,
        permit: &Arc<crate::managed::WorkPermit>,
    ) -> Result<()> {
        if permit.namespace.header().repository != self.repository.identity
            || permit.namespace.header().storage
                != crate::managed::policy::StorageMode::CompatibilityRetainAll
        {
            return Err(crate::managed::Error::incompatible(
                "legacy access requires this repository's compatibility namespace",
            )
            .into());
        }
        Ok(())
    }

    fn ensure_inner(
        &self,
        revision: &str,
        profile: IndexingProfile,
        predecessor: Option<&Arc<Generation>>,
        permit: Option<&Arc<crate::managed::WorkPermit>>,
    ) -> Result<EnsureResult> {
        let control = permit.map(crate::managed::process::Control::work);
        let (commit, tree) =
            git::commit_tree_controlled(&self.repository, revision, control.as_ref())?;
        let key = GenerationKey::legacy(&self.repository, tree, profile);
        if let Some(previous) = predecessor {
            let mut expected = key.clone();
            expected.tree = previous.key().tree.clone();
            if previous.key() != &expected {
                return Err(GenerationError::Incompatible(
                    "predecessor repository/profile/format differs".into(),
                ));
            }
        }
        let _lock = self.lock_controlled(permit)?;
        let directory = self.directory.join(key.storage_name());
        if directory.try_exists()? {
            let generation = self.open_locked_controlled(&key, permit)?;
            let stats = BuildStats {
                reused_generation: true,
                tracked_entries: generation.entries().len(),
                ..BuildStats::default()
            };
            return Ok(EnsureResult {
                generation,
                stats,
                requested_commit: commit,
            });
        }
        let mut stats = self.build(&key, &commit, predecessor, permit)?;
        let generation = self.open_locked_controlled(&key, permit)?;
        stats.published = true;
        Ok(EnsureResult {
            generation,
            stats,
            requested_commit: commit,
        })
    }

    /// Open only this exact published key. Missing, partial or invalid generations
    /// error; they are never repaired/replaced while another reader could map them.
    pub fn open(&self, key: &GenerationKey) -> Result<Arc<Generation>> {
        self.validate_key(key)?;
        let _lock = self.lock()?;
        self.open_locked(key)
    }

    pub(crate) fn open_controlled(
        &self,
        key: &GenerationKey,
        permit: &Arc<crate::managed::WorkPermit>,
    ) -> Result<Arc<Generation>> {
        self.validate_compatibility_permit(permit)?;
        self.validate_key(key)?;
        let _lock = self.lock_controlled(Some(permit))?;
        self.open_locked_controlled(key, Some(permit))
    }

    /// Published keys only; interrupted `.stage-*` directories are not candidates.
    /// Malformed published metadata is an error, not a silently skipped generation.
    pub fn list(&self) -> Result<Vec<GenerationKey>> {
        let _lock = self.lock()?;
        let mut keys = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with("gen-") {
                continue;
            }
            check_plain(&entry.path(), true)?;
            let manifest = read_manifest(&entry.path())?;
            self.validate_key(&manifest.key)?;
            if entry.file_name() != manifest.key.storage_name().as_str() {
                return Err(GenerationError::InvalidMetadata(
                    "generation name/key mismatch".into(),
                ));
            }
            // Listing must not advertise a partially written final directory.
            self.open_locked(&manifest.key)?;
            keys.push(manifest.key);
        }
        keys.sort_unstable_by_key(GenerationKey::storage_name);
        Ok(keys)
    }

    fn validate_key(&self, key: &GenerationKey) -> Result<()> {
        if key.repository != self.repository.identity
            || key.index_format != INDEX_FORMAT_VERSION
            || key.schema != SCHEMA_VERSION
        {
            return Err(GenerationError::Incompatible(
                "repository, index format or generation schema differs".into(),
            ));
        }
        if !git::valid_oid(&key.tree, self.repository.oid_length) {
            return Err(GenerationError::InvalidMetadata("invalid tree OID".into()));
        }
        Ok(())
    }

    fn lock(&self) -> Result<File> {
        self.lock_controlled(None)
    }

    fn lock_controlled(&self, permit: Option<&Arc<crate::managed::WorkPermit>>) -> Result<File> {
        check_plain(&self.directory, true)?;
        let path = self.directory.join("publication.lock");
        if path.try_exists()? {
            check_plain(&path, false)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        // Keep locking compatible with the Windows release pipeline's older Rust toolchain.
        if let Some(permit) = permit {
            loop {
                permit.check()?;
                match fs2::FileExt::try_lock_exclusive(&file) {
                    Ok(()) => break,
                    Err(error) => {
                        let error = crate::managed::Error::io(error);
                        if error.category != crate::managed::ErrorCategory::Busy {
                            return Err(error.into());
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            }
        } else {
            fs2::FileExt::lock_exclusive(&file)?;
        }
        Ok(file)
    }

    fn open_locked(&self, key: &GenerationKey) -> Result<Arc<Generation>> {
        self.open_locked_controlled(key, None)
    }

    fn open_locked_controlled(
        &self,
        key: &GenerationKey,
        permit: Option<&Arc<crate::managed::WorkPermit>>,
    ) -> Result<Arc<Generation>> {
        if let Some(permit) = permit {
            permit.check()?;
        }
        self.validate_key(key)?;
        let directory = self.directory.join(key.storage_name());
        cached_generation(
            directory.clone(),
            permit.map(|permit| permit.namespace.header().namespace.clone()),
            || self.load(&directory, key, permit),
        )
    }

    fn load(
        &self,
        directory: &Path,
        key: &GenerationKey,
        permit: Option<&Arc<crate::managed::WorkPermit>>,
    ) -> Result<Generation> {
        check_plain(directory, true)?;
        for name in INDEX_FILES {
            check_plain(&directory.join(name), false)?;
        }
        let loading = permit
            .map(|permit| -> Result<_> {
                use std::io::{Seek, SeekFrom};
                permit.check()?;
                let directory = crate::managed::storage::Directory::open(directory)?;
                let limits = crate::reader::SnapshotLimits::measure(
                    crate::ondisk::IndexLayout::Legacy,
                    |name| Ok(directory.open_file(name, false)?),
                )?;
                let manifest_size = directory
                    .open_file(MANIFEST, false)?
                    .seek(SeekFrom::End(0))?;
                let base_private = limits.private_estimate()?;
                let private = manifest_size
                    .checked_mul(6)
                    .and_then(|bytes| bytes.checked_add(base_private))
                    .ok_or_else(|| {
                        crate::managed::Error::pressure("generation-private-account-overflow")
                    })?;
                Ok((limits, manifest_size, permit.memory(private)?))
            })
            .transpose()?;
        let manifest = read_manifest_controlled(
            directory,
            loading.as_ref().map_or(u64::MAX, |(_, size, _)| *size),
            permit,
        )?;
        if &manifest.key != key || !git::valid_oid(&manifest.commit, self.repository.oid_length) {
            return Err(GenerationError::InvalidMetadata(
                "manifest key/commit mismatch".into(),
            ));
        }
        let mut base = match (&loading, permit) {
            (Some((limits, _, _)), Some(permit)) => {
                SharedBase::open_controlled(directory, permit, limits)?
            }
            _ => SharedBase::open(directory)?,
        };
        if base.snapshot_id() != manifest.base_id {
            return Err(GenerationError::InvalidMetadata(
                "manifest/index fingerprint mismatch".into(),
            ));
        }
        match permit {
            Some(_) => {
                validate_entries_controlled(&manifest, &base, self.repository.oid_length, permit)?
            }
            None => validate_entries(&manifest, &base, self.repository.oid_length)?,
        }
        if let Some((limits, _, memory)) = loading {
            base.retain_memory(memory.retain(limits.mapped_bytes()?)?)?;
        }
        Ok(Generation {
            directory: directory.to_path_buf(),
            manifest,
            base: Arc::new(base),
        })
    }

    fn build(
        &self,
        key: &GenerationKey,
        commit: &str,
        predecessor: Option<&Arc<Generation>>,
        permit: Option<&Arc<crate::managed::WorkPermit>>,
    ) -> Result<BuildStats> {
        let stage = tempfile::Builder::new()
            .prefix(".stage-")
            .tempdir_in(&self.directory)?;
        let output = match permit {
            Some(permit) => crate::output::Output::compatibility(stage.path(), permit),
            None => crate::output::Output::legacy(stage.path()),
        };
        let stats = Self::build_into(&self.repository, key, commit, predecessor, &output)?;
        drop(output);
        self.publish(stage, key, permit)?;
        Ok(stats)
    }

    pub(crate) fn build_into(
        repository: &Repository,
        key: &GenerationKey,
        commit: &str,
        predecessor: Option<&Arc<Generation>>,
        output: &crate::output::Output,
    ) -> Result<BuildStats> {
        Self::build_into_tuned(repository, key, commit, predecessor, output, None)
    }

    fn build_into_tuned(
        repository: &Repository,
        key: &GenerationKey,
        commit: &str,
        predecessor: Option<&Arc<Generation>>,
        output: &crate::output::Output,
        tuning: Option<ExtractionTuning>,
    ) -> Result<BuildStats> {
        output.check()?;
        let metadata_limit = output.control().map(|permit| permit.private_limit() / 64);
        let mut metadata_memory = output
            .control()
            .map(|permit| permit.memory(0))
            .transpose()?;
        let control = output.control().map(crate::managed::process::Control::work);
        let mut entries = git::entries_controlled(
            repository,
            &key.tree,
            control.as_ref(),
            metadata_limit
                .map_or(Ok(usize::MAX), usize::try_from)
                .map_err(|_| crate::managed::Error::pressure("tree-metadata-address-range"))?,
            metadata_memory.as_mut(),
        )?;
        let mut stats = BuildStats {
            tracked_entries: entries.len(),
            ..BuildStats::default()
        };
        let sort_bytes = match output.control() {
            Some(permit) => {
                usize::try_from(permit.read_limits().sort_buffer_bytes).map_err(|_| {
                    crate::managed::Error::invalid("sort buffer exceeds addressable memory")
                })?
            }
            None => builder::DEFAULT_INDEX_BUFFER_BYTES,
        };
        let _sort_memory = output
            .control()
            .map(|permit| permit.memory((sort_bytes.max(16384) as u64).saturating_mul(4)))
            .transpose()?;
        let mut sorter = ExternalSorter::with_output(output.clone(), sort_bytes);
        let mut paths = Vec::new();
        let mut previous_blobs = HashMap::new();
        if let Some(previous) = predecessor {
            if let Some(memory) = &mut metadata_memory {
                memory.grow(
                    (previous.entries().len() as u64)
                        .checked_mul(128)
                        .ok_or_else(|| {
                            crate::managed::Error::pressure("predecessor-metadata-memory-overflow")
                        })?,
                )?;
            }
            for entry in previous
                .entries()
                .iter()
                .filter(|entry| entry.mode.is_regular())
            {
                output.check()?;
                previous_blobs.entry(entry.oid.as_str()).or_insert(entry);
            }
        }
        let mut reuse: HashMap<&str, Vec<u32>> = HashMap::new();
        // Group by blob so repeated files also cause only one extraction.
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, entry) in entries.iter_mut().enumerate() {
            output.check()?;
            if !entry.mode.is_regular() {
                continue;
            }
            let size = entry.size.expect("regular blob size");
            if key.profile.max_blob_bytes.is_some_and(|limit| size > limit) {
                entry.content = EntryContent::TooLarge;
            } else {
                groups.entry(entry.oid.clone()).or_default().push(index);
            }
        }
        let groups: Vec<(String, u64, Vec<usize>)> = groups
            .into_iter()
            .map(|(oid, indices)| {
                let size = entries[indices[0]].size.expect("regular blob size");
                (oid, size, indices)
            })
            .collect();
        let reusable = |oid: &str| {
            previous_blobs.get(oid).copied().filter(|previous| {
                matches!(
                    previous.content,
                    EntryContent::Indexed { .. } | EntryContent::Binary
                )
            })
        };
        let extracted_blobs = groups
            .iter()
            .filter(|(oid, ..)| reusable(oid).is_none())
            .count();
        let mut tuning =
            tuning.unwrap_or_else(|| ExtractionTuning::new(output.control(), extracted_blobs));
        let batch_bytes = tuning.batch_bytes;
        let plan = |position: usize| {
            let mut end = position;
            let mut work = Vec::new();
            let mut charged = 0_u64;
            while let Some((oid, size, _)) = groups.get(end) {
                if reusable(oid).is_none() {
                    let charge = size.saturating_mul(6).saturating_add(64 * 1024);
                    if !work.is_empty() && charged.saturating_add(charge) > batch_bytes {
                        break;
                    }
                    charged = charged.saturating_add(charge);
                    work.push((oid.as_str(), *size));
                }
                end += 1;
            }
            (end, work)
        };
        let source = BlobSource {
            repository,
            control: control.as_ref(),
            permit: output.control(),
        };
        let abort = AtomicBool::new(false);
        let mut readers: Vec<Option<git::Blobs>> = vec![None];
        std::thread::scope(|scope| -> Result<()> {
            let mut prefetched: Option<Prefetch<'_, '_>> = None;
            let applied = (|| {
                let mut position = 0;
                // Stable file IDs make independent builds deterministic.
                while position < groups.len() {
                    let (end, mut results) = match prefetched.take() {
                        Some(prefetch) => prefetch.join(&mut readers),
                        None => {
                            let (end, work) = plan(position);
                            let (_parallelism, workers) =
                                parallelism(source.permit, tuning.workers.min(work.len()))?;
                            let mut results = Vec::new();
                            if workers > 1 {
                                if readers.len() < workers {
                                    readers.resize_with(workers, || None);
                                }
                                results =
                                    source.extract_parallel(&mut readers[..workers], &work, &abort);
                            }
                            (end, results)
                        }
                    };
                    if end < groups.len() && tuning.workers > 1 {
                        // Extraction of the next batch overlaps this batch's
                        // ordered ID assignment and posting spill.
                        let (next, work) = plan(end);
                        let (parallelism, workers) =
                            parallelism(source.permit, tuning.workers.min(work.len()))?;
                        if workers > 1 {
                            // The first reader stays available for serial retries.
                            let mut owned = readers.split_off(1);
                            if owned.len() < workers {
                                owned.resize_with(workers, || None);
                            }
                            let abort = &abort;
                            prefetched = Some(Prefetch {
                                end: next,
                                worker: scope.spawn(move || {
                                    let results = source.extract_parallel(
                                        &mut owned[..workers],
                                        &work,
                                        abort,
                                    );
                                    (owned, results)
                                }),
                                _parallelism: parallelism,
                            });
                        }
                    }
                    let mut next = 0;
                    for (oid, size, indices) in &groups[position..end] {
                        output.check()?;
                        if let (Some(previous), Some(generation)) = (reusable(oid), predecessor) {
                            let has_postings = if previous.content_id().is_some() {
                                generation
                                    .base()
                                    .reader()
                                    .snapshot_path_has_postings(&previous.path)
                                    .ok_or_else(|| {
                                        GenerationError::InvalidMetadata(
                                            "indexed predecessor lacks validated posting membership"
                                                .into(),
                                        )
                                    })?
                            } else {
                                false
                            };
                            for &index in indices {
                                let entry = &mut entries[index];
                                if entry.size != previous.size {
                                    return Err(GenerationError::InvalidMetadata(
                                        "same blob has different sizes".into(),
                                    ));
                                }
                                entry.content = previous.content.clone();
                                if entry.content_id().is_some() {
                                    let id = add_path(&mut paths, &entry.path)?;
                                    if has_postings {
                                        reuse.entry(&previous.path).or_default().push(id);
                                    }
                                    stats.reused_indexed_files += 1;
                                }
                            }
                            continue;
                        }
                        let extraction = match results.get_mut(next).and_then(Option::take) {
                            Some(Ok(extraction)) => extraction,
                            Some(Err(error)) if !is_private_pressure(&error) => {
                                return Err(error);
                            }
                            retry => {
                                if retry.is_some() {
                                    // Retained results and readers can exhaust private
                                    // memory; continue with one reservation at a time.
                                    results.clear();
                                    tuning.workers = 1;
                                    abort.store(true, Ordering::Relaxed);
                                    if let Some(prefetch) = prefetched.take() {
                                        prefetch.join(&mut readers);
                                    }
                                    for reader in readers.drain(1..).flatten() {
                                        reader.finish()?;
                                    }
                                }
                                source.extract(&mut readers[0], oid, *size, false)?
                            }
                        };
                        next += 1;
                        stats.blobs_read += 1;
                        stats.blob_bytes_read += size;
                        let Some((content_id, masks)) = &extraction.indexed else {
                            for &index in indices {
                                entries[index].content = EntryContent::Binary;
                            }
                            continue;
                        };
                        stats.blobs_extracted += 1;
                        for &index in indices {
                            let entry = &mut entries[index];
                            entry.content = EntryContent::Indexed {
                                content_id: *content_id,
                            };
                            let file_id = add_path(&mut paths, &entry.path)?;
                            sorter.push_file(
                                file_id,
                                masks.iter().map(|(&trigram, &masks)| (trigram, masks)),
                            )?;
                        }
                    }
                    position = end;
                }
                Ok(())
            })();
            abort.store(true, Ordering::Relaxed);
            if let Some(prefetch) = prefetched {
                prefetch.join(&mut readers);
            }
            applied
        })?;
        for reader in readers.into_iter().flatten() {
            reader.finish()?;
        }
        if !reuse.is_empty()
            && let Some(previous) = predecessor
        {
            let reader = previous.base().reader();
            let remap: Vec<_> = reader
                .all_paths()
                .iter()
                .map(|path| reuse.get(path.as_str()))
                .collect();
            for index in 0..reader.num_trigrams() {
                if index.is_multiple_of(4096) {
                    output.check()?;
                }
                stats.predecessor_posting_lists_read += 1;
                let (trigram, postings) = reader.trigram_posting_at(index);
                for posting in postings {
                    if let Some(ids) = remap[posting.file_id as usize] {
                        for &file_id in ids {
                            sorter.push(TrigramPosting {
                                trigram,
                                entry: PostingEntry { file_id, ..posting },
                            })?;
                            stats.postings_reused += 1;
                        }
                    }
                }
            }
        }
        let (trigram_count, _) = sorter.write_postings_to(output)?;
        builder::write_files_and_meta_to(
            output,
            repository.common_dir(),
            paths.len(),
            paths.iter().map(String::as_str),
            trigram_count,
            Some(true),
            true,
        )?;
        let base_id = output.open_base()?.snapshot_id();
        let manifest = Manifest {
            key: key.clone(),
            commit: commit.into(),
            base_id,
            entries,
        };
        let manifest = match output.control() {
            Some(_) => SealedManifest::seal_controlled(manifest, output.control())?,
            None => SealedManifest::seal(manifest)?,
        };
        let name = if output.layout() == crate::ondisk::IndexLayout::Managed {
            "generation.tgm"
        } else {
            MANIFEST
        };
        output.write_json(name, &manifest)?;
        output.check()?;
        Ok(stats)
    }

    fn publish(
        &self,
        stage: tempfile::TempDir,
        key: &GenerationKey,
        permit: Option<&Arc<crate::managed::WorkPermit>>,
    ) -> Result<()> {
        // Validate the entire final representation before its name is discoverable.
        drop(self.load(stage.path(), key, permit)?);
        for name in INDEX_FILES.into_iter().chain([MANIFEST]) {
            if let Some(permit) = permit {
                permit.check()?;
            }
            OpenOptions::new()
                .write(true)
                .open(stage.path().join(name))?
                .sync_all()?;
        }
        if let Some(permit) = permit {
            permit.check()?;
        }
        fs::rename(stage.path(), self.directory.join(key.storage_name()))?;
        // Rename is atomic visibility, not a parent-directory power-loss guarantee.
        // Ownership moved to the published name; do not clean up the old path.
        let _ = stage.keep();
        Ok(())
    }
}

fn add_path(paths: &mut Vec<String>, path: &str) -> Result<u32> {
    if paths.len() >= crate::live::OVERLAY_BIT as usize {
        return Err(GenerationError::Unsupported("too many base files".into()));
    }
    let id = paths.len() as u32;
    paths.push(path.into());
    Ok(id)
}

const MIN_BLOBS_PER_READER: usize = 16;
const PRIVATE_BYTES_PER_READER: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
struct ExtractionTuning {
    workers: usize,
    /// Bounds the blob reservations of one batch; at most two batches of
    /// extracted trigrams are retained at once.
    batch_bytes: u64,
}

impl ExtractionTuning {
    fn new(permit: Option<&Arc<crate::managed::WorkPermit>>, blobs: usize) -> Self {
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(64)
            .min(blobs.div_ceil(MIN_BLOBS_PER_READER));
        let Some(permit) = permit else {
            return Self {
                workers,
                batch_bytes: builder::INDEX_BUILD_BATCH_BYTES / 2,
            };
        };
        let private = permit.private_limit();
        Self {
            workers: workers
                .min(usize::try_from(private / PRIVATE_BYTES_PER_READER).unwrap_or(usize::MAX)),
            batch_bytes: (private / 8).min(builder::INDEX_BUILD_BATCH_BYTES / 2),
        }
    }
}

struct BlobExtraction {
    indexed: Option<(ContentId, trigram::TrigramMaskMap)>,
    _memory: Option<crate::managed::work::MemoryCharge>,
}

type Extractions = Vec<Option<Result<BlobExtraction>>>;

struct Prefetch<'scope, 'permit> {
    end: usize,
    worker: std::thread::ScopedJoinHandle<'scope, (Vec<Option<git::Blobs>>, Extractions)>,
    _parallelism: Option<crate::managed::work::Parallelism<'permit>>,
}

impl Prefetch<'_, '_> {
    fn join(self, readers: &mut Vec<Option<git::Blobs>>) -> (usize, Extractions) {
        let (owned, results) = self
            .worker
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        readers.extend(owned);
        (self.end, results)
    }
}

fn parallelism(
    permit: Option<&Arc<crate::managed::WorkPermit>>,
    wanted: usize,
) -> Result<(Option<crate::managed::work::Parallelism<'_>>, usize)> {
    match permit {
        Some(permit) if wanted > 1 => {
            let guard = permit.parallelism(wanted)?;
            let workers = guard.workers;
            Ok((Some(guard), workers))
        }
        _ => Ok((None, wanted)),
    }
}

#[derive(Clone, Copy)]
struct BlobSource<'a> {
    repository: &'a Repository,
    control: Option<&'a crate::managed::process::Control>,
    permit: Option<&'a Arc<crate::managed::WorkPermit>>,
}

impl BlobSource<'_> {
    fn extract(
        self,
        reader: &mut Option<git::Blobs>,
        oid: &str,
        size: u64,
        admitted: bool,
    ) -> Result<BlobExtraction> {
        if reader.is_none() {
            *reader = Some(match self.control {
                Some(control) => git::Blobs::controlled(self.repository, control.clone())?,
                None => git::Blobs::new(self.repository)?,
            });
        }
        let blobs = reader.as_mut().expect("spawned blob reader");
        let mut admission = None;
        let mut blob_memory = None;
        if let Some(permit) = self.permit {
            if size > permit.read_limits().blob_bytes {
                return Err(crate::managed::Error::pressure("blob-work-limit").into());
            }
            let reservation = size
                .checked_mul(6)
                .and_then(|bytes| bytes.checked_add(64 * 1024))
                .ok_or_else(|| crate::managed::Error::pressure("blob-memory-overflow"))?;
            if admitted {
                admission = Some(permit.admit_read(reservation)?);
            } else {
                blob_memory = Some(permit.memory(reservation)?);
            }
        }
        let bytes = blobs.read(oid, size)?;
        let text = encoding::decode_for_index_controlled(&bytes, self.permit)?;
        if trigram::is_binary(&text) {
            return Ok(BlobExtraction {
                indexed: None,
                _memory: None,
            });
        }
        let content_id = ContentId::from_indexed_bytes_controlled(&text, self.permit)?;
        let mut memory = self.permit.map(|permit| permit.memory(0)).transpose()?;
        let masks = match (self.permit, &mut memory) {
            (Some(permit), Some(memory)) => {
                trigram::extract_merged_masks_controlled(&text, permit, memory)?
            }
            _ => trigram::extract_merged_masks(&text),
        };
        drop((admission, blob_memory));
        Ok(BlobExtraction {
            indexed: Some((content_id, masks)),
            _memory: memory,
        })
    }

    /// Workers claim blobs in order and stop claiming after any failure, so
    /// every blob before the first failure has a result.
    fn extract_parallel(
        self,
        readers: &mut [Option<git::Blobs>],
        work: &[(&str, u64)],
        abort: &AtomicBool,
    ) -> Extractions {
        let next = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let mut results: Vec<_> = std::iter::repeat_with(|| None).take(work.len()).collect();
        std::thread::scope(|scope| {
            let workers: Vec<_> = readers
                .iter_mut()
                .map(|reader| {
                    let (next, stop) = (&next, &stop);
                    scope.spawn(move || {
                        let mut done = Vec::new();
                        while !stop.load(Ordering::Relaxed) && !abort.load(Ordering::Relaxed) {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some(&(oid, size)) = work.get(index) else {
                                break;
                            };
                            let result = self.extract(reader, oid, size, true);
                            if result.is_err() {
                                stop.store(true, Ordering::Relaxed);
                                *reader = None;
                            }
                            done.push((index, result));
                        }
                        done
                    })
                })
                .collect();
            for worker in workers {
                match worker.join() {
                    Ok(done) => {
                        for (index, result) in done {
                            results[index] = Some(result);
                        }
                    }
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            }
        });
        results
    }
}

fn is_private_pressure(error: &GenerationError) -> bool {
    matches!(error, GenerationError::Index(crate::Error::Managed(error))
        if error.category == crate::managed::ErrorCategory::ResourcePressure
            && error.reason_code == "reserved-private-memory-exhausted")
}

fn read_manifest(directory: &Path) -> Result<Manifest> {
    read_manifest_controlled(directory, u64::MAX, None)
}

fn read_manifest_controlled(
    directory: &Path,
    limit: u64,
    permit: Option<&Arc<crate::managed::WorkPermit>>,
) -> Result<Manifest> {
    check_plain(&directory.join(MANIFEST), false)?;
    let file = match permit {
        Some(_) => {
            crate::managed::storage::Directory::open(directory)?.open_file(MANIFEST, false)?
        }
        None => File::open(directory.join(MANIFEST))?,
    };
    let sealed: SealedManifest = crate::managed::inputs::read_json(file, limit, permit)?;
    if crate::output::checksum(&sealed.manifest, permit)? != sealed.checksum {
        return Err(GenerationError::InvalidMetadata(
            "manifest checksum mismatch".into(),
        ));
    }
    Ok(sealed.manifest)
}

fn create_plain_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    check_plain(path, true)
}

fn check_plain(path: &Path, directory: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let link = metadata.file_type().is_symlink();
    #[cfg(windows)]
    let link = {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        link || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    };
    if link || (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(GenerationError::Unsupported(format!(
            "expected a plain {}: {}",
            if directory { "directory" } else { "file" },
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn validate_tracked_path(path: &str) -> Result<()> {
    crate::shared::validate_path(path).map_err(|_| {
        GenerationError::Unsupported(format!("unrepresentable tracked path: {path:?}"))
    })?;
    if path.len() > u16::MAX as usize
        || path
            .split('/')
            .any(|component| component.eq_ignore_ascii_case(".git"))
    {
        return Err(GenerationError::Unsupported(format!(
            "unsafe or oversized tracked path: {path:?}"
        )));
    }
    #[cfg(windows)]
    for component in path.split('/') {
        let stem = component
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if component.contains(['<', '>', ':', '"', '|', '?', '*'])
            || component.ends_with(['.', ' '])
            || component.chars().any(|ch| ch.is_control())
            || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit())
        {
            return Err(GenerationError::Unsupported(format!(
                "tracked path is not representable on Windows: {path:?}"
            )));
        }
    }
    Ok(())
}

fn validate_entries(manifest: &Manifest, base: &SharedBase, oid_length: usize) -> Result<()> {
    validate_entries_controlled(manifest, base, oid_length, None)
}

fn validate_entries_controlled(
    manifest: &Manifest,
    base: &SharedBase,
    oid_length: usize,
    permit: Option<&Arc<crate::managed::WorkPermit>>,
) -> Result<()> {
    let mut indexed = HashSet::new();
    let mut previous: Option<&str> = None;
    for entry in &manifest.entries {
        if let Some(permit) = permit {
            permit.check()?;
        }
        validate_tracked_path(&entry.path)?;
        if previous.is_some_and(|path| path >= entry.path.as_str())
            || !git::valid_oid(&entry.oid, oid_length)
        {
            return Err(GenerationError::InvalidMetadata(
                "unsorted/duplicate entries or invalid blob OID".into(),
            ));
        }
        previous = Some(&entry.path);
        let regular = entry.mode.is_regular();
        if (entry.mode == EntryMode::Gitlink) != entry.size.is_none() {
            return Err(GenerationError::InvalidMetadata(
                "mode/size mismatch".into(),
            ));
        }
        let too_large = entry
            .size
            .zip(manifest.key.profile.max_blob_bytes)
            .is_some_and(|(size, limit)| size > limit);
        let valid = match &entry.content {
            EntryContent::NotRegular => !regular,
            EntryContent::TooLarge => regular && too_large,
            EntryContent::Binary => regular && !too_large,
            EntryContent::Indexed { .. } => {
                indexed.insert(entry.path.as_str());
                regular && !too_large
            }
        };
        if !valid {
            return Err(GenerationError::InvalidMetadata(
                "mode/profile/content mismatch".into(),
            ));
        }
    }
    if indexed.len() != base.reader().num_files() {
        return Err(GenerationError::InvalidMetadata(
            "tracked content/index membership mismatch".into(),
        ));
    }
    for path in base.reader().all_paths() {
        if let Some(permit) = permit {
            permit.check()?;
        }
        if !indexed.contains(path.as_str()) {
            return Err(GenerationError::InvalidMetadata(
                "tracked content/index membership mismatch".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::IndexMeta;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn publication_lock_is_exclusive_and_released_when_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let repository = Repository {
            common_dir: directory.path().to_path_buf(),
            git_dir: directory.path().to_path_buf(),
            identity: "test".into(),
            oid_length: 40,
        };
        let manager = GenerationManager {
            repository,
            directory: directory.path().to_path_buf(),
        };
        let held = manager.lock().unwrap();
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(manager.directory().join("publication.lock"))
            .unwrap();
        let error = fs2::FileExt::try_lock_exclusive(&contender).unwrap_err();
        assert_eq!(
            error.raw_os_error(),
            fs2::lock_contended_error().raw_os_error()
        );
        drop(held);
        fs2::FileExt::try_lock_exclusive(&contender).unwrap();
        drop(contender);
        manager.lock().unwrap();
    }

    #[test]
    fn default_content_eligibility_keeps_the_legacy_profile_wire_shape() {
        let legacy = serde_json::json!({
            "content": "raw-git-blob-auto-v1",
            "coverage": "tracked-regular-files-v1",
            "max_blob_bytes": 67108864,
        });
        assert_eq!(
            serde_json::to_value(IndexingProfile::default()).unwrap(),
            legacy
        );
        assert_eq!(
            serde_json::from_value::<IndexingProfile>(legacy.clone()).unwrap(),
            IndexingProfile::default()
        );
        let content_only = IndexingProfile {
            eligibility: ContentEligibility::ContentOnly,
            ..IndexingProfile::default()
        };
        let mut wide = legacy;
        wide["eligibility"] = "content-only".into();
        assert_eq!(serde_json::to_value(content_only).unwrap(), wide);
        assert_eq!(
            serde_json::from_value::<IndexingProfile>(wide.clone()).unwrap(),
            content_only
        );
        wide["eligibility"] = "extensions".into();
        assert!(serde_json::from_value::<IndexingProfile>(wide).is_err());

        let key = |profile| GenerationKey {
            repository: "test".into(),
            tree: "0".repeat(40),
            profile,
            index_format: INDEX_FORMAT_VERSION,
            schema: SCHEMA_VERSION,
        };
        let default_key = key(IndexingProfile::default());
        assert!(
            !serde_json::to_string(&default_key)
                .unwrap()
                .contains("eligibility")
        );
        assert_ne!(
            default_key.storage_name(),
            key(content_only).storage_name(),
            "eligibility is part of generation identity"
        );
    }

    fn empty_generation(directory: &Path) -> Generation {
        builder::write_index_from_snapshot(directory, directory, &[], &HashMap::new(), true)
            .unwrap();
        let mut meta = IndexMeta::load(directory).unwrap();
        meta.hidden_complete = true;
        meta.save(directory).unwrap();
        let base = Arc::new(SharedBase::open(directory).unwrap());
        Generation {
            directory: directory.to_path_buf(),
            manifest: Manifest {
                key: GenerationKey {
                    repository: "test".into(),
                    tree: "0".repeat(40),
                    profile: IndexingProfile::default(),
                    index_format: INDEX_FORMAT_VERSION,
                    schema: SCHEMA_VERSION,
                },
                commit: "1".repeat(40),
                base_id: base.snapshot_id(),
                entries: vec![],
            },
            base,
        }
    }

    #[test]
    fn cold_load_does_not_block_unrelated_warm_cache_hit() {
        let cold_directory = tempfile::tempdir().unwrap();
        let warm_directory = tempfile::tempdir().unwrap();
        let cold_generation = empty_generation(cold_directory.path());
        let warm = cached_generation(warm_directory.path().to_path_buf(), None, || {
            Ok(empty_generation(warm_directory.path()))
        })
        .unwrap();
        let (loading, started) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let (hit, observed) = mpsc::channel();
        let cold_path = cold_directory.path().to_path_buf();
        let cold = thread::spawn(move || {
            cached_generation(cold_path, None, || {
                loading.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(15)).unwrap();
                Ok(cold_generation)
            })
            .unwrap()
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let warm_path = warm_directory.path().to_path_buf();
        let lookup = thread::spawn(move || {
            let result =
                cached_generation(warm_path, None, || panic!("warm entry must not reload"))
                    .unwrap();
            hit.send(()).unwrap();
            result
        });
        let hit_before_release = observed.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        cold.join().unwrap();
        let same = lookup.join().unwrap();
        assert!(
            hit_before_release.is_ok(),
            "cold load held the global cache mutex"
        );
        assert!(Arc::ptr_eq(&warm, &same));
    }

    #[test]
    fn concurrent_cold_loads_recheck_before_cache_insertion() {
        let directory = tempfile::tempdir().unwrap();
        let first = empty_generation(directory.path());
        let second = Generation {
            directory: first.directory.clone(),
            manifest: serde_json::from_slice(&serde_json::to_vec(&first.manifest).unwrap())
                .unwrap(),
            base: Arc::clone(&first.base),
        };
        let (loading, started) = mpsc::channel();
        let mut releases = Vec::new();
        let threads: Vec<_> = [first, second]
            .into_iter()
            .map(|generation| {
                let loading = loading.clone();
                let (release, resume) = mpsc::channel();
                releases.push(release);
                thread::spawn(move || {
                    cached_generation(generation.directory.clone(), None, || {
                        loading.send(()).unwrap();
                        resume.recv_timeout(Duration::from_secs(15)).unwrap();
                        Ok(generation)
                    })
                    .unwrap()
                })
            })
            .collect();
        let both_loading = started
            .recv_timeout(Duration::from_secs(5))
            .and_then(|_| started.recv_timeout(Duration::from_secs(5)));
        for release in releases {
            let _ = release.send(());
        }
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert!(
            both_loading.is_ok(),
            "cold loads were serialized by the global cache"
        );
        assert!(Arc::ptr_eq(&results[0], &results[1]));
    }

    #[test]
    fn invalid_staged_generation_is_never_published() {
        let root = tempfile::tempdir().unwrap();
        let repository = Repository {
            common_dir: root.path().to_path_buf(),
            git_dir: root.path().to_path_buf(),
            identity: "test".into(),
            oid_length: 40,
        };
        let manager = GenerationManager {
            repository,
            directory: root.path().to_path_buf(),
        };
        let key = GenerationKey {
            repository: "test".into(),
            tree: "0".repeat(40),
            profile: IndexingProfile::default(),
            index_format: INDEX_FORMAT_VERSION,
            schema: SCHEMA_VERSION,
        };
        let stage = tempfile::Builder::new()
            .prefix(".stage-")
            .tempdir_in(root.path())
            .unwrap();
        builder::write_index_from_snapshot(root.path(), stage.path(), &[], &HashMap::new(), true)
            .unwrap();
        let mut meta = IndexMeta::load(stage.path()).unwrap();
        meta.hidden_complete = true;
        meta.save(stage.path()).unwrap();
        let base_id = SharedBase::open(stage.path()).unwrap().snapshot_id();
        // A self-consistent checksum is not sufficient: membership must match.
        let sealed = SealedManifest::seal(Manifest {
            key: key.clone(),
            commit: "1".repeat(40),
            base_id,
            entries: vec![TrackedEntry {
                path: "absent.txt".into(),
                oid: "2".repeat(40),
                size: Some(0),
                mode: EntryMode::File,
                content: EntryContent::Indexed {
                    content_id: ContentId::from_indexed_bytes(b""),
                },
            }],
        })
        .unwrap();
        fs::write(
            stage.path().join(MANIFEST),
            serde_json::to_vec(&sealed).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            manager.publish(stage, &key, None),
            Err(GenerationError::InvalidMetadata(_))
        ));
        assert!(!manager.directory.join(key.storage_name()).exists());
        assert!(manager.list().unwrap().is_empty());
    }

    const SERIAL: ExtractionTuning = ExtractionTuning {
        workers: 1,
        batch_bytes: u64::MAX,
    };
    // Small batches keep a prefetch in flight across many batch boundaries.
    const PARALLEL: [ExtractionTuning; 2] = [
        ExtractionTuning {
            workers: 4,
            batch_bytes: 256 * 1024,
        },
        ExtractionTuning {
            workers: 8,
            batch_bytes: u64::MAX,
        },
    ];

    struct BuildFixture {
        _root: tempfile::TempDir,
        repository: Repository,
        commit: String,
        tree: String,
    }

    type BuildOutput = (BuildStats, Vec<(String, Vec<u8>)>);

    fn git(root: &Path, args: &[&str]) {
        let mut command = std::process::Command::new("git");
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
            .current_dir(root)
            .args(["-c", "user.name=Generation Tests"])
            .args(["-c", "user.email=generations@example.invalid"])
            .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn build_fixture(files: &[(String, Vec<u8>)]) -> BuildFixture {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "--quiet", "--initial-branch=main"]);
        for (path, bytes) in files {
            let path = root.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        git(root.path(), &["add", "--all"]);
        git(
            root.path(),
            &["commit", "--quiet", "--allow-empty", "-m", "fixture"],
        );
        let repository = Repository::discover(root.path()).unwrap();
        let (commit, tree) = repository.resolve_commit_tree("HEAD").unwrap();
        BuildFixture {
            _root: root,
            repository,
            commit,
            tree,
        }
    }

    fn noisy_text(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (0..len)
            .map(|index| {
                if index % 64 == 63 {
                    return b'\n';
                }
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                b'!' + (state % 94) as u8
            })
            .collect()
    }

    fn mixed_corpus() -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        for index in 0..160_usize {
            let mut text = format!("fn item_{index}() -> usize {{ {index} }}\n");
            let lines = if index % 40 == 0 { 2500 } else { index % 9 };
            for line in 0..lines {
                text.push_str(&format!("let value_{index}_{line} = {};\n", line * index));
            }
            files.push((
                format!("src/{:02}/item-{index:03}.rs", index % 7),
                text.into_bytes(),
            ));
        }
        let shared = b"contents shared by many paths\n".repeat(40);
        for index in 0..24 {
            files.push((format!("copies/copy-{index:02}.txt"), shared.clone()));
        }
        for index in 0..6_u8 {
            files.push((
                format!("bin/blob-{index}.bin"),
                [index, 0, 1, 2, 0, 0xff, b'x'].repeat(200),
            ));
        }
        let wide = "utf sixteen needle text\n".repeat(20);
        let mut le = vec![0xff, 0xfe];
        le.extend(wide.encode_utf16().flat_map(u16::to_le_bytes));
        let mut be = vec![0xfe, 0xff];
        be.extend(wide.encode_utf16().flat_map(u16::to_be_bytes));
        files.push(("text/utf16le.txt".into(), le));
        files.push(("text/utf16be.txt".into(), be));
        files.push((
            "text/latin1.txt".into(),
            b"caf\xe9 cr\xe8me br\xfbl\xe9e\n".repeat(30),
        ));
        files.push((
            "text/invalid.txt".into(),
            b"valid prefix \xc3\x28 then \xa0\xa1 more\n".repeat(30),
        ));
        files.push(("text/empty.txt".into(), Vec::new()));
        files.push((
            "large/oversized.txt".into(),
            b"oversized line of text\n".repeat(20_000),
        ));
        files
    }

    fn capped_profile() -> IndexingProfile {
        IndexingProfile {
            max_blob_bytes: Some(256 * 1024),
            ..IndexingProfile::default()
        }
    }

    fn build_output(
        fixture: &BuildFixture,
        profile: IndexingProfile,
        permit: Option<&Arc<crate::managed::WorkPermit>>,
        tuning: Option<ExtractionTuning>,
    ) -> Result<BuildOutput> {
        let stage = tempfile::tempdir().unwrap();
        let output = match permit {
            Some(permit) => crate::output::Output::compatibility(stage.path(), permit),
            None => crate::output::Output::legacy(stage.path()),
        };
        let key = GenerationKey::legacy(&fixture.repository, fixture.tree.clone(), profile);
        let stats = GenerationManager::build_into_tuned(
            &fixture.repository,
            &key,
            &fixture.commit,
            None,
            &output,
            tuning,
        )?;
        drop(output);
        let mut files = Vec::new();
        for entry in fs::read_dir(stage.path()).unwrap() {
            let entry = entry.unwrap();
            assert!(entry.file_type().unwrap().is_file());
            let name = entry.file_name().into_string().unwrap();
            let mut bytes = fs::read(entry.path()).unwrap();
            if name == "meta.json" {
                let mut meta: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                for timestamp in ["created_at", "updated_at"] {
                    meta.as_object_mut().unwrap().remove(timestamp).unwrap();
                }
                bytes = serde_json::to_vec(&meta).unwrap();
            }
            files.push((name, bytes));
        }
        files.sort();
        Ok((stats, files))
    }

    fn managed_permit(
        fixture: &BuildFixture,
        policy: crate::managed::Policy,
        private_bytes: u64,
    ) -> (
        tempfile::TempDir,
        Arc<crate::managed::Namespace>,
        Arc<crate::managed::WorkPermit>,
    ) {
        let storage = tempfile::tempdir().unwrap();
        let namespace =
            crate::managed::Namespace::initialize(&fixture.repository, storage.path(), policy)
                .unwrap();
        namespace.activate().unwrap();
        let operation = namespace
            .accept_system_operation(
                crate::managed::Token::parse("generation-build").unwrap(),
                "build",
                serde_json::json!({}),
            )
            .unwrap();
        let permit = namespace
            .reserve(
                &operation.id,
                None,
                crate::managed::WorkRequest {
                    allocation_version: 1,
                    private_bytes,
                    staging_bytes: 32 * 1024 * 1024,
                    slots: 1,
                },
            )
            .unwrap();
        (storage, namespace, permit)
    }

    fn managed_output(
        fixture: &BuildFixture,
        policy: crate::managed::Policy,
        private_bytes: u64,
        profile: IndexingProfile,
        tuning: Option<ExtractionTuning>,
    ) -> Result<BuildOutput> {
        let (_storage, _namespace, permit) = managed_permit(fixture, policy, private_bytes);
        build_output(fixture, profile, Some(&permit), tuning)
    }

    fn pressure_reason(result: Result<BuildOutput>) -> String {
        match result {
            Err(GenerationError::Index(crate::Error::Managed(error)))
                if error.category == crate::managed::ErrorCategory::ResourcePressure =>
            {
                error.reason_code
            }
            other => panic!(
                "expected resource pressure, got {:?}",
                other.map(|(s, _)| s)
            ),
        }
    }

    #[test]
    fn parallel_blob_extraction_builds_the_serial_generation_bytes() {
        let fixture = build_fixture(&mixed_corpus());
        let (stats, files) = build_output(&fixture, capped_profile(), None, Some(SERIAL)).unwrap();
        assert_eq!(stats.tracked_entries, 196);
        assert_eq!(stats.blobs_read, 172);
        assert_eq!(stats.blobs_extracted, 166);
        let names: Vec<_> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&MANIFEST) && names.contains(&"meta.json"));
        let manifest: SealedManifest =
            serde_json::from_slice(&files.iter().find(|(name, _)| name == MANIFEST).unwrap().1)
                .unwrap();
        let content = |path: &str| {
            manifest
                .manifest
                .entries
                .iter()
                .find(|entry| entry.path == path)
                .unwrap()
                .content
                .clone()
        };
        assert!(matches!(content("bin/blob-3.bin"), EntryContent::Binary));
        assert!(matches!(
            content("large/oversized.txt"),
            EntryContent::TooLarge
        ));
        for path in [
            "text/utf16le.txt",
            "text/utf16be.txt",
            "text/latin1.txt",
            "text/invalid.txt",
            "text/empty.txt",
            "copies/copy-17.txt",
        ] {
            assert!(
                matches!(content(path), EntryContent::Indexed { .. }),
                "{path}"
            );
        }
        assert_eq!(content("text/utf16le.txt"), content("text/utf16be.txt"));
        for tuning in PARALLEL.map(Some).into_iter().chain([None]) {
            let parallel = build_output(&fixture, capped_profile(), None, tuning).unwrap();
            assert!(parallel == (stats.clone(), files.clone()), "{tuning:?}");
        }
    }

    #[test]
    fn managed_parallel_blob_extraction_builds_the_serial_generation_bytes() {
        let fixture = build_fixture(&mixed_corpus());
        let policy = crate::managed::policy::fixture_policy;
        let serial =
            managed_output(&fixture, policy(), 32 << 20, capped_profile(), Some(SERIAL)).unwrap();
        let legacy = build_output(&fixture, capped_profile(), None, Some(SERIAL)).unwrap();
        assert!(serial == legacy);
        for tuning in PARALLEL.map(Some).into_iter().chain([None]) {
            let parallel =
                managed_output(&fixture, policy(), 32 << 20, capped_profile(), tuning).unwrap();
            assert!(parallel == serial, "{tuning:?}");
        }
    }

    #[test]
    fn retained_parallel_extractions_fall_back_to_serial_under_private_pressure() {
        let files: Vec<_> = (0..24)
            .map(|index| {
                (
                    format!("noise/{index:02}.txt"),
                    noisy_text(index, 16 * 1024),
                )
            })
            .collect();
        let fixture = build_fixture(&files);
        let policy = crate::managed::policy::fixture_policy;
        let private = 16 << 20;
        let serial =
            managed_output(&fixture, policy(), private, capped_profile(), Some(SERIAL)).unwrap();
        assert_eq!(serial.0.blobs_extracted, 24);
        // One batch would retain every extracted table, far beyond the reservation.
        let tuning = ExtractionTuning {
            workers: 8,
            batch_bytes: u64::MAX,
        };
        let parallel =
            managed_output(&fixture, policy(), private, capped_profile(), Some(tuning)).unwrap();
        assert!(parallel == serial);
    }

    #[test]
    fn parallel_extraction_reports_the_serial_blob_work_limit() {
        let mut files = mixed_corpus();
        files.retain(|(path, _)| path != "large/oversized.txt");
        files.push((
            "large/over-work-limit.txt".into(),
            noisy_text(7, 1536 * 1024),
        ));
        let fixture = build_fixture(&files);
        let policy = crate::managed::policy::fixture_policy;
        let profile = IndexingProfile::default();
        for tuning in [SERIAL].into_iter().chain(PARALLEL) {
            let result = managed_output(&fixture, policy(), 32 << 20, profile, Some(tuning));
            assert_eq!(pressure_reason(result), "blob-work-limit", "{tuning:?}");
        }
    }

    #[test]
    fn parallel_extraction_builds_an_empty_tree() {
        let fixture = build_fixture(&[]);
        let serial = build_output(&fixture, capped_profile(), None, Some(SERIAL)).unwrap();
        assert_eq!(serial.0.tracked_entries, 0);
        for tuning in PARALLEL {
            let parallel = build_output(&fixture, capped_profile(), None, Some(tuning)).unwrap();
            assert!(parallel == serial);
        }
    }

    #[test]
    fn cancellation_stops_parallel_extraction() {
        let fixture = build_fixture(&mixed_corpus());
        let policy = crate::managed::policy::fixture_policy;
        for delay in [0, 2, 10, 40] {
            let (_storage, _namespace, permit) = managed_permit(&fixture, policy(), 32 << 20);
            let canceller = {
                let permit = permit.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(delay));
                    permit.cancel();
                })
            };
            let result = build_output(&fixture, capped_profile(), Some(&permit), Some(PARALLEL[0]));
            canceller.join().unwrap();
            if let Err(error) = result {
                assert!(permit.check().is_err(), "{error}");
            }
        }
    }
}
