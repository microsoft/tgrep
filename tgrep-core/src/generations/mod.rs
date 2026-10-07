//! Immutable, repository-scoped indexes of committed Git trees.
//!
//! This is not worktree synchronization. A generation indexes a tracked-path
//! superset using **raw Git blobs**, not checkout-filtered bytes. An agent runtime
//! must reconcile content and membership before searching a worktree view.
//! Published generations are retained indefinitely; there is no online GC.

mod git;

pub(crate) fn worktree_root(root: &Path) -> Result<PathBuf> {
    git::worktree_root(root)
}

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde::{Deserialize, Serialize};

use crate::builder;
use crate::external::{ExternalSorter, TrigramPosting};
use crate::meta::{ContentId, INDEX_FORMAT_VERSION, IndexMeta};
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
        let root = fs::canonicalize(root)?;
        let common_dir = git::common_dir(&root)?;
        let git_dir = git::worktree_git_dir(&root)?;
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
        let mut repository = Self {
            common_dir,
            git_dir,
            identity: hash.finalize().to_hex().to_string(),
            oid_length: 0,
        };
        repository.oid_length = git::object_format(&repository)?;
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

    /// Resolve a revision to its exact (commit, tree) IDs without building or
    /// publishing a generation. Symbolic revisions use this worktree's HEAD.
    pub fn resolve_commit_tree(&self, revision: &str) -> Result<(String, String)> {
        git::commit_tree(self, revision)
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

/// Exact, serializable compatibility profile. Budgets are not part of identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexingProfile {
    pub content: ContentSemantics,
    pub coverage: Coverage,
    /// Limit on raw blob size; `None` removes the cap (one blob is held in memory).
    pub max_blob_bytes: Option<u64>,
}

impl Default for IndexingProfile {
    fn default() -> Self {
        Self {
            content: ContentSemantics::RawGitBlobAutoV1,
            coverage: Coverage::TrackedRegularFilesV1,
            max_blob_bytes: Some(64 * 1024 * 1024),
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
        let checksum = *blake3::hash(&serde_json::to_vec(&manifest)?).as_bytes();
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
        RetentionPolicy::RetainAll
    }
}

pub struct EnsureResult {
    pub generation: Arc<Generation>,
    pub stats: BuildStats,
    /// The requested commit, including on reuse of a tree from another commit.
    pub requested_commit: String,
}

type Cache = HashMap<PathBuf, Weak<Generation>>;
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

fn cached_generation(
    directory: PathBuf,
    load: impl FnOnce() -> Result<Generation>,
) -> Result<Arc<Generation>> {
    let cache = CACHE.get_or_init(Default::default);
    {
        let cache = cache.lock().map_err(|_| GenerationError::Synchronization)?;
        if let Some(generation) = cache.get(&directory).and_then(Weak::upgrade) {
            return Ok(generation);
        }
    }
    let generation = Arc::new(load()?);
    let mut cache = cache.lock().map_err(|_| GenerationError::Synchronization)?;
    if let Some(existing) = cache.get(&directory).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    cache.retain(|_, value| value.strong_count() > 0);
    cache.insert(directory, Arc::downgrade(&generation));
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
        let (commit, tree) = git::commit_tree(&self.repository, revision)?;
        let key = GenerationKey {
            repository: self.repository.identity.clone(),
            tree,
            profile,
            index_format: INDEX_FORMAT_VERSION,
            schema: SCHEMA_VERSION,
        };
        if let Some(previous) = predecessor {
            let mut expected = key.clone();
            expected.tree = previous.key().tree.clone();
            if previous.key() != &expected {
                return Err(GenerationError::Incompatible(
                    "predecessor repository/profile/format differs".into(),
                ));
            }
        }
        let _lock = self.lock()?;
        let directory = self.directory.join(key.storage_name());
        if directory.try_exists()? {
            let generation = self.open_locked(&key)?;
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
        let mut stats = self.build(&key, &commit, predecessor)?;
        let generation = self.open_locked(&key)?;
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
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(file)
    }

    fn open_locked(&self, key: &GenerationKey) -> Result<Arc<Generation>> {
        self.validate_key(key)?;
        let directory = self.directory.join(key.storage_name());
        cached_generation(directory.clone(), || self.load(&directory, key))
    }

    fn load(&self, directory: &Path, key: &GenerationKey) -> Result<Generation> {
        check_plain(directory, true)?;
        for name in INDEX_FILES {
            check_plain(&directory.join(name), false)?;
        }
        let manifest = read_manifest(directory)?;
        if &manifest.key != key || !git::valid_oid(&manifest.commit, self.repository.oid_length) {
            return Err(GenerationError::InvalidMetadata(
                "manifest key/commit mismatch".into(),
            ));
        }
        let base = Arc::new(SharedBase::open(directory)?);
        if base.snapshot_id() != manifest.base_id {
            return Err(GenerationError::InvalidMetadata(
                "manifest/index fingerprint mismatch".into(),
            ));
        }
        validate_entries(&manifest, &base, self.repository.oid_length)?;
        Ok(Generation {
            directory: directory.to_path_buf(),
            manifest,
            base,
        })
    }

    fn build(
        &self,
        key: &GenerationKey,
        commit: &str,
        predecessor: Option<&Arc<Generation>>,
    ) -> Result<BuildStats> {
        let mut entries = git::entries(&self.repository, &key.tree)?;
        let mut stats = BuildStats {
            tracked_entries: entries.len(),
            ..BuildStats::default()
        };
        let stage = tempfile::Builder::new()
            .prefix(".stage-")
            .tempdir_in(&self.directory)?;
        let mut sorter = ExternalSorter::new(stage.path(), builder::DEFAULT_INDEX_BUFFER_BYTES);
        let mut paths = Vec::new();
        let mut previous_blobs = HashMap::new();
        if let Some(previous) = predecessor {
            for entry in previous
                .entries()
                .iter()
                .filter(|entry| entry.mode.is_regular())
            {
                previous_blobs.entry(entry.oid.as_str()).or_insert(entry);
            }
        }
        let mut reuse: HashMap<&str, Vec<u32>> = HashMap::new();
        let mut blobs = None;
        // Group by blob so repeated files also cause only one extraction.
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, entry) in entries.iter_mut().enumerate() {
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
        // Stable file IDs make independent builds deterministic.
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (oid, indices) in groups {
            let previous = previous_blobs.get(oid.as_str()).copied();
            if let (Some(previous), Some(generation)) = (previous, predecessor) {
                match previous.content {
                    EntryContent::Indexed { .. } | EntryContent::Binary => {
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
                        for index in indices {
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
                    _ => {}
                }
            }
            let batch = match &mut blobs {
                Some(batch) => batch,
                None => blobs.insert(git::Blobs::new(&self.repository)?),
            };
            let size = entries[indices[0]].size.expect("regular blob size");
            let bytes = batch.read(&oid, size)?;
            stats.blobs_read += 1;
            stats.blob_bytes_read += size;
            let text = encoding::decode_for_index(&bytes);
            if trigram::is_binary(&text) {
                for index in indices {
                    entries[index].content = EntryContent::Binary;
                }
                continue;
            }
            let content_id = ContentId::from_indexed_bytes(&text);
            let masks = trigram::extract_merged_masks(&text);
            stats.blobs_extracted += 1;
            for index in indices {
                let entry = &mut entries[index];
                entry.content = EntryContent::Indexed { content_id };
                let file_id = add_path(&mut paths, &entry.path)?;
                sorter.push_file(
                    file_id,
                    masks.iter().map(|(&trigram, &masks)| (trigram, masks)),
                )?;
            }
        }
        if let Some(blobs) = blobs {
            blobs.finish()?;
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
        let (trigram_count, _) = sorter.write_postings(stage.path())?;
        builder::write_files_and_meta(
            stage.path(),
            self.repository.common_dir(),
            paths.len(),
            paths.iter().map(String::as_str),
            trigram_count,
            Some(true),
        )?;
        let mut meta = IndexMeta::load(stage.path())?;
        meta.hidden_complete = true;
        meta.save(stage.path())?;
        let base_id = SharedBase::open(stage.path())?.snapshot_id();
        let manifest = SealedManifest::seal(Manifest {
            key: key.clone(),
            commit: commit.into(),
            base_id,
            entries,
        })?;
        let mut writer = BufWriter::new(File::create(stage.path().join(MANIFEST))?);
        serde_json::to_writer(&mut writer, &manifest)?;
        writer.flush()?;
        drop(writer);
        self.publish(stage, key)?;
        Ok(stats)
    }

    fn publish(&self, stage: tempfile::TempDir, key: &GenerationKey) -> Result<()> {
        // Validate the entire final representation before its name is discoverable.
        drop(self.load(stage.path(), key)?);
        for name in INDEX_FILES.into_iter().chain([MANIFEST]) {
            OpenOptions::new()
                .write(true)
                .open(stage.path().join(name))?
                .sync_all()?;
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

fn read_manifest(directory: &Path) -> Result<Manifest> {
    check_plain(&directory.join(MANIFEST), false)?;
    let sealed: SealedManifest =
        serde_json::from_reader(BufReader::new(File::open(directory.join(MANIFEST))?))?;
    if blake3::hash(&serde_json::to_vec(&sealed.manifest)?).as_bytes() != &sealed.checksum {
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

fn validate_tracked_path(path: &str) -> Result<()> {
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
    let mut indexed = HashSet::new();
    let mut previous: Option<&str> = None;
    for entry in &manifest.entries {
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
    if indexed.len() != base.reader().num_files()
        || base
            .reader()
            .all_paths()
            .iter()
            .any(|path| !indexed.contains(path.as_str()))
    {
        return Err(GenerationError::InvalidMetadata(
            "tracked content/index membership mismatch".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let warm = cached_generation(warm_directory.path().to_path_buf(), || {
            Ok(empty_generation(warm_directory.path()))
        })
        .unwrap();
        let (loading, started) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let (hit, observed) = mpsc::channel();
        let cold_path = cold_directory.path().to_path_buf();
        let cold = thread::spawn(move || {
            cached_generation(cold_path, || {
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
                cached_generation(warm_path, || panic!("warm entry must not reload")).unwrap();
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
                    cached_generation(generation.directory.clone(), || {
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
            manager.publish(stage, &key),
            Err(GenerationError::InvalidMetadata(_))
        ));
        assert!(!manager.directory.join(key.storage_name()).exists());
        assert!(manager.list().unwrap().is_empty());
    }
}
