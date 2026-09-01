/// `tgrep serve` — TCP JSON-RPC server with file watcher.
///
/// Keeps the trigram index in memory (HybridIndex), watches for filesystem
/// changes, and serves search/status queries over TCP.
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use fs2::FileExt;
use lru::LruCache;

use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use tgrep_core::builder;
use tgrep_core::hybrid::HybridIndex;
use tgrep_core::query;

const CACHE_CAPACITY: usize = 50_000;
/// Total decoded bytes the content cache may hold. The entry-count limit above
/// says nothing about memory; without this a handful of large files can pin
/// tens of gigabytes for the life of the process.
const CACHE_MAX_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB
/// Largest single entry the cache will admit. A file bigger than this would
/// evict most of the cache to store itself, so it is read straight through
/// instead. It is still searched - only the caching is skipped.
const CACHE_MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024; // 64 MiB
/// Default mutation count that triggers a background save (override with
/// `--auto-save-mutations`).
const AUTO_SAVE_MUTATIONS: u32 = 5000;
const AUTO_SAVE_INTERVAL: Duration = Duration::from_secs(600); // 10 minutes

/// Default bound on the queue between the OS notification callback and the
/// watcher worker (override with `--watcher-queue-cap`).
///
/// Sized to absorb a bulk change — a branch switch or a build — without
/// dropping events, while staying small enough that a truly runaway producer
/// is capped rather than growing memory without limit. Each queued `Event`
/// holds only its paths, so this is on the order of a few MB at worst.
const WATCHER_QUEUE_CAP: usize = 16_384;

/// How long the watcher worker waits for an event before looking for an
/// overflow to repair. Doubles as the quiet period that must elapse before a
/// reconciling stale check runs.
const WATCHER_IDLE_POLL: Duration = Duration::from_secs(1);

/// Git's index is hidden from the repository watcher. Poll its metadata only
/// when the case-insensitive tracked-file exemption is active.
const TRACKED_INDEX_POLL: Duration = Duration::from_secs(2);

/// How long a file the watcher never heard about can stay wrong in the index.
///
/// Every mutation the index takes after the initial build arrives as an OS
/// notification, and a notification can go missing: the queue between the
/// callback and the worker can overflow, a network or virtualised filesystem
/// can decline to report a change at all, and a `serve` that is running while
/// the tree is replaced wholesale (a branch switch, a build) can be handed
/// more events than the platform is willing to buffer. Overflow is detected
/// and repaired immediately; the rest is silent. Nothing else in the server
/// ever revisits a file it believes it already knows, so a miss lasts until
/// the file happens to change again — which for a deleted file is never.
///
/// A full stale check finds all of it, because it compares the whole tree
/// against the index rather than trusting any event. Running one on a timer
/// turns "until something else happens to fix it" into a bound.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(3600);

/// How often the reconcile loop wakes to see whether it is due.
const RECONCILE_POLL: Duration = Duration::from_secs(60);

/// How long the server must have gone without a search before a scheduled
/// reconcile runs. A reconcile walks the entire tree and holds the snapshot
/// gate while it does, so on a repository being actively queried it waits for
/// a gap rather than competing.
const RECONCILE_QUIET_PERIOD: Duration = Duration::from_secs(120);

/// The point at which a reconcile stops waiting for a quiet gap. A server
/// queried steadily every minute would otherwise never see one, and would
/// never reconcile at all — which is the failure this exists to prevent.
const RECONCILE_DEADLINE: Duration = Duration::from_secs(4 * 3600);

/// Server discovery info, written to `serve.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub pid: u32,
    pub port: u16,
}

impl ServerInfo {
    pub fn save(&self, index_dir: &Path) -> Result<()> {
        let path = index_dir.join("serve.json");
        let json = serde_json::to_string(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load(index_dir: &Path) -> Result<Self> {
        let path = index_dir.join("serve.json");
        let data = std::fs::read_to_string(path)?;
        let info: Self = serde_json::from_str(&data)?;
        Ok(info)
    }

    pub fn cleanup(index_dir: &Path) {
        let _ = std::fs::remove_file(index_dir.join("serve.json"));
    }
}

/// Attempt to acquire an exclusive lock on `serve.lock` inside the index
/// directory. Returns the held `File` (must be kept alive for the duration of
/// the server) or an error with a user-friendly message when another server is
/// already running.
fn try_acquire_server_lock(index_dir: &Path) -> Result<File> {
    std::fs::create_dir_all(index_dir)?;
    let lock_path = index_dir.join("serve.lock");
    let file = File::create(&lock_path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(file),
        Err(_) => {
            // Another server holds the lock — provide a helpful message.
            let detail = if let Ok(info) = ServerInfo::load(index_dir) {
                format!(" (pid {}, port {})", info.pid, info.port,)
            } else {
                String::new()
            };
            anyhow::bail!(
                "another tgrep server is already running for index directory `{}`{}. \
                 Stop the existing server before starting a new one.",
                index_dir.display(),
                detail,
            );
        }
    }
}

/// Lock ordering (acquire in this order to avoid deadlocks):
///
///   1. `snapshot_gate` — coordinates watcher mutations with publish cycle
///   2. `gitignore`     — guards the watcher matcher; drop before file/index work
///   3. `publish_lock`  — serializes on-disk index publication
///   4. `index`         — guards the in-memory HybridIndex (read-heavy)
///   5. `filename_extra_paths` — guards paths omitted from the content index
///   6. `cache`         — guards the file content LRU cache
///   7. `file_stamps`   — guards per-file mtime/size stamps
///
/// `indexing` and `flushing` coordinate the handoff between bulk indexing and
/// final flush with the auto-save loop; use sequentially consistent accesses
/// for those flags so auto-save never observes both as false during handoff.
/// Searches only acquire `index` (read) and `cache` (read then write);
/// they never take `snapshot_gate` or `publish_lock`.
/// A file's searchable text together with the map back to its on-disk byte
/// offsets, so columns and `--byte-offset` mean the same thing over the server
/// as they do locally even when lossy decoding widened invalid bytes.
struct DecodedFile {
    text: String,
    fixups: tgrep_core::encoding::LossyFixups,
}

impl DecodedFile {
    fn new(bytes: Vec<u8>, encoding: tgrep_core::encoding::EncodingMode) -> Self {
        let (text, fixups) = tgrep_core::encoding::decode_owned_with_fixups(bytes, encoding);
        Self { text, fixups }
    }

    /// Heap cost of this entry, used to bound the cache by memory rather than
    /// by entry count.
    fn heap_bytes(&self) -> u64 {
        self.text.capacity() as u64 + self.fixups.heap_bytes()
    }
}

/// An LRU of decoded file contents bounded by *bytes* as well as by entry count.
///
/// Bounding only by entry count makes memory unbounded in practice: 50,000
/// entries of arbitrary size. Serving a repository that contains one 13.7 GiB
/// build artifact took the server to 14.4 GiB of private memory after a single
/// query, and it stayed there for the life of the process, because nothing ever
/// evicted that one entry.
///
/// Two limits are applied:
///   * `max_bytes` - total cached bytes; the least-recently-used entries are
///     evicted until the total fits.
///   * `max_entry_bytes` - entries larger than this are never admitted. A file
///     that alone exceeds the whole budget would evict every useful entry to
///     cache itself, and would then be evicted by the next insert anyway.
struct ContentCache {
    lru: LruCache<String, Arc<DecodedFile>>,
    bytes: u64,
    max_bytes: u64,
    max_entry_bytes: u64,
}

impl ContentCache {
    fn new(capacity: usize, max_bytes: u64, max_entry_bytes: u64) -> Self {
        Self {
            lru: LruCache::new(NonZeroUsize::new(capacity).unwrap()),
            bytes: 0,
            max_bytes,
            max_entry_bytes,
        }
    }

    fn peek(&self, key: &str) -> Option<&Arc<DecodedFile>> {
        self.lru.peek(key)
    }

    /// Promote an entry to most-recently-used.
    fn touch(&mut self, key: &str) {
        self.lru.get(key);
    }

    fn put(&mut self, key: String, value: Arc<DecodedFile>) {
        let size = value.heap_bytes();
        if size > self.max_entry_bytes {
            return;
        }
        // `push`, not `put`: `put` returns the old value only when the *key*
        // already existed, and stays silent when the entry-count limit evicts
        // some other entry to make room. That eviction still frees bytes, so
        // using `put` would leak the running total upward until the byte budget
        // looked full and the cache evicted everything.
        if let Some((_, old)) = self.lru.push(key, value) {
            self.bytes = self.bytes.saturating_sub(old.heap_bytes());
        }
        self.bytes = self.bytes.saturating_add(size);
        self.evict_to_fit();
    }

    fn evict_to_fit(&mut self) {
        while self.bytes > self.max_bytes {
            match self.lru.pop_lru() {
                Some((_, evicted)) => {
                    self.bytes = self.bytes.saturating_sub(evicted.heap_bytes());
                }
                None => {
                    self.bytes = 0;
                    break;
                }
            }
        }
    }

    fn pop(&mut self, key: &str) {
        if let Some(old) = self.lru.pop(key) {
            self.bytes = self.bytes.saturating_sub(old.heap_bytes());
        }
    }

    fn clear(&mut self) {
        self.lru.clear();
        self.bytes = 0;
    }

    fn len(&self) -> usize {
        self.lru.len()
    }

    fn byte_len(&self) -> u64 {
        self.bytes
    }
}

struct ServerState {
    index: RwLock<HybridIndex>,
    /// Paths admitted by traversal but deliberately absent from the content
    /// index. Unioning these with `HybridIndex::all_paths` answers `--files`
    /// without duplicating every searchable path in memory.
    filename_extra_paths: RwLock<std::collections::HashSet<String>>,
    /// False for legacy/partial indexes until a complete filesystem walk has
    /// established the extra-path set.
    filename_index_ready: std::sync::atomic::AtomicBool,
    /// The in-memory extra-path set differs from the last sidecar successfully
    /// published to disk. Authoritative walks retry while this remains set.
    filename_index_dirty: std::sync::atomic::AtomicBool,
    cache: RwLock<ContentCache>,
    cache_generation: std::sync::atomic::AtomicU64,
    root: PathBuf,
    watcher_active: std::sync::atomic::AtomicBool,
    /// True while the initial index build is in progress.
    indexing: std::sync::atomic::AtomicBool,
    /// True while a bulk flush to disk is running. Internal-only; not
    /// surfaced through `status`. Used to suppress the auto-save loop
    /// from kicking off a redundant parallel snapshot while the bulk
    /// flush (or stale-refresh flush) is still executing.
    flushing: std::sync::atomic::AtomicBool,
    /// True until the `.gitignore` matcher has been published for the first
    /// time. The watcher must not touch the index while this is set: with no
    /// matcher in place `should_skip_watcher_path` cannot apply gitignore
    /// rules, so it would index build output the walk deliberately skipped.
    ///
    /// Only the *initial* publish is gated. Later rebuilds (an edited
    /// `.gitignore`) swap the matcher atomically and leave the previous one
    /// readable throughout, so events keep being filtered by slightly stale
    /// but still valid rules rather than being dropped.
    gitignore_pending: std::sync::atomic::AtomicBool,
    /// An ignore file changed during the initial build and requires a
    /// reconciliation after the build publishes.
    ignore_rules_dirty: std::sync::atomic::AtomicBool,
    /// Ensures a burst of ignore-file events uses at most one refresh worker.
    ignore_refresh_scheduled: std::sync::atomic::AtomicBool,
    /// Last observed tracked-path membership. `None` means the published
    /// matcher has no case-insensitive tracked-file exemption.
    tracked_membership: Mutex<Option<tgrep_core::gitignore::TrackedMembershipFingerprint>>,
    /// Set when events were lost, cleared by the next subscription sync, which
    /// then re-issues every subscription instead of trusting its own records.
    ///
    /// A dropped directory-removal event leaves the registry recording a watch
    /// the kernel has already released. Nothing later contradicts that record:
    /// a path recreated at the same location is both wanted and believed
    /// watched, so every sync skips it and it never reports again. Only
    /// overflow can produce that state, so only overflow pays for the repair.
    watch_resubscribe: std::sync::atomic::AtomicBool,
    /// The ignore files the published matcher was built from.
    ///
    /// A recovery scan can spot an ignore file that *arrived* during its window
    /// by its mtime, but a deleted one leaves nothing behind to notice. That is
    /// the more damaging direction: the matcher keeps enforcing rules whose
    /// source is gone, so an entire subtree stays unsubscribed and unindexed
    /// until something else forces a rebuild. Keeping the source list lets the
    /// scan test for it directly, at one stat per ignore file per scan.
    ignore_sources: RwLock<Vec<PathBuf>>,
    /// What the published matcher actually read, per ignore source: a hash of
    /// the bytes, keyed by the file's path relative to `root`.
    ///
    /// A pathname is not evidence about contents, and neither is metadata. An
    /// existing `.gitignore` can be replaced after the matcher was built by one
    /// restored from an archive — same length, mtime preserved by the restore,
    /// so the path is still a known source and nothing about its metadata has
    /// moved. Neither test in [`changed_ignore_rules_in`] would fire, and the
    /// subtree would be indexed under rules that were never read. Comparing
    /// against what was read closes that.
    ///
    /// Also holds an entry for the target of any source reached through a
    /// symlink, when that target is itself under `root`. Following links is
    /// what the walker does, so the matcher's contents come from the target —
    /// but an edit to the target does not touch the link, and no event names a
    /// path whose basename is `.gitignore`. The target's own entry is what
    /// lets that event be recognised. A target outside `root` is not watched at
    /// all, so for those the reconcile stays the backstop.
    ignore_source_stamps: RwLock<IgnoreStamps>,
    /// Serializes the whole check-read-commit cycle in [`reindex_file`].
    ///
    /// `snapshot_gate` is held for *read* by everything that indexes a file, so
    /// the watcher worker and a recovery scan can be inside `reindex_file` for
    /// the same path at once. Both then see the same old stamp, both read, and
    /// whichever commits last wins — which is not necessarily the one that read
    /// the newer content. The losing write is already consumed, so the stale
    /// version survives until the next reconcile.
    ///
    /// Taken per file rather than per scan, so a recovery pass and the watcher
    /// interleave instead of one waiting out the other. It is never held across
    /// anything but one file's read, and searches do not take it at all.
    reindex_lock: Mutex<()>,
    /// Paths the watcher saw while `indexing` was set, kept so they can be
    /// replayed once the build publishes, each with whether its original event
    /// could have introduced a directory.
    ///
    /// Events that arrive during a build cannot be applied — the stamps do not
    /// describe the index yet, so every path would read as changed — but they
    /// are the only record that those paths moved. The build's own walk misses
    /// anything written to a directory it has already passed, and on a
    /// whole-subtree backend there is no per-directory recovery scan to fall
    /// back on, so discarding them leaves the change invisible until the hourly
    /// reconcile.
    ///
    /// The flag has to be carried rather than reconstructed. Replaying
    /// everything as a create would put every recorded path through
    /// `watch_new_subtree`, and a recursive `chmod` or a checkout fires a
    /// metadata-only modify per directory — so a tree that produced no new
    /// directories at all would be walked and force-resubscribed once per
    /// recorded path, which is quadratic over a deep checkout.
    ///
    /// `None` means the buffer overflowed and the paths were dropped; the
    /// replay then falls back to a full stale refresh, which is slower but
    /// complete. Bounded because a build can run for minutes on a large
    /// repository and a checkout or a build tree churning underneath it is
    /// unbounded.
    deferred_events: Mutex<Option<std::collections::HashMap<PathBuf, bool>>>,
    /// Progress: number of files indexed so far.
    index_progress: std::sync::atomic::AtomicU64,
    /// Total files discovered for indexing.
    index_total: std::sync::atomic::AtomicU64,
    /// True when file watching is enabled for this server.
    watch_enabled: bool,
    /// The live watcher and the directories it is subscribed to.
    ///
    /// Held here rather than by `run` because the subscription set is not
    /// fixed: publishing a new ignore matcher renarrows it, and a directory
    /// created after startup has to be subscribed as it appears.
    watch_registry: Mutex<Option<WatchRegistry>>,
    /// Directories to exclude from indexing.
    exclude_dirs: Vec<String>,
    /// Disable all source-control ignore files for every server discovery path.
    no_ignore: bool,
    /// Respect `.gitignore` outside a git repository, for every server
    /// discovery path. Must be applied consistently to the index build, the
    /// startup metadata walk and the watcher's rescan, or those disagree about
    /// which files belong in the index.
    no_require_git: bool,
    /// Size cap for indexable files, shared by the index build and the startup
    /// metadata walk. They must agree: a file the index holds but the metadata
    /// walk skips looks *deleted* to the stale check and is evicted.
    max_file_size: Option<u64>,
    /// On-disk index directory used by live ignore-rule reconciliation.
    index_dir: PathBuf,
    /// Serializes on-disk index publication across all publishers
    /// (auto-save, checkpoint, flush). Held across
    /// `move_staged_files` + `IndexReader::open` + `swap_reader` so
    /// that concurrent publishers cannot interleave per-file renames
    /// into `index_dir` (which would leave a mismatched mix of
    /// `index.bin` / `lookup.bin` / `files.bin` from different
    /// snapshots) or swap readers out of order. Searches do **not**
    /// take this lock, so they continue uninterrupted during a publish.
    publish_lock: Mutex<()>,
    /// Last-known per-file stamps (mtime + size). Used by the file
    /// watcher to ignore notify events that don't reflect a real
    /// content change (e.g. atime-only updates, attribute changes,
    /// or events triggered by the search itself opening files on
    /// some filesystems). Loaded from `filestamps.json` at startup
    /// and refreshed during the initial build, on stale-state
    /// refresh, and per watcher event that actually mutates the
    /// index.
    file_stamps: RwLock<std::collections::HashMap<String, tgrep_core::meta::FileStamp>>,
    /// Coordinates overlay mutations with the snapshot→publish→prune
    /// window. Watcher mutations (handle_fs_event) acquire it for
    /// **read** before touching the live overlay; flush/auto-save
    /// publishers acquire it for **write** for the entire cycle from
    /// taking the snapshot through pruning the now-persisted entries.
    ///
    /// Without this gate, a watcher event that fires after the
    /// snapshot is taken but before `prune_persisted_entries` runs
    /// would silently lose its mutation: the snapshot doesn't see
    /// the new content, the on-disk reader is reopened with the old
    /// version, then prune deletes the overlay entry by path because
    /// the path now matches a reader entry — orphaning the new data.
    ///
    /// Searches do **not** take this lock; they keep using the
    /// current reader + overlay throughout.
    snapshot_gate: RwLock<()>,
    /// Serializes the complete stale walk → matcher → merge cycle. Serializing
    /// only publication would let an older walk publish after a newer
    /// ignore-rule refresh and roll the index back to stale ignore semantics.
    stale_refresh_lock: Mutex<()>,
    /// Gitignore matcher used by the file watcher to drop events for
    /// paths the initial walk would have skipped via the `ignore` crate
    /// (`.gitignore`, `.git/info/exclude`, global gitignore, etc.).
    /// Built asynchronously after the server starts so large repos don't block
    /// `serve ready` on a full-tree `.gitignore` discovery walk. `None` while
    /// loading or if no matcher could be built; during that window the watcher
    /// falls back to hidden / exclude filtering.
    gitignore: RwLock<Option<tgrep_core::gitignore::IgnoreMatcher>>,
    /// Maximum RSS budget (bytes). When the process exceeds this during the
    /// initial build, the indexer flushes the overlay to disk and continues so
    /// peak memory stays bounded while still producing a complete index.
    memory_cap_bytes: u64,
    /// Number of worker threads used for the parallel file-reading/trigram
    /// extraction during the initial build. Caps indexing CPU usage.
    index_threads: usize,
    /// Number of accumulated in-memory mutations that triggers a background
    /// save. Higher values reduce save frequency (and the pauses they cause)
    /// at the cost of more unsaved work if the process is killed.
    auto_save_mutations: u32,
    /// Files a delta build could not read, and the metadata they had when it
    /// tried.
    ///
    /// A file that fails to read has its stamp withheld so the next reconcile
    /// retries it rather than recording it as indexed. That is right for a
    /// transient failure and wrong for a permanent one: a file locked by
    /// another process, or unreadable by permission, fails identically every
    /// time, and with a reconcile on a timer it would rewrite the whole index
    /// once an hour forever to re-attempt a file that will fail again.
    ///
    /// Remembering the metadata of the attempt separates the two. A file whose
    /// mtime and size have not moved since it failed is not worth another
    /// read; one that has changed gets a fresh look. The record lives in
    /// memory, so restarting the server retries everything — which is the
    /// escape hatch for a file made readable without being modified.
    unreadable: RwLock<std::collections::HashMap<String, tgrep_core::meta::FileStamp>>,
    /// Reference point for [`ServerState::quiet_for`], because an `Instant`
    /// cannot live in an atomic.
    started: Instant,
    /// Milliseconds since `started` at the last search request, used by the
    /// periodic reconcile to stay out of the way of a server in active use.
    last_search_ms: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    stale_refresh_hook: Mutex<Option<StaleRefreshHook>>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum StaleRefreshPhase {
    BeforeWalk,
    AfterBuildBeforeStampPublish,
    AfterConcreteRead,
    BeforeConcreteCommit,
    AfterMatcherPublish,
}

#[cfg(test)]
type StaleRefreshHook = Arc<dyn Fn(StaleRefreshPhase) + Send + Sync>;

impl ServerState {
    fn note_search(&self) {
        self.last_search_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// How long the server has gone without a search.
    fn quiet_for(&self) -> Duration {
        let last = self.last_search_ms.load(Ordering::Relaxed);
        self.started
            .elapsed()
            .saturating_sub(Duration::from_millis(last))
    }
}

/// `Default` exists only for tests, which need to vary one field at a time.
/// Production builds construct every field from the request so a new one can
/// never be silently left at its zero value.
#[cfg_attr(test, derive(Default))]
struct SearchOpts {
    files_only: bool,
    invert_match: bool,
    only_matching: bool,
    max_count: Option<usize>,
    before_context: usize,
    after_context: usize,
    multiline: bool,
    /// `-a/--text`: search binary files instead of reporting a note.
    text: bool,
    /// Send the matching lines of a binary file along with the marker, for
    /// clients whose output format reports them (currently `--json`).
    binary_lines: bool,
    /// `--max-filesize`: skip candidates larger than this.
    max_filesize: Option<u64>,
    /// `--passthru`: emit every line, matching or not.
    passthru: bool,
    /// `-r/--replace`: template substituted for each match.
    replace: Option<String>,
    /// `--stop-on-nonmatch`: stop searching a file at its first non-match.
    stop_on_nonmatch: bool,
    /// `--vimgrep`: collapse a multiline match onto the line it starts on so
    /// the client emits one row per match.
    vimgrep: bool,
    /// Whether the client will read per-match `spans` and `columns`.
    ///
    /// They dominate the size of a reply, so a client that only prints lines
    /// asks for them to be left out.
    detail: bool,
    /// Whether the client will read per-row `offset` and `term`.
    ///
    /// Only `-b/--byte-offset` and `-M/--max-columns` look at them.
    positions: bool,
}

impl SearchOpts {
    /// The matching-relevant subset, shared with the local search path.
    fn match_options(&self) -> crate::matching::MatchOptions {
        crate::matching::MatchOptions {
            invert_match: self.invert_match,
            multiline: self.multiline,
            only_matching: self.only_matching,
            before_context: self.before_context,
            after_context: self.after_context,
            max_count: if self.files_only {
                Some(1)
            } else {
                self.max_count
            },
            passthru: self.passthru,
            replace: self.replace.clone(),
            stop_on_nonmatch: self.stop_on_nonmatch,
            vimgrep: self.vimgrep,
            all_spans: self.detail,
        }
    }
}

pub struct ServeOptions<'a> {
    pub no_watch: bool,
    pub exclude_dirs: &'a [String],
    pub memory_cap_bytes: u64,
    pub index_threads: usize,
    pub no_ignore: bool,
    /// `--no-require-git`: respect `.gitignore` outside a git repository.
    pub no_require_git: bool,
    /// `--max-filesize`: skip files larger than this when building and when
    /// checking for stale files. `None` means no limit.
    pub max_file_size: Option<u64>,
    pub auto_save_mutations: Option<u32>,
    /// Bound on the watcher's hand-off queue. `None` uses [`WATCHER_QUEUE_CAP`].
    pub watcher_queue_cap: Option<usize>,
}

pub fn run(root: &Path, index_path: Option<&Path>, options: ServeOptions<'_>) -> Result<()> {
    let ServeOptions {
        no_watch,
        exclude_dirs,
        memory_cap_bytes,
        index_threads,
        no_ignore,
        no_require_git,
        max_file_size,
        auto_save_mutations,
        watcher_queue_cap,
    } = options;
    let serve_start = Instant::now();
    let watcher_queue_cap = watcher_queue_cap.unwrap_or(WATCHER_QUEUE_CAP);
    let auto_save_mutations = auto_save_mutations.unwrap_or(AUTO_SAVE_MUTATIONS);
    let root = std::fs::canonicalize(root)?;
    let index_dir = index_path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| builder::default_index_dir(&root));

    // Ensure only one server runs per index directory.
    // The lock file is held for the lifetime of the server and released on exit.
    let _lock_file = try_acquire_server_lock(&index_dir)?;

    let has_index = index_dir.join("lookup.bin").exists();
    let mut needs_build = !has_index;

    if !has_index {
        // Create an empty on-disk index so HybridIndex can open it.
        create_empty_index(&index_dir)?;
        eprintln!("[trace] no existing index found, will build in background");
    }

    // Open the hybrid index (may be empty or partial from a previous checkpoint)
    let index_start = Instant::now();
    let hybrid = match HybridIndex::open(&index_dir, &root) {
        Ok(hybrid) => hybrid,
        Err(e) if has_index => {
            eprintln!("[trace] existing index failed to load ({e}); rebuilding in background");
            create_empty_index(&index_dir)?;
            needs_build = true;
            HybridIndex::open(&index_dir, &root)?
        }
        Err(e) => return Err(e.into()),
    };
    let existing_files = hybrid.num_files();

    // Check meta.json complete flag to decide whether to rebuild
    let index_complete = tgrep_core::meta::IndexMeta::load(&index_dir)
        .map(|m| m.complete)
        .unwrap_or(false);
    let (filename_extra_paths, filename_index_ready) = if index_complete {
        match tgrep_core::path_index::read_extra_paths(&index_dir) {
            Ok(Some(paths)) => (paths.into_iter().collect(), true),
            Ok(None) => (std::collections::HashSet::new(), false),
            Err(error) => {
                eprintln!("[trace] filename index failed to load ({error}); rebuilding");
                (std::collections::HashSet::new(), false)
            }
        }
    } else {
        (std::collections::HashSet::new(), false)
    };

    let needs_build = needs_build || !index_complete;

    eprintln!(
        "[trace] opened index: {} files, {} trigrams in {:.1}ms{}",
        existing_files,
        hybrid.num_trigrams(),
        index_start.elapsed().as_secs_f64() * 1000.0,
        if !index_complete && has_index {
            " (partial — will continue building)"
        } else {
            ""
        }
    );

    let state = Arc::new(ServerState {
        index: RwLock::new(hybrid),
        filename_extra_paths: RwLock::new(filename_extra_paths),
        filename_index_ready: std::sync::atomic::AtomicBool::new(filename_index_ready),
        filename_index_dirty: std::sync::atomic::AtomicBool::new(false),
        cache: RwLock::new(ContentCache::new(
            CACHE_CAPACITY,
            CACHE_MAX_BYTES,
            CACHE_MAX_ENTRY_BYTES,
        )),
        cache_generation: std::sync::atomic::AtomicU64::new(0),
        root: root.clone(),
        watcher_active: std::sync::atomic::AtomicBool::new(false),
        indexing: std::sync::atomic::AtomicBool::new(needs_build),
        flushing: std::sync::atomic::AtomicBool::new(false),
        // Only meaningful when the watcher runs with gitignore filtering
        // enabled; otherwise there is no matcher to wait for.
        gitignore_pending: std::sync::atomic::AtomicBool::new(!no_watch && !no_ignore),
        ignore_rules_dirty: std::sync::atomic::AtomicBool::new(false),
        ignore_refresh_scheduled: std::sync::atomic::AtomicBool::new(false),
        tracked_membership: Mutex::new(None),
        watch_resubscribe: std::sync::atomic::AtomicBool::new(false),
        ignore_sources: RwLock::new(Vec::new()),
        ignore_source_stamps: RwLock::new(IgnoreStamps::new()),
        reindex_lock: Mutex::new(()),
        deferred_events: Mutex::new(Some(std::collections::HashMap::new())),
        index_progress: std::sync::atomic::AtomicU64::new(0),
        index_total: std::sync::atomic::AtomicU64::new(0),
        watch_enabled: !no_watch,
        watch_registry: Mutex::new(None),
        exclude_dirs: exclude_dirs.to_vec(),
        no_ignore,
        no_require_git,
        max_file_size,
        index_dir: index_dir.clone(),
        publish_lock: Mutex::new(()),
        file_stamps: RwLock::new(tgrep_core::meta::read_filestamps(&index_dir).unwrap_or_default()),
        snapshot_gate: RwLock::new(()),
        stale_refresh_lock: Mutex::new(()),
        gitignore: RwLock::new(None),
        memory_cap_bytes,
        index_threads,
        auto_save_mutations,
        unreadable: RwLock::new(std::collections::HashMap::new()),
        started: serve_start,
        last_search_ms: std::sync::atomic::AtomicU64::new(0),
        #[cfg(test)]
        stale_refresh_hook: Mutex::new(None),
    });

    // Bind TCP listener on a random port
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    // Write server info
    let info = ServerInfo {
        pid: std::process::id(),
        port,
    };
    info.save(&index_dir)?;

    eprintln!(
        "[trace] serve ready in {:.1}ms. TCP on port {}. Cache: max {} entries / {} MiB \
         (entries over {} MiB not cached). \
         Memory cap: {} MB. Index threads: {}. Watcher queue cap: {}.",
        serve_start.elapsed().as_secs_f64() * 1000.0,
        port,
        CACHE_CAPACITY,
        CACHE_MAX_BYTES / (1024 * 1024),
        CACHE_MAX_ENTRY_BYTES / (1024 * 1024),
        memory_cap_bytes / (1024 * 1024),
        index_threads,
        watcher_queue_cap,
    );

    // If no pre-existing index, build into the LiveIndex in background
    if needs_build {
        let build_state = Arc::clone(&state);
        let build_root = root.clone();
        let build_index_dir = index_dir.clone();
        thread::spawn(move || {
            background_index_build(&build_state, &build_root, &build_index_dir);
        });
    } else {
        // Index is complete — check for files that changed while server was offline
        let stale_state = Arc::clone(&state);
        let stale_root = root.clone();
        let stale_index_dir = index_dir.clone();
        thread::spawn(move || {
            // The stale check owns publishing the gitignore matcher on this
            // path: it walks the whole tree anyway, so it collects the ignore
            // files as it goes rather than paying for a second traversal.
            //
            // On this path `indexing` is false from the very first event, so
            // `gitignore_pending` is the only thing keeping the watcher off
            // the index while the matcher is missing. That gate is armed when
            // `ServerState` is built — before this thread is spawned and
            // before `start_file_watcher` runs below — so it holds whichever
            // of the two wins the race, and the stale check releases it as
            // soon as its walk finishes, ahead of every one of its returns.
            //
            // The same stale check is also what recovers events dropped
            // during the gap: it compares the whole tree against the index,
            // so any edit that landed while the matcher was still building is
            // picked up here.
            let mut retry_delay = Duration::from_secs(1);
            while !background_refresh_stale(&stale_state, &stale_root, &stale_index_dir, false) {
                eprintln!(
                    "[trace] stale check: retrying in {:.0}s",
                    retry_delay.as_secs_f64()
                );
                thread::sleep(retry_delay);
                retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
            }
        });
    }

    // Start file watcher (unless --no-watch)
    if no_watch {
        eprintln!("[trace] file watcher disabled (--no-watch)");
    } else {
        let watcher_state = Arc::clone(&state);
        let watcher_root = root.clone();
        start_file_watcher(watcher_state, &watcher_root, watcher_queue_cap);
    }

    // Set up graceful shutdown
    let shutdown_index_dir = index_dir.clone();
    ctrlc_handler(move || {
        eprintln!("\n[trace] shutting down...");
        ServerInfo::cleanup(&shutdown_index_dir);
        std::process::exit(0);
    });

    // Start auto-save thread
    let save_state = Arc::clone(&state);
    thread::spawn(move || auto_save_loop(save_state));

    // Bound how long a change the watcher never heard about can stay wrong.
    // Pointless without a watcher: `--no-watch` means the index is only ever
    // refreshed on request, and silently rewriting it on a timer would be a
    // surprise rather than a repair.
    if !no_watch {
        let reconcile_state = Arc::clone(&state);
        let reconcile_root = root.clone();
        let reconcile_index_dir = index_dir.clone();
        thread::spawn(move || {
            periodic_reconcile_loop(reconcile_state, reconcile_root, reconcile_index_dir)
        });
    }

    // Accept connections
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, &state) {
                        eprintln!("[trace] connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[trace] accept error: {e}"),
        }
    }

    Ok(())
}

/// Build the ignore matcher used by the watcher from files discovered by a walk
/// that already happened. The stale path needs it even with `--no-watch`.
///
/// Reusing the caller's walk is the whole point: `gitignore::build_matcher`
/// would traverse the entire tree a second time purely to find the same ignore
/// files. On a 289k-file repo on a network drive that second walk cost 205s,
/// against 1.6s for the stale-check walk over the same tree.
///
fn build_stale_matcher(
    state: &ServerState,
    root: &Path,
    walk: &tgrep_core::walker::MetaWalkResult,
    ignorecase: Option<std::sync::Arc<tgrep_core::gitignore::CaseInsensitiveIgnore>>,
) -> Option<tgrep_core::gitignore::IgnoreMatcher> {
    if state.no_ignore {
        return None;
    }

    let start = Instant::now();
    let matcher = tgrep_core::walker::build_gitignore_matcher_from_files_with_ignorecase(
        root,
        &walk.gitignore_files,
        &walk.ignore_files,
        state.no_require_git,
        ignorecase,
    );
    let has_matcher = matcher.is_some();
    eprintln!(
        "[trace] gitignore matcher built from stale walk in {:.1}ms \
         ({} .gitignore + {} .ignore files{})",
        start.elapsed().as_secs_f64() * 1000.0,
        walk.gitignore_files.len(),
        walk.ignore_files.len(),
        if has_matcher { "" } else { ", no rules found" }
    );
    matcher
}

/// What an ignore source contained when the matcher read it: its length and a
/// hash of its bytes, keyed by its path relative to the served root.
///
/// A digest rather than metadata, because metadata does not answer the
/// question. `rsync -a`, `tar -x` and a restore from an archive all preserve
/// mtime, and two different sets of rules can easily be the same number of
/// bytes — at which point a size-and-mtime pair is identical across a
/// replacement and the scan accepts a matcher built from rules that are gone.
///
/// Never persisted, so the hash only has to be stable within a run.
type IgnoreStamps = std::collections::HashMap<String, (u64, u64)>;

/// The length and content hash of `path`, following links.
///
/// `None` when it cannot be read, which is treated as "not what was read":
/// a source that has become unreadable has stopped contributing the rules the
/// matcher is enforcing, and that is a change.
fn ignore_digest_of(path: &Path) -> Option<(u64, u64)> {
    use std::hash::{Hash, Hasher};

    // The whole file, as the matcher builder reads it. These are rule files:
    // a few hundred bytes each in practice, and already in the page cache from
    // the walk that found them.
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some((bytes.len() as u64, hasher.finish()))
}

/// How far an mtime may lag the write it records.
///
/// Two seconds, which covers the coarsest granularity still in use: FAT and
/// its descendants store modification times in two-second units, HFS+ and
/// ext3 in whole seconds. Used to widen comparisons against a wall-clock
/// instant, which has no such rounding, so a write cannot be dated before a
/// moment it actually followed.
const MTIME_GRANULARITY: Duration = Duration::from_secs(2);

/// The ignore files a matcher was built from, as one list.
///
/// Root-level `p4ignore.ini` is a separate source from the walker's point of
/// view — it is applied as its own filter rather than collected with the
/// gitignore files — but deleting it invalidates the published rules exactly
/// the same way, so it belongs in the list.
///
/// So do the sources that sit *outside* the served tree: parent-directory
/// `.ignore` / `.gitignore` files and the repository's `info/exclude`. The
/// walk that found everything else never visits them, so without this nothing
/// would notice one being deleted — and a rule with no source left keeps being
/// enforced, holding a subtree unsubscribed and unindexed until an unrelated
/// rebuild happens along. Only files that exist are listed; a path that was
/// never there is not a source that went missing.
///
/// The user's global ignore file is deliberately absent: the `ignore` crate
/// resolves it through git's config precedence and does not hand back the path
/// it chose, and guessing wrong would report a source as vanished on every
/// scan.
fn ignore_sources_of(
    root: &Path,
    gitignore_files: &[PathBuf],
    ignore_files: &[PathBuf],
    no_require_git: bool,
) -> Vec<PathBuf> {
    let mut sources = Vec::with_capacity(gitignore_files.len() + ignore_files.len() + 1);
    sources.extend_from_slice(gitignore_files);
    sources.extend_from_slice(ignore_files);
    let p4 = root.join(tgrep_core::gitignore::P4IGNORE_FILENAME);
    if p4.is_file() {
        sources.push(p4);
    }
    sources.extend(
        tgrep_core::gitignore::ancestor_ignore_paths(root, no_require_git)
            .into_iter()
            .map(|(path, _)| path),
    );
    sources.extend(tgrep_core::gitignore::repo_exclude_path(root));
    sources
}

/// Read every source so a later scan can ask whether the file it finds is the
/// one the matcher read, rather than merely whether something of that name is
/// there.
///
/// A source reached through a symlink gets a second entry under its target's
/// own relative path, when the target is under `root`. The read follows links
/// either way, so both entries describe the contents that were read.
///
/// Taken here rather than inside the matcher builder because the `ignore`
/// crate opens these files itself and does not hand back what it read. That
/// leaves a window between its read and this one, which is what the mtime test
/// in [`changed_ignore_rules_in`] is for.
fn ignore_stamps_of(root: &Path, sources: &[PathBuf]) -> IgnoreStamps {
    let canonical_root = std::fs::canonicalize(root).ok();
    let mut stamps = IgnoreStamps::with_capacity(sources.len());
    let mut record = |path: &Path, base: &Path| {
        // A source above the root — a parent `.gitignore`, the repository's
        // `info/exclude` — has no path relative to it. Key it by its own full
        // path rather than dropping it: these keys only have to tell one source
        // from another within a single run, and `changed_ignore_rules_in` looks
        // up by relative path, so an absolute key is simply never hit there.
        // Dropped, the digest comparison in `publish_ignore_matcher` would be
        // blind to a source it is being asked to watch.
        let key = match path.strip_prefix(base) {
            Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
            Err(_) => path.to_string_lossy().replace('\\', "/"),
        };
        if let Some(digest) = ignore_digest_of(path) {
            stamps.insert(key, digest);
        }
    };
    for source in sources {
        record(source, root);
        let is_link = std::fs::symlink_metadata(source).is_ok_and(|m| m.file_type().is_symlink());
        if !is_link {
            continue;
        }
        if let Some(canonical_root) = canonical_root.as_ref()
            && let Ok(target) = std::fs::canonicalize(source)
            && target.starts_with(canonical_root)
        {
            record(&target, canonical_root);
        }
    }
    stamps
}

/// Publish a new ignore matcher and bring everything that depends on it up to
/// date. `None` is a legitimate matcher when no rules exist.
///
/// Callers on the stale path hold `snapshot_gate` for write, which is what
/// makes the matcher swap and the index decisions around it atomic from the
/// watcher's point of view.
///
/// Returns the directories that were newly subscribed to as a result. Those
/// were unwatched while the caller's walk ran, so anything written to them in
/// that window produced no event and appears in no walk result. Callers pass
/// them to [`reindex_files_in`] once `state.file_stamps` describes the index
/// they just published.
///
/// The returned directories must be paired with a timestamp the *caller*
/// captured before the walk that produced `matcher`, and handed to
/// [`reindex_files_in`] as its `since`. This function cannot supply it: the
/// subscription walk below starts later, so an ignore file written between the
/// caller's walk and this point would predate any timestamp taken here and be
/// read as already accounted for by rules that never saw it.
///
/// `sources` are the ignore files `matcher` was built from. They are recorded
/// so a recovery scan can notice one being deleted — which no test against
/// what is on disk can see, a deleted file leaving nothing to read — and read
/// so it can also notice one being replaced, which neither a pathname nor a
/// timestamp can show.
///
/// The matcher is built *here*, from `build`, rather than being handed in
/// already made. The stamps have to describe the bytes the matcher actually
/// read, and the read happens inside the build — `GitignoreBuilder::add` opens
/// each source itself. Stamping afterwards alone would record whatever is on
/// disk when the build finishes, so an mtime-preserving atomic replace during
/// the build would leave the matcher enforcing the old rules while the stamps
/// swore they were current, and every later check — pathname, timestamp,
/// digest — would agree that nothing needed rereading. Taking the digests on
/// both sides of the build turns that into something visible: if a source
/// moved underneath it, the published matcher is marked stale and a refresh is
/// scheduled. It is still published, because the alternative is no matcher at
/// all, which means indexing ignored paths until the refresh lands.
#[must_use = "newly watched directories need a recovery scan or writes race the subscription"]
fn publish_ignore_matcher(
    state: &Arc<ServerState>,
    root: &Path,
    sources: Vec<PathBuf>,
    build: impl FnOnce() -> Option<tgrep_core::gitignore::IgnoreMatcher>,
) -> Vec<PathBuf> {
    let before = ignore_stamps_of(root, &sources);
    let matcher = build();
    let stamps = ignore_stamps_of(root, &sources);
    let raced = stamps != before;

    *state.ignore_source_stamps.write().unwrap() = stamps;
    *state.ignore_sources.write().unwrap() = sources;
    // Keep the matcher and semantic baseline in one critical section. The
    // matcher's tracked exemption is immutable, so this baseline describes the
    // exact decisions the stale walk and watcher publication made.
    let mut published = state.gitignore.write().unwrap();
    let mut membership = state.tracked_membership.lock().unwrap();
    *membership = matcher
        .as_ref()
        .and_then(tgrep_core::gitignore::IgnoreMatcher::tracked_membership_fingerprint);
    *published = matcher;
    drop(membership);
    drop(published);
    state.gitignore_pending.store(false, Ordering::SeqCst);
    // New rules mean a different set of directories worth hearing about:
    // a tightened rule releases the subscriptions under it, and a relaxed
    // one takes subscriptions for the tree it used to hide.
    let newly_watched = sync_watch_registrations(state, root).0;

    if raced {
        // After the publish, so the refresh runs against the matcher and the
        // subscriptions this call just established rather than racing them.
        // The refresh rewalks and republishes, and a filesystem that has
        // stopped moving produces matching digests next time, so this
        // converges rather than looping.
        eprintln!(
            "[trace] warning: an ignore rules file changed while the matcher was \
             being built; scheduling a refresh"
        );
        state.ignore_rules_dirty.store(true, Ordering::SeqCst);
        schedule_ignore_rules_refresh(Arc::clone(state), root.to_path_buf());
    }

    newly_watched
}

fn handle_connection(stream: TcpStream, state: &Arc<ServerState>) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let mut line = String::new();
    while reader.read_line(&mut line)? > 0 {
        let response = process_request(&line, state);
        writeln!(writer, "{response}")?;
        writer.flush()?;
        line.clear();
    }

    Ok(())
}

fn process_request(request: &str, state: &Arc<ServerState>) -> String {
    let req: serde_json::Value = match serde_json::from_str(request) {
        Ok(v) => v,
        Err(e) => {
            return json_rpc_error(None, -32700, &format!("Parse error: {e}"));
        }
    };

    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    match method {
        "search" => handle_search(id, &params, state),
        "files" => handle_files(id, state),
        "status" => handle_status(id, state),
        "reload" => handle_reload(id, state),
        _ => json_rpc_error(id, -32601, &format!("Method not found: {method}")),
    }
}

fn handle_files(id: Option<serde_json::Value>, state: &ServerState) -> String {
    state.note_search();
    if state.indexing.load(Ordering::SeqCst) || !state.filename_index_ready.load(Ordering::SeqCst) {
        return json_rpc_error(id, -32001, "filename index is not ready");
    }

    let mut files = {
        // Keep the documented index -> filename lock order.
        let index = state.index.read().unwrap();
        let extra = state.filename_extra_paths.read().unwrap();
        let mut files = index.all_paths();
        files.extend(extra.iter().cloned());
        files
    };
    files.sort_unstable();
    files.dedup();
    json_rpc_result(id, serde_json::json!({ "files": files }))
}

/// Parsed and validated search request parameters.
struct SearchRequest {
    pattern: String,
    case_insensitive: bool,
    matcher: crate::matching::SearchMatcher,
    plan: query::QueryPlan,
    glob_filter: crate::glob_filter::GlobFilter,
    type_filter: tgrep_core::filetypes::TypeFilter,
    encoding: tgrep_core::encoding::EncodingMode,
    opts: SearchOpts,
}

/// Parse and validate all search parameters from a JSON-RPC request.
/// Returns Err(String) with an error message suitable for json_rpc_error on failure.
fn parse_search_params(params: &serde_json::Value) -> std::result::Result<SearchRequest, String> {
    let pattern = params.get("pattern").and_then(|p| p.as_str()).unwrap_or("");
    let extra_patterns: Vec<String> = params
        .get("extra_patterns")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let case_insensitive = params
        .get("case_insensitive")
        .and_then(|c| c.as_bool())
        .unwrap_or(false);
    let fixed_string = params
        .get("fixed_string")
        .and_then(|f| f.as_bool())
        .unwrap_or(false);
    let word_boundary = params
        .get("word_boundary")
        .and_then(|w| w.as_bool())
        .unwrap_or(false);
    let max_count = params
        .get("max_count")
        .and_then(|m| m.as_u64())
        .map(|m| m as usize);
    let glob_filter_strs: Vec<String> = params
        .get("glob")
        .and_then(|g| g.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let iglob_strs: Vec<String> = params
        .get("iglob")
        .and_then(|g| g.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let glob_case_insensitive = params
        .get("glob_case_insensitive")
        .and_then(|g| g.as_bool())
        .unwrap_or(false);
    let glob_filter =
        crate::glob_filter::GlobFilter::new(&glob_filter_strs, &iglob_strs, glob_case_insensitive)
            .map_err(|e| format!("{e}"))?;
    let str_array = |key: &str| -> Vec<String> {
        params
            .get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    // The client sends the raw `-t`/`-T`/`--type-add`/`--type-clear` values and
    // the server rebuilds the filter with the same shared helper, so both sides
    // are guaranteed to derive an identical definition table.
    let type_filter = crate::search::build_type_filter(
        &str_array("type_add"),
        &str_array("type_clear"),
        &str_array("types"),
        &str_array("types_not"),
    )
    .map_err(|e| format!("{e}"))?;
    let invert_match = params
        .get("invert_match")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let only_matching = params
        .get("only_matching")
        .and_then(|o| o.as_bool())
        .unwrap_or(false);
    let after_context = params
        .get("after_context")
        .and_then(|a| a.as_u64())
        .map(|a| a as usize)
        .unwrap_or(0);
    let before_context = params
        .get("before_context")
        .and_then(|b| b.as_u64())
        .map(|b| b as usize)
        .unwrap_or(0);
    let multiline = params
        .get("multiline")
        .and_then(|m| m.as_bool())
        .unwrap_or(false);
    let multiline_dotall = params
        .get("multiline_dotall")
        .and_then(|m| m.as_bool())
        .unwrap_or(false);
    let files_only = params
        .get("files_only")
        .and_then(|f| f.as_bool())
        .unwrap_or(false);
    let text = params
        .get("text")
        .and_then(|t| t.as_bool())
        .unwrap_or(false);
    // Clients rendering JSON want the matching lines of a binary file, not just
    // the marker. Defaults to false so an older client keeps today's behaviour.
    let binary_lines = params
        .get("binary_lines")
        .and_then(|t| t.as_bool())
        .unwrap_or(false);
    let max_filesize = params.get("max_filesize").and_then(|m| m.as_u64());
    let line_regexp = params
        .get("line_regexp")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let no_unicode = params
        .get("no_unicode")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let engine = crate::matching::RegexEngine::from_str_opt(
        params
            .get("engine")
            .and_then(|v| v.as_str())
            .unwrap_or("auto"),
    )
    .ok_or_else(|| "unrecognized regex engine".to_string())?;
    // Saturate rather than truncate: these arrive from a client that already
    // rejected values too large for its own `usize`, so this only guards
    // against a malformed request applying a silently smaller limit.
    let regex_size_limit = params
        .get("regex_size_limit")
        .and_then(|v| v.as_u64())
        .map(|v| usize::try_from(v).unwrap_or(usize::MAX));
    let dfa_size_limit = params
        .get("dfa_size_limit")
        .and_then(|v| v.as_u64())
        .map(|v| usize::try_from(v).unwrap_or(usize::MAX));
    let replace = params
        .get("replace")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let passthru = params
        .get("passthru")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let stop_on_nonmatch = params
        .get("stop_on_nonmatch")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let vimgrep = params
        .get("vimgrep")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Older clients do not send this and do read the arrays, so absence has to
    // mean "send them".
    let detail = params
        .get("detail")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let positions = params
        .get("positions")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let encoding = tgrep_core::encoding::parse_encoding(
        params
            .get("encoding")
            .and_then(|e| e.as_str())
            .unwrap_or("auto"),
    )
    .map_err(|e| format!("{e}"))?;

    // Build combined regex from all patterns
    let mut all_patterns = vec![pattern.to_string()];
    all_patterns.extend(extra_patterns);

    let matcher = crate::matching::build_search_matcher(
        &all_patterns,
        &crate::matching::MatcherConfig {
            case_insensitive,
            fixed_string,
            word_boundary,
            line_regexp,
            multiline,
            dotall: multiline_dotall,
            no_unicode,
            engine,
            regex_size_limit,
            dfa_size_limit,
        },
    )
    .map_err(|e| format!("{e}"))?;

    // Build query plan from every pattern for index filtering. `-v` inverts at
    // the line level, so a file matches when some line does *not* match; the
    // trigram plan would select exactly the wrong candidates. A non-default
    // `--encoding` is unsound for the same reason: the index holds trigrams of
    // the BOM-sniffed text, so a re-decoded file can match bytes that were
    // never indexed.
    //
    // A PCRE-style pattern cannot be parsed by `regex-syntax` at all, but it can
    // usually be *relaxed* into one that can (dropping lookarounds and the like).
    // The relaxed pattern matches a superset, so its trigrams are still mandatory
    // for the original and the candidate set stays sound.
    let plan = if invert_match || encoding.may_differ_from_index() {
        query::QueryPlan::MatchAll
    } else if matcher.is_standard() || fixed_string {
        query::build_multi_pattern_plan(&all_patterns, fixed_string, case_insensitive)?
    } else {
        query::build_relaxed_multi_pattern_plan(&all_patterns, case_insensitive)
    };

    Ok(SearchRequest {
        pattern: pattern.to_string(),
        case_insensitive,
        matcher,
        plan,
        glob_filter,
        type_filter,
        encoding,
        opts: SearchOpts {
            files_only,
            invert_match,
            only_matching,
            max_count,
            before_context,
            after_context,
            multiline,
            text,
            binary_lines,
            max_filesize,
            passthru,
            replace,
            stop_on_nonmatch,
            vimgrep,
            detail,
            positions,
        },
    })
}

fn handle_search(
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
    state: &ServerState,
) -> String {
    let start = Instant::now();
    // Marks the server as in use, so the periodic reconcile waits for a gap
    // rather than walking the tree underneath a client that is mid-session.
    state.note_search();

    let req = match parse_search_params(params) {
        Ok(r) => r,
        Err(e) => return json_rpc_error(id, -32602, &e),
    };

    let matcher = req.matcher;
    let plan = req.plan;
    let glob_filter = req.glob_filter;
    let type_filter = req.type_filter;
    let encoding = req.encoding;
    let opts = req.opts;
    let pattern = req.pattern;
    let case_insensitive = req.case_insensitive;

    // The index build already dropped everything above the server's own cap, so
    // re-checking a query cap that is no stricter can never reject a candidate
    // the index did not already reject — it would only buy a `metadata` call
    // per candidate on every query. Now that a cap is the default rather than
    // opt-in, that is the common case, so recognise it and skip the stat.
    let query_size_limit = match (opts.max_filesize, state.max_file_size) {
        (Some(query), Some(built)) if built <= query => None,
        (query, _) => query,
    };

    // Collect candidates and their paths/full_paths while holding the index lock briefly
    let t_index = Instant::now();
    let (candidate_info, raw_candidate_count): (Vec<(String, PathBuf)>, usize) = {
        let index = state.index.read().unwrap();

        // The reader snapshot is returned alongside the file IDs so that
        // path resolution below uses the *same* reader that produced the
        // posting-list results.  Without this, a concurrent `swap_reader`
        // between the query and the path lookups would silently drop every
        // candidate whose ID does not exist in the new reader.
        let (candidates, reader_snapshot) = index.execute_query_with_masks(&plan);
        let raw_count = candidates.len();

        // Diagnostic counters for filter stages
        let mut no_path_count: usize = 0;
        let mut type_filtered_count: usize = 0;
        let mut glob_filtered_count: usize = 0;
        let mut first_glob_rejected: Option<String> = None;

        let filtered: Vec<(String, PathBuf)> = candidates
            .iter()
            .filter_map(|&fid| {
                let rel_path = match index.resolve_path(fid, &reader_snapshot) {
                    Some(p) => p,
                    None => {
                        no_path_count += 1;
                        return None;
                    }
                };
                if !type_filter.matches(&rel_path) {
                    type_filtered_count += 1;
                    return None;
                }
                if !glob_filter.is_empty() && !glob_filter.matches(&rel_path) {
                    glob_filtered_count += 1;
                    if first_glob_rejected.is_none() {
                        first_glob_rejected = Some(rel_path);
                    }
                    return None;
                }
                let full_path = index.resolve_full_path(fid, &reader_snapshot)?;
                if let Some(limit) = query_size_limit
                    && std::fs::metadata(&full_path).is_ok_and(|md| md.len() > limit)
                {
                    return None;
                }
                Some((rel_path, full_path))
            })
            .collect();

        // Log filter breakdown when raw candidates are dropped to zero
        let glob_active = !glob_filter.is_empty();
        let type_active = !type_filter.is_empty();
        if raw_count > 0 && filtered.is_empty() {
            eprintln!(
                "[trace] filter: raw={raw_count} no_path={no_path_count} \
                 type_filtered={type_filtered_count} glob_filtered={glob_filtered_count} \
                 type_active={type_active} glob_active={glob_active} \
                 sample_rejected={first_glob_rejected:?}",
            );
        }

        (filtered, raw_count)
    }; // index lock released here
    let index_ms = t_index.elapsed().as_secs_f64() * 1000.0;

    // Resolve file contents from cache (LRU) or disk.
    // Two-phase approach: read-lock for cache hits, then disk I/O outside the
    // lock, then a single write-lock to promote hits and insert misses.
    let t_resolve = Instant::now();
    let candidate_contents: Vec<(String, Arc<DecodedFile>)> = if encoding.may_differ_from_index() {
        // The cache holds text decoded with the default (BOM-sniffing) rules and
        // is shared by every client. A request that asked for a different
        // `--encoding` must neither read those entries nor write its own, or one
        // client's `-E sjis` would silently change what the next client sees.
        candidate_info
            .iter()
            .filter_map(|(rel_path, full_path)| {
                let bytes = std::fs::read(full_path).ok()?;
                Some((
                    rel_path.clone(),
                    Arc::new(DecodedFile::new(bytes, encoding)),
                ))
            })
            .collect()
    } else {
        let cache_generation = state.cache_generation.load(Ordering::SeqCst);
        // Phase 1: read-lock to find cache hits (peek avoids write-lock need)
        let mut hit_keys: Vec<String> = Vec::new();
        let mut hits: Vec<(String, Arc<DecodedFile>)> = Vec::with_capacity(candidate_info.len());
        let mut misses: Vec<(String, PathBuf)> = Vec::new();
        {
            let cache = state.cache.read().unwrap();
            for (rel_path, full_path) in &candidate_info {
                if let Some(cached) = cache.peek(rel_path) {
                    hit_keys.push(rel_path.clone());
                    hits.push((rel_path.clone(), Arc::clone(cached)));
                } else {
                    misses.push((rel_path.clone(), full_path.clone()));
                }
            }
        } // read lock released

        // Phase 2: read cache misses from disk (no lock held)
        let disk_results: Vec<(String, Arc<DecodedFile>)> = misses
            .into_iter()
            .filter_map(|(rel_path, full_path)| {
                // Lossy, like the local path: refusing invalid UTF-8 would make
                // UTF-16 and Latin-1 sources silently invisible over the server
                // while the same query works with --no-index.
                let bytes = std::fs::read(&full_path).ok()?;
                let decoded = DecodedFile::new(bytes, tgrep_core::encoding::EncodingMode::Auto);
                Some((rel_path, Arc::new(decoded)))
            })
            .collect();

        // Phase 3: single write-lock to promote hits and insert misses
        if !hit_keys.is_empty() || !disk_results.is_empty() {
            update_content_cache(state, cache_generation, &hit_keys, &disk_results);
        }

        // Combine hits and disk results, preserving candidate order
        let mut result_map: std::collections::HashMap<&str, Arc<DecodedFile>> =
            std::collections::HashMap::with_capacity(hits.len() + disk_results.len());
        for (rel_path, content) in &hits {
            result_map.insert(rel_path, Arc::clone(content));
        }
        for (rel_path, content) in &disk_results {
            result_map.insert(rel_path, Arc::clone(content));
        }
        candidate_info
            .iter()
            .filter_map(|(rel_path, _)| {
                result_map
                    .remove(rel_path.as_str())
                    .map(|c| (rel_path.clone(), c))
            })
            .collect()
    };
    let resolve_ms = t_resolve.elapsed().as_secs_f64() * 1000.0;

    // Parallel regex matching across candidate files
    let t_search = Instant::now();
    let per_file: std::result::Result<Vec<Vec<serde_json::Value>>, String> = candidate_contents
        .par_iter()
        .map(|(rel_path, content)| {
            search_file_matches(rel_path, content, &matcher, &opts).map_err(|e| format!("{e}"))
        })
        .collect();
    let matches: Vec<serde_json::Value> = match per_file {
        Ok(per_file) => per_file.into_iter().flatten().collect(),
        Err(e) => return json_rpc_error(id, -32603, &e),
    };

    let search_ms = t_search.elapsed().as_secs_f64() * 1000.0;
    let elapsed = start.elapsed();
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;

    eprintln!(
        "[trace] search: pattern={:?} case_insensitive={} raw_candidates={} candidates={} matches={} elapsed={:.1}ms (index={:.1}ms resolve={:.1}ms search={:.1}ms)",
        pattern,
        case_insensitive,
        raw_candidate_count,
        candidate_info.len(),
        matches.len(),
        elapsed_ms,
        index_ms,
        resolve_ms,
        search_ms,
    );

    let result = serde_json::json!({
        "matches": matches,
        // The number of rows in `matches`, which is not a ripgrep-style match
        // count: it spans the whole indexed tree (the client applies any
        // subdirectory argument to the reply) and includes context rows. A
        // client reporting `--stats` has to count the rows it actually prints.
        "num_matches": matches.len(),
        "elapsed_ms": elapsed_ms,
    });

    json_rpc_result(id, result)
}

fn update_content_cache(
    state: &ServerState,
    expected_generation: u64,
    hit_keys: &[String],
    disk_results: &[(String, Arc<DecodedFile>)],
) {
    let mut cache = state.cache.write().unwrap();
    // Index mutations invalidate cached paths and advance this generation under
    // the same lock. Earlier disk reads may serve their in-flight query, but
    // must not repopulate stale bytes for later searches.
    if state.cache_generation.load(Ordering::SeqCst) != expected_generation {
        return;
    }
    for key in hit_keys {
        cache.touch(key);
    }
    for (rel_path, content) in disk_results {
        if cache.peek(rel_path).is_none() {
            cache.put(rel_path.clone(), Arc::clone(content));
        }
    }
}

/// Invalidate cache entries while the caller holds the index write lock.
/// Searches acquire these locks in index-then-cache order, so the new posting
/// set and its corresponding cache generation become visible atomically.
fn invalidate_cached_paths_locked<'a>(
    state: &ServerState,
    paths: impl IntoIterator<Item = &'a str>,
) {
    let Ok(mut cache) = state.cache.write() else {
        return;
    };
    let mut invalidated = false;
    for path in paths {
        cache.pop(path);
        invalidated = true;
    }
    if invalidated {
        state.cache_generation.fetch_add(1, Ordering::SeqCst);
    }
}

fn search_file_matches(
    rel_path: &str,
    file: &DecodedFile,
    matcher: &crate::matching::SearchMatcher,
    opts: &SearchOpts,
) -> anyhow::Result<Vec<serde_json::Value>> {
    use crate::matching::FileMatches;

    let content = file.text.as_str();
    let fixups = &file.fixups;
    let match_opts = opts.match_options();

    // The client applies ripgrep's "only explicitly named binary files are
    // visible" rule, so the offset is reported here and filtered there.
    //
    // It is mapped back to the file's own bytes first: the NUL is found in the
    // repaired text, where each byte of invalid UTF-8 ahead of it has grown to
    // a three-byte U+FFFD. The client has no fixups for a server-side file, so
    // this is the only place the mapping can happen.
    let binary_offset = if opts.text {
        None
    } else {
        content
            .as_bytes()
            .iter()
            .position(|&b| b == 0)
            .map(|off| fixups.to_source_offset(off))
    };

    let found = FileMatches::find(content, matcher, &match_opts)?;
    if found.is_empty() {
        return Ok(Vec::new());
    }

    // Never stream raw binary back to the client; report a note instead, the
    // same way the local path does. Emitted for `-l`/`-c` too so the client can
    // apply ripgrep's "implicit binary files are invisible" rule uniformly.
    if let Some(off) = binary_offset {
        let marker = serde_json::json!({
            "type": "binary",
            "file": rel_path,
            "offset": off,
            // `-c` still reports a real count for binary files, so carry it
            // here instead of streaming the file's raw contents back.
            "lines": found.matched_lines(),
        });
        // `--json` reports binary matches as ordinary match events carrying a
        // `binary_offset`, so those clients ask for the lines as well.
        if !opts.binary_lines {
            return Ok(vec![marker]);
        }
        let mut results = vec![marker];
        results.extend(collect_match_rows(
            &found,
            &match_opts,
            matcher,
            rel_path,
            fixups,
            opts.detail,
            opts.positions,
        )?);
        return Ok(results);
    }

    collect_match_rows(
        &found,
        &match_opts,
        matcher,
        rel_path,
        fixups,
        opts.detail,
        opts.positions,
    )
}

/// Turn a file's matches into the protocol's `match`/`context` rows.
fn collect_match_rows(
    found: &crate::matching::FileMatches,
    match_opts: &crate::matching::MatchOptions,
    matcher: &crate::matching::SearchMatcher,
    rel_path: &str,
    fixups: &tgrep_core::encoding::LossyFixups,
    detail: bool,
    positions: bool,
) -> anyhow::Result<Vec<serde_json::Value>> {
    use crate::matching::Emit;

    let mut results = Vec::new();
    found.for_each(match_opts, matcher, |emit| -> anyhow::Result<()> {
        match emit {
            Emit::Match {
                line_number,
                content,
                columns,
                spans,
                absolute_offset,
                line_offset,
                column_shifts,
                offset_shift,
                terminator_len,
            } => {
                let (columns, offset) = crate::search::to_source_positions(
                    &columns,
                    &column_shifts,
                    absolute_offset,
                    offset_shift,
                    line_offset,
                    fixups,
                );
                let mut entry = serde_json::json!({
                    "type": "match",
                    "file": rel_path,
                    "line": line_number,
                    "content": content,
                });
                // The two arrays are what make a reply large; see `SearchOpts::detail`.
                if detail {
                    entry["spans"] =
                        serde_json::json!(spans.iter().map(|&(s, e)| [s, e]).collect::<Vec<_>>());
                    entry["columns"] = serde_json::json!(columns);
                }
                if positions {
                    entry["offset"] = serde_json::json!(offset);
                    entry["term"] = serde_json::json!(terminator_len);
                }
                results.push(entry);
            }
            Emit::Context {
                line_number,
                content,
                absolute_offset,
                terminator_len,
            } => {
                let mut entry = serde_json::json!({
                    "type": "context",
                    "file": rel_path,
                    "line": line_number,
                    "content": content,
                });
                if positions {
                    entry["offset"] = serde_json::json!(fixups.to_source_offset(absolute_offset));
                    entry["term"] = serde_json::json!(terminator_len);
                }
                results.push(entry);
            }
        }
        Ok(())
    })?;

    Ok(results)
}

fn handle_status(id: Option<serde_json::Value>, state: &ServerState) -> String {
    let index = state.index.read().unwrap();
    let cache = state.cache.read().unwrap();
    let indexing = state.indexing.load(Ordering::SeqCst);

    let result = serde_json::json!({
        "num_files": index.num_files(),
        "num_trigrams": index.num_trigrams(),
        "cache_size": cache.len(),
        "cache_capacity": CACHE_CAPACITY,
        "cache_bytes": cache.byte_len(),
        "cache_max_bytes": CACHE_MAX_BYTES,
        "watcher_active": state.watcher_active.load(std::sync::atomic::Ordering::Relaxed),
        "indexing": indexing,
        "index_progress": state.index_progress.load(std::sync::atomic::Ordering::Relaxed),
        "index_total": state.index_total.load(std::sync::atomic::Ordering::Relaxed),
    });

    json_rpc_result(id, result)
}

fn handle_reload(id: Option<serde_json::Value>, state: &Arc<ServerState>) -> String {
    let index_dir = state.index_dir.clone();
    while state.indexing.load(Ordering::SeqCst) || state.flushing.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(50));
    }
    let refresh = state.stale_refresh_lock.lock().unwrap();
    let gate = state.snapshot_gate.write().unwrap();
    {
        let _deferred = match state.deferred_events.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.indexing.store(true, Ordering::SeqCst);
    }
    let ignorecase = frozen_tracked_membership(state, &state.root);
    #[cfg(test)]
    run_stale_refresh_hook(state, StaleRefreshPhase::BeforeWalk);
    let since = SystemTime::now();

    // Rebuild the index and watcher matcher from the same immutable tracked
    // membership. The snapshot gate keeps watcher/auto-save mutations out until
    // both are published.
    let staging_dir = index_dir.join(".reload-build");
    let _ = std::fs::remove_dir_all(&staging_dir);
    let outcome = match builder::build_index_with_options_and_ignorecase(
        &state.root,
        Some(&staging_dir),
        &builder::BuildOptions {
            no_ignore: state.no_ignore,
            no_require_git: state.no_require_git,
            max_file_size: state.max_file_size,
            exclude_dirs: state.exclude_dirs.clone(),
            collect_gitignore_files: !state.no_ignore,
            ..Default::default()
        },
        ignorecase.clone(),
    ) {
        Ok(outcome) => outcome,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging_dir);
            state.indexing.store(false, Ordering::SeqCst);
            drop(gate);
            drop(refresh);
            replay_deferred_events(state, &state.root);
            schedule_pending_ignore_refresh(state);
            return json_rpc_error(id, -32000, &format!("rebuild failed: {e}"));
        }
    };
    #[cfg(test)]
    run_stale_refresh_hook(state, StaleRefreshPhase::AfterBuildBeforeStampPublish);

    // Freeze the deferred buffer through publication. Events already captured
    // have their stamps withheld; callbacks arriving now wait on this lock,
    // then observe `indexing == false` and process normally after the gate drops.
    let deferred = match state.deferred_events.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let stamps = match tgrep_core::meta::read_filestamps(&staging_dir) {
        Ok(stamps) if state.watch_enabled => {
            withhold_stamps_for_deferred_snapshot(&state.root, stamps, deferred.as_ref())
        }
        // Without a watcher there is no event to identify a file that changed
        // between extraction and the builder's later stamp collection. Publish
        // no claims from that racy window; the serialized catch-up below then
        // treats every path as changed and rebuilds it once.
        Ok(_) => std::collections::HashMap::new(),
        Err(e) => {
            eprintln!("[trace] warning: reload could not read file stamps ({e})");
            std::collections::HashMap::new()
        }
    };
    if let Err(e) = tgrep_core::meta::write_filestamps(&stamps, &staging_dir) {
        let _ = std::fs::remove_dir_all(&staging_dir);
        state.indexing.store(false, Ordering::SeqCst);
        drop(deferred);
        drop(gate);
        drop(refresh);
        replay_deferred_events(state, &state.root);
        schedule_pending_ignore_refresh(state);
        return json_rpc_error(id, -32000, &format!("could not stage rebuild stamps: {e}"));
    }
    if !publish_reloaded_index(state, &index_dir, &staging_dir, outcome.num_files) {
        state.indexing.store(false, Ordering::SeqCst);
        drop(deferred);
        drop(gate);
        drop(refresh);
        replay_deferred_events(state, &state.root);
        schedule_pending_ignore_refresh(state);
        return json_rpc_error(id, -32000, "rebuild publication failed");
    }
    let indexed = outcome.num_files as u64;
    state.index_total.store(indexed, Ordering::Relaxed);
    state.index_progress.store(indexed, Ordering::Relaxed);
    *state.file_stamps.write().unwrap() = stamps;

    let newly_watched = if state.no_ignore {
        Vec::new()
    } else {
        publish_ignore_matcher(
            state,
            &state.root,
            ignore_sources_of(
                &state.root,
                &outcome.gitignore_files,
                &outcome.ignore_files,
                state.no_require_git,
            ),
            || {
                tgrep_core::walker::build_gitignore_matcher_from_files_with_ignorecase(
                    &state.root,
                    &outcome.gitignore_files,
                    &outcome.ignore_files,
                    state.no_require_git,
                    ignorecase,
                )
            },
        )
    };
    #[cfg(test)]
    run_stale_refresh_hook(state, StaleRefreshPhase::AfterMatcherPublish);
    let mut membership_changed = tracked_membership_changed(state);
    state.indexing.store(false, Ordering::SeqCst);
    drop(deferred);
    drop(gate);
    drop(refresh);
    replay_deferred_events(state, &state.root);
    schedule_pending_ignore_refresh(state);

    if !state.watch_enabled {
        #[cfg(test)]
        if membership_changed {
            run_stale_refresh_hook(state, StaleRefreshPhase::BeforeWalk);
        }
        let caught_up = catch_up_unwatched_build(state, &state.root, &state.index_dir);
        if !caught_up {
            schedule_tracked_membership_correction(state, &state.root, membership_changed);
            return json_rpc_error(
                id,
                -32000,
                "rebuild catch-up did not complete; reload was not authoritative",
            );
        }
        membership_changed = tracked_membership_changed(state);
    }
    if state.watch_enabled {
        spawn_recovery_scan(state, &state.root, newly_watched, since);
    }
    schedule_tracked_membership_correction(state, &state.root, membership_changed);
    json_rpc_result(id, serde_json::json!({"status": "reloaded"}))
}

fn start_file_watcher(state: Arc<ServerState>, root: &Path, queue_cap: usize) -> bool {
    use std::sync::mpsc::{RecvTimeoutError, TrySendError};

    let root_path = root.to_path_buf();

    // Hand events to a worker thread instead of indexing inside the callback.
    // The callback runs on the platform's notification thread, which on Windows
    // owns a fixed-size `ReadDirectoryChangesW` buffer; doing file I/O and
    // trigram extraction there stalls it, and everything arriving meanwhile is
    // dropped by the OS with no error we can see. The queue is bounded so a
    // burst (a branch switch, a build) can't grow it without limit.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Event>(queue_cap);
    let overflowed = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let callback_overflow = Arc::clone(&overflowed);
    let callback_state = Arc::clone(&state);
    let mut watcher = match notify::recommended_watcher(
        move |result: std::result::Result<Event, notify::Error>| match result {
            Ok(event) => match tx.try_send(event) {
                Ok(()) => {}
                // Don't block the notification thread waiting for room —
                // that's the stall this hand-off exists to avoid. Drop the
                // event and note that we did; the worker reconciles with a
                // stale check, which is cheaper and more reliable than
                // trying to replay an unknown number of lost events.
                Err(TrySendError::Full(_)) => {
                    callback_overflow.store(true, Ordering::SeqCst);
                    callback_state
                        .watch_resubscribe
                        .store(true, Ordering::SeqCst);
                }
                Err(TrySendError::Disconnected(_)) => {}
            },
            // A native drop is the same loss as a full channel, and the OS
            // will not say what it lost — inotify's `IN_Q_OVERFLOW` and a
            // dropped `ReadDirectoryChangesW` buffer both arrive here with no
            // paths attached. Reconcile on them too: reporting without
            // recovering left exactly one of the two overflow paths handled,
            // and it was the one the kernel does not use.
            //
            // Surfaced as well. A dropped buffer looks exactly like "the
            // watcher stopped working" from the outside, and silence makes it
            // impossible to tell apart from a bug in our own filtering.
            Err(e) => {
                eprintln!("[trace] warning: file watcher error: {e}");
                callback_overflow.store(true, Ordering::SeqCst);
                callback_state
                    .watch_resubscribe
                    .store(true, Ordering::SeqCst);
            }
        },
    ) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[trace] warning: failed to start file watcher: {e}");
            return false;
        }
    };

    // The root is always subscribed. On a whole-subtree backend that single
    // recursive subscription is the entire watch set; on a per-directory
    // backend it is the anchor, and `sync_watch_registrations` below adds the
    // descendants the ignore rules allow.
    let root_mode = if PER_DIRECTORY_WATCHES {
        RecursiveMode::NonRecursive
    } else {
        RecursiveMode::Recursive
    };
    if let Err(e) = watcher.watch(root, root_mode) {
        eprintln!("[trace] warning: failed to watch directory: {e}");
        return false;
    }

    *state.watch_registry.lock().unwrap() = Some(WatchRegistry {
        watcher,
        root: root.to_path_buf(),
        watched: std::iter::once(root.to_path_buf()).collect(),
    });

    // Subscribing to the descendants needs the ignore matcher, and on a warm
    // start it is still being built on another thread. Skipping the sync here
    // costs nothing: events are dropped while `gitignore_pending` is set, and
    // the publish that clears it runs this same sync. Subscribing first and
    // narrowing afterwards would mean briefly holding exactly the watches this
    // is meant to avoid — on a repo big enough to exhaust the inotify budget,
    // long enough to fail.
    if state.gitignore_pending.load(Ordering::SeqCst) {
        eprintln!("[trace] watcher subscriptions deferred until the ignore matcher is ready");
    } else {
        // This can be the first subscription pass the repository ever gets:
        // the stale check runs on a thread spawned before this function, so it
        // can publish while `watch_registry` is still `None` and take no
        // subscriptions at all. Its walk is then already over by the time we
        // subscribe here, and nothing else revisits the tree until the hourly
        // reconcile — so the recovery scan is not optional on this path.
        let (newly_watched, since) = sync_watch_registrations(&state, root);
        spawn_recovery_scan(&state, root, newly_watched, since);
    }

    let worker_state = Arc::clone(&state);
    let worker_root = root_path;
    let worker_index_dir = state.index_dir.clone();
    if std::thread::Builder::new()
        .name("tgrep-watcher".into())
        .spawn(move || {
            let mut last_tracked_index_poll = Instant::now();
            loop {
                if last_tracked_index_poll.elapsed() >= TRACKED_INDEX_POLL {
                    last_tracked_index_poll = Instant::now();
                    let changed = poll_tracked_membership_changed(&worker_state);
                    if changed {
                        eprintln!(
                            "[trace] Git tracked paths changed; reconciling tracked-file exemptions"
                        );
                        worker_state
                            .ignore_rules_dirty
                            .store(true, Ordering::SeqCst);
                        schedule_ignore_rules_refresh(
                            Arc::clone(&worker_state),
                            worker_root.clone(),
                        );
                    }
                }
                match rx.recv_timeout(WATCHER_IDLE_POLL) {
                    Ok(event) => handle_fs_event(&worker_state, &worker_root, &event),
                    // A quiet interval means the burst has drained, so this is
                    // the point to repair an overflow: reconciling earlier
                    // would run a full stale check while events are still
                    // queued behind it, and repeat for each one.
                    Err(RecvTimeoutError::Timeout) => {
                        if !overflowed.load(Ordering::SeqCst) {
                            continue;
                        }
                        // An index build or a pending matcher ends with its own
                        // stale check, which reconciles the same drift. Leave
                        // the flag set and let that run instead of racing it.
                        if worker_state.indexing.load(Ordering::SeqCst)
                            || worker_state.gitignore_pending.load(Ordering::SeqCst)
                        {
                            continue;
                        }
                        overflowed.store(false, Ordering::SeqCst);
                        eprintln!(
                            "[trace] watcher queue overflowed (cap {queue_cap}); \
                             reconciling with a stale check"
                        );
                        if !background_refresh_stale(
                            &worker_state,
                            &worker_root,
                            &worker_index_dir,
                            false,
                        ) {
                            overflowed.store(true, Ordering::SeqCst);
                            thread::sleep(Duration::from_secs(1));
                        }
                    }
                    // The watcher was dropped, so the server is shutting down.
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .is_err()
    {
        eprintln!("[trace] warning: failed to start the watcher worker thread");
        *state.watch_registry.lock().unwrap() = None;
        return false;
    }

    state
        .watcher_active
        .store(true, std::sync::atomic::Ordering::Relaxed);
    eprintln!("[trace] file watcher started");

    true
}

/// Detect a tracked-file exemption change without subscribing to `.git`.
///
/// The metadata probe is enabled only while the published matcher actually
/// uses the exemption. Index metadata only decides when to reparse its paths;
/// an unchanged membership fingerprint does not schedule a repository walk.
/// Updating the observed membership before scheduling makes
/// a burst one dirty signal; the existing refresh scheduler coalesces it with
/// any ignore-source changes and serializes the full stale reconciliation.
fn tracked_membership_changed(state: &ServerState) -> bool {
    let matcher = state.gitignore.read().unwrap();
    let current = matcher
        .as_ref()
        .and_then(tgrep_core::gitignore::IgnoreMatcher::current_tracked_membership_fingerprint);
    let mut observed = state.tracked_membership.lock().unwrap();
    let changed = matches!(
        (observed.as_ref(), current.as_ref()),
        (Some(previous), Some(current)) if previous != current
    );
    *observed = current;
    changed
}

fn poll_tracked_membership_changed(state: &ServerState) -> bool {
    // A reconcile holds the write side across snapshot, walk and publication.
    // Waiting here prevents a poll from committing transient A→B→A membership.
    let _gate = state.snapshot_gate.read().unwrap();
    tracked_membership_changed(state)
}

fn frozen_tracked_membership(
    state: &ServerState,
    root: &Path,
) -> Option<Arc<tgrep_core::gitignore::CaseInsensitiveIgnore>> {
    (!state.no_ignore)
        .then(|| {
            tgrep_core::gitignore::CaseInsensitiveIgnore::frozen_snapshot(root, true, true, true)
        })
        .flatten()
        .map(Arc::new)
}

fn schedule_tracked_membership_correction(state: &Arc<ServerState>, root: &Path, changed: bool) {
    if !changed {
        return;
    }
    eprintln!("[trace] Git tracked paths changed during reconciliation; scheduling a retry");
    state.ignore_rules_dirty.store(true, Ordering::SeqCst);
    schedule_ignore_rules_refresh(Arc::clone(state), root.to_path_buf());
}

#[cfg(test)]
fn run_stale_refresh_hook(state: &ServerState, phase: StaleRefreshPhase) {
    let hook = state.stale_refresh_hook.lock().unwrap().clone();
    if let Some(hook) = hook {
        hook(phase);
    }
}

/// Decide whether the file watcher should skip a path entirely.
///
/// Mirrors the file walker's hidden-path, `--exclude` directory filtering,
/// and `.gitignore` rules so the watcher does not reindex files that the
/// initial walk would never have indexed for those reasons:
///   * any path component starting with `.` (matches `WalkBuilder::hidden(true)`),
///     including the file name itself (e.g. `.envrc`).
///   * any *ancestor directory* component matching one of the configured
///     `--exclude` names. The walker only treats `--exclude` names as
///     directory subtree filters (it skips the whole subtree when the entry
///     is a directory). A regular file whose basename happens to equal one
///     of the excluded names (e.g. a file literally called `vendor` at the
///     repo root, or `src/target`) is still indexed by the initial walk
///     and so must NOT be skipped here — otherwise the in-memory and
///     on-disk indexes would diverge.
///   * any path matched by the gitignore matcher (loaded from
///     `.gitignore` files + `.git/info/exclude` + global gitignore).
///     Without this, the watcher would happily upsert files like
///     `target/release/foo.log` or `*.tmp` that the indexer's
///     `git_ignore(true)` walk skipped, causing the in-memory and
///     on-disk indexes to diverge over time.
///
/// `rel_path` must be a forward-slash relative path (as produced by
/// `handle_fs_event`).
fn should_skip_watcher_path(
    rel_path: &str,
    exclude_dirs: &[String],
    gitignore: Option<&tgrep_core::gitignore::IgnoreMatcher>,
) -> bool {
    should_skip_watcher_entry(rel_path, exclude_dirs, gitignore, false)
}

/// [`should_skip_watcher_path`] for a path known to be a directory.
///
/// Two rules read differently for a directory. `--exclude` names apply to the
/// final segment as well, because the walker drops the whole subtree when the
/// entry it is looking at *is* the excluded directory. And the gitignore
/// matcher is told it is matching a directory, so a directory-only rule like
/// `build/` matches — as a file path, `build` does not.
fn should_skip_watcher_dir(
    rel_path: &str,
    exclude_dirs: &[String],
    gitignore: Option<&tgrep_core::gitignore::IgnoreMatcher>,
) -> bool {
    should_skip_watcher_entry(rel_path, exclude_dirs, gitignore, true)
}

fn should_skip_watcher_entry(
    rel_path: &str,
    exclude_dirs: &[String],
    gitignore: Option<&tgrep_core::gitignore::IgnoreMatcher>,
    is_dir: bool,
) -> bool {
    // Single streaming pass over path components — no Vec allocation
    // on the hot watcher path. The hidden-component check applies to
    // every segment (including the basename); the exclude_dirs check
    // applies only to *ancestor* directory components, so we test
    // "is there a next segment?" via Peekable to skip the basename.
    let mut segments = rel_path
        .split('/')
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .peekable();

    while let Some(seg) = segments.next() {
        if seg.starts_with('.') {
            return true;
        }
        // An ancestor is always a directory; the final segment is one only
        // when the caller says so.
        let segment_is_dir = segments.peek().is_some() || is_dir;
        if segment_is_dir && !exclude_dirs.is_empty() && exclude_dirs.iter().any(|d| d == seg) {
            return true;
        }
    }

    // Gitignore check (if a matcher is available).
    if let Some(gi) = gitignore {
        // For a file event we don't know whether the path is a dir, so we
        // treat it as a file. Notify usually fires per-file events anyway,
        // and gitignore rules that target dirs would have already skipped
        // the dir's contents via `matched_path_or_any_parents`.
        if gi.is_ignored(Path::new(rel_path), is_dir) {
            return true;
        }
    }

    false
}

/// Whether a changed path is an ignore-rules source, i.e. one whose contents
/// feed the matcher published in `ServerState::gitignore`.
///
/// `.gitignore` and `.ignore` are matched by file name at any depth, because
/// [`tgrep_core::gitignore::matcher_from_ignore_paths`] anchors nested files of
/// both kinds. `.ignore` must be included even though it is git-agnostic —
/// leaving it out meant a `.ignore` written while the server was live never
/// refreshed the matcher, so the watcher kept indexing files the rule excluded.
///
/// `p4ignore.ini` stays root-scoped, mirroring the walker, which only reads the
/// root-level file.
fn is_ignore_rules_file(root: &Path, path: &Path) -> bool {
    let name = path.file_name().and_then(|name| name.to_str());
    matches!(
        name,
        Some(tgrep_core::gitignore::GITIGNORE_FILENAME)
            | Some(tgrep_core::gitignore::DOT_IGNORE_FILENAME)
    ) || path == root.join(tgrep_core::gitignore::P4IGNORE_FILENAME)
}

/// Whether this platform's `notify` backend takes one OS subscription per
/// directory rather than a single recursive one for the whole tree.
///
/// inotify has no recursive mode. `RecursiveMode::Recursive` makes notify walk
/// the tree itself and spend one watch descriptor per directory, so every
/// ignored directory costs a descriptor from the per-user
/// `fs.inotify.max_user_watches` budget purely to deliver events we then throw
/// away. Worse, notify's registration loop propagates the first failure, so a
/// repo whose ignored build output exhausts the budget makes `watch()` return
/// an error and the server loses its watcher entirely.
///
/// This implementation deliberately keeps one recursive root subscription on
/// Windows (`ReadDirectoryChangesW`) and one root stream on macOS (FSEvents).
/// Ignored events are filtered after delivery there, so ignored descendants are
/// not unwatched even though the design avoids per-directory descriptor growth.
///
/// Deliberately limited to the backends we can exercise in CI. kqueue and
/// `PollWatcher` are per-path too, but nothing here builds or tests them.
const PER_DIRECTORY_WATCHES: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// Whether `path` is a directory in its own right rather than a symlink to one.
///
/// [`Path::is_dir`] follows links, so it answers "does this lead to a
/// directory", which is the wrong question here: the walker does not follow
/// symlinks, so a symlinked directory is not part of the indexed tree. Treating
/// one as a directory would subscribe to and index its target — possibly a tree
/// outside `root` entirely, and possibly a cycle.
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_dir())
}

/// Whether `path` is a directory reached from `root` without crossing a symlink.
///
/// [`is_real_dir`] only inspects the last component, which answers the wrong
/// question for a path assembled from an event: `root/a/b` is a perfectly real
/// directory while `a` is a symlink pointing anywhere on the machine. The
/// walker never descends through `a`, so nothing under it belongs to the served
/// tree, yet a `Create` for `root/a/b` would subscribe to it and enumerate and
/// index whatever is inside — a watch descriptor per directory of a tree that
/// is not ours, and file content filed under paths that do not lead to it.
///
/// `root` itself is the trusted anchor and is not tested. It may legitimately
/// be reached through a link (`tgrep serve /var/tmp/...` on macOS is the common
/// case), and refusing it would leave nothing watchable at all. This is the
/// same contract [`open_within_root`] works to: containment is established
/// relative to the root that was served, not against the real filesystem.
///
/// Component-by-component with `symlink_metadata`, so the answer is a snapshot
/// rather than a guarantee — a link swapped in afterwards is not visible here.
/// That residual window is what the post-registration re-check in
/// [`WatchRegistry::subscribe`] and `open_within_root`'s no-follow descent
/// exist to bound.
fn is_contained_dir(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let mut cursor = root.to_path_buf();
    for component in rel.components() {
        // `..` would climb back out of the tree and `.` cannot appear in a path
        // built from an event; anything but a plain name is not a descent.
        let std::path::Component::Normal(name) = component else {
            return false;
        };
        cursor.push(name);
        if !is_real_dir(&cursor) {
            return false;
        }
    }
    true
}

/// The watcher plus the set of directories it is currently subscribed to.
///
/// Only meaningful when [`PER_DIRECTORY_WATCHES`] is true; elsewhere `watched`
/// holds just the root, which is subscribed recursively.
struct WatchRegistry {
    watcher: RecommendedWatcher,
    /// The served root. Subscriptions are only ever taken for directories
    /// reachable from it without crossing a symlink; see
    /// [`WatchRegistry::contained`].
    root: PathBuf,
    watched: std::collections::HashSet<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TraversalCompleteness {
    Complete,
    Incomplete,
}

/// Consume one overflow repair request only when this traversal can complete it.
///
/// Clearing before the sync means an overflow that arrives during a complete
/// pass sets the flag for another pass. Re-arming an incomplete pass preserves
/// the original request without overwriting a concurrent `true`.
fn take_force_resubscribe(
    pending: &std::sync::atomic::AtomicBool,
    completeness: TraversalCompleteness,
) -> bool {
    let force = pending.swap(false, Ordering::SeqCst);
    if force && completeness == TraversalCompleteness::Incomplete {
        pending.store(true, Ordering::SeqCst);
        return false;
    }
    force
}

struct WatchableDirs {
    dirs: std::collections::HashSet<PathBuf>,
    completeness: TraversalCompleteness,
}

impl WatchRegistry {
    /// Subscribe to every directory in `desired` that is not already
    /// subscribed, leaving existing subscriptions alone.
    ///
    /// Returns the directories that were newly subscribed to. A single
    /// directory that cannot be subscribed is reported and skipped rather than
    /// failing the whole call: the watcher is still useful for everything
    /// else, and giving up on the entire tree is exactly the failure mode this
    /// registration exists to avoid.
    fn add_all<'a>(&mut self, desired: impl IntoIterator<Item = &'a PathBuf>) -> Vec<PathBuf> {
        self.subscribe(desired, false)
    }

    /// Subscribe to every directory in `dirs`, re-issuing the subscription even
    /// for ones already recorded as watched.
    ///
    /// For directories that have just appeared, where membership in `watched`
    /// proves nothing. The kernel drops an inotify watch by itself when its
    /// directory is deleted or moved away, and nothing tells us the descriptor
    /// is gone — so a path recreated at the same location would look
    /// subscribed while receiving no events at all. [`Self::add_all`] would
    /// skip it, and so would every later [`Self::sync`], since the path is in
    /// `desired` *and* in `watched`: the entry stays poisoned until the server
    /// restarts. Re-adding is cheap and idempotent (`inotify_add_watch`
    /// returns the existing descriptor), so the doubt is worth paying for.
    ///
    /// Returns only the directories that were not previously recorded, so a
    /// caller's notion of "newly watched" keeps its meaning.
    fn resubscribe_all<'a>(&mut self, dirs: impl IntoIterator<Item = &'a PathBuf>) -> Vec<PathBuf> {
        self.subscribe(dirs, true)
    }

    /// Whether `dir` is still a directory the served tree actually contains.
    ///
    /// Every entry in `watched` was checked by this method before it went in,
    /// so a directory whose parent is already watched (or is the root) inherits
    /// that proof and only its own last component needs testing. That short
    /// circuit is what keeps the startup sync affordable: the full walk in
    /// [`is_contained_dir`] costs one `symlink_metadata` per level, and paying
    /// it for each of forty thousand directories at depth ten would be four
    /// hundred thousand syscalls to re-derive what the previous entry proved.
    /// The sync feeds directories shallowest-first for exactly this reason, so
    /// the cheap path is the one nearly every call takes; anything arriving
    /// out of order still gets the full walk and the right answer.
    fn contained(&self, dir: &Path) -> bool {
        match dir.parent() {
            Some(parent) if parent == self.root || self.watched.contains(parent) => {
                is_real_dir(dir)
            }
            _ => is_contained_dir(&self.root, dir),
        }
    }

    fn subscribe<'a>(
        &mut self,
        desired: impl IntoIterator<Item = &'a PathBuf>,
        force: bool,
    ) -> Vec<PathBuf> {
        let mut added = Vec::new();
        let mut failures = 0;
        // Iterating `desired` and testing membership is deliberate: a
        // `difference` would be proportional to the whole watched set, and
        // this runs per newly created directory on repositories where that set
        // is tens of thousands of entries.
        for dir in desired {
            let known = self.watched.contains(dir);
            if known && !force {
                continue;
            }
            match self.watcher.watch(dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    // notify's inotify backend registers without
                    // `IN_DONT_FOLLOW`, so the descriptor lands on whatever the
                    // name resolves to at that instant — and the no-follow
                    // check that qualified this directory happened earlier, in
                    // the walk. A checkout or a rename can replace it with a
                    // symlink in between, leaving the descriptor watching an
                    // inode outside the root while `watched` records an
                    // in-root name as covered.
                    //
                    // Re-checking after the fact catches that: if the name is
                    // no longer a real directory, the registration is undone
                    // and the entry is not recorded, so a later `sync` retries
                    // it rather than treating a poisoned subscription as live.
                    //
                    // This narrows the window rather than closing it — notify
                    // takes a path, not a handle, so a swap that is reverted
                    // before this check is undetectable through its API. What
                    // that costs is bounded: it is missed *events* on a real
                    // directory, which the periodic reconcile picks up, and
                    // never misplaced content, since `open_within_root`
                    // establishes containment from the handle it reads.
                    if !self.contained(dir) {
                        let _ = self.watcher.unwatch(dir);
                        if known {
                            self.watched.remove(dir);
                        }
                        continue;
                    }
                    if !known {
                        self.watched.insert(dir.clone());
                        added.push(dir.clone());
                    }
                }
                Err(e) => {
                    // A forced re-add that fails means the directory is gone
                    // again; drop the entry so a later attempt can retry it
                    // rather than trusting a descriptor that does not exist.
                    if known {
                        self.watched.remove(dir);
                    }
                    // One line per call, not per directory: exhausting the
                    // inotify budget fails thousands of these at once.
                    if failures == 0 {
                        eprintln!(
                            "[trace] warning: could not watch {}: {e} \
                             (continuing with the directories that succeeded)",
                            dir.display()
                        );
                    }
                    failures += 1;
                }
            }
        }
        if failures > 1 {
            eprintln!("[trace] warning: {failures} directories could not be watched");
        }
        added
    }

    /// Whether `path` is already subscribed.
    ///
    /// For deciding whether a directory found during a recovery scan is one the
    /// sync already knew about or one that appeared after it — the latter has
    /// to be picked up explicitly, since a non-recursive subscription on its
    /// parent says nothing about it.
    fn is_watched(&self, path: &Path) -> bool {
        self.watched.contains(path)
    }

    /// Drop a path that no longer exists from the subscription set.
    ///
    /// The kernel has already released the descriptor if the directory was
    /// deleted; the `unwatch` is for the moved-away case, where it is still
    /// live and now pointing outside the tree. What matters either way is
    /// clearing `watched`, so that if the path comes back, it is treated as
    /// the new directory it is instead of an already-subscribed one.
    ///
    /// Cheap by design: one hash lookup per removal event, because deleting a
    /// tree delivers one event per directory in it and anything proportional
    /// to the whole watched set would turn that into quadratic work.
    /// Descendants left behind by a move are pruned by the next
    /// [`Self::sync`], which no longer finds them under the root.
    fn forget(&mut self, path: &Path) {
        if self.watched.remove(path) {
            let _ = self.watcher.unwatch(path);
        }
    }

    /// Bring the subscription set in line with `desired`, subscribing to
    /// directories that are newly relevant and dropping ones that are not.
    ///
    /// `force` re-issues the subscription for directories already recorded as
    /// watched. Only needed after events were lost: a directory removal that
    /// never arrived leaves the kernel's descriptor gone and this registry's
    /// entry intact, and a path recreated there is then in `desired` *and* in
    /// `watched`, so an ordinary sync skips it forever. Off by default because
    /// re-registering costs a syscall per directory, and a monorepo reconcile
    /// would pay forty thousand of them for a doubt only overflow raises.
    ///
    /// Returns `(added, removed)`. Only for a set that describes the whole
    /// tree. `completeness` makes its authority explicit: a complete set prunes
    /// anything absent from `desired`, while an incomplete traversal only adds
    /// directories it proved desirable. To subscribe to a subtree without
    /// disturbing the rest, use [`Self::add_all`].
    fn sync(
        &mut self,
        desired: &std::collections::HashSet<PathBuf>,
        completeness: TraversalCompleteness,
        force: bool,
    ) -> (Vec<PathBuf>, usize) {
        let mut removed = 0;
        if completeness == TraversalCompleteness::Complete {
            let stale: Vec<PathBuf> = self.watched.difference(desired).cloned().collect();
            for dir in stale {
                // Best effort. inotify drops a descriptor by itself when the
                // directory is deleted, so "not found" is an expected outcome
                // here, not an error worth reporting.
                let _ = self.watcher.unwatch(&dir);
                self.watched.remove(&dir);
                removed += 1;
            }
        }

        // Shallowest first, because `desired` is a `HashSet` and hands its
        // contents out in whatever order hashing produced. [`Self::contained`]
        // establishes containment cheaply by leaning on the parent already
        // being watched; a child that arrives before its parent gets no such
        // proof and walks every ancestor instead. Unordered, that is the
        // common case rather than the exception, and it turns the startup
        // sync from one `symlink_metadata` per directory into one per level
        // per directory — on a monorepo, hundreds of thousands of syscalls in
        // the path that exists to make startup cheap. Sorting by depth is
        // enough: a parent is always strictly shallower than its children.
        let mut ordered: Vec<&PathBuf> = desired.iter().collect();
        ordered.sort_by_key(|dir| dir.components().count());

        let added = if force {
            self.resubscribe_all(ordered)
        } else {
            self.add_all(ordered)
        };
        (added, removed)
    }
}

/// Every directory at or below `start` whose contents the watcher needs to
/// hear about.
///
/// A per-directory subscription reports the files directly inside it, so the
/// set is "`start`, plus every descendant directory the indexer would walk
/// into". Ignored directories are pruned along with their subtrees, which is
/// the whole point: the tree under `target/` or `node_modules/` is usually
/// most of the directories in a repo.
///
/// `root` is the repository root and is only used to build the relative paths
/// the ignore rules are written against; `start` is where the walk begins.
/// They differ when a subtree that appeared at runtime is being subscribed,
/// and conflating them would match every rule at the wrong anchor.
///
/// `start` itself is always included — callers are responsible for not asking
/// about a directory that is ignored.
///
/// Symlinked directories are not descended into, matching the walker (the
/// `ignore` crate does not follow links by default). That also keeps a
/// symlink cycle from turning this into an infinite walk.
///
/// Any failed listing, entry read, or type query marks the result incomplete.
/// Its proven directories remain useful for adding subscriptions, but its
/// omissions must not be used to remove existing ones.
fn watchable_dirs(
    root: &Path,
    start: &Path,
    exclude_dirs: &[String],
    gitignore: Option<&tgrep_core::gitignore::IgnoreMatcher>,
) -> WatchableDirs {
    let mut found = std::collections::HashSet::new();
    let mut completeness = TraversalCompleteness::Complete;
    found.insert(start.to_path_buf());

    let mut stack = vec![start.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            // An unreadable directory is not a reason to abandon the rest of
            // the tree, but it makes the result unsafe for pruning.
            completeness = TraversalCompleteness::Incomplete;
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                completeness = TraversalCompleteness::Incomplete;
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                completeness = TraversalCompleteness::Incomplete;
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(root) else {
                completeness = TraversalCompleteness::Incomplete;
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if should_skip_watcher_dir(&rel, exclude_dirs, gitignore) {
                continue;
            }
            stack.push(path.clone());
            found.insert(path);
        }
    }
    WatchableDirs {
        dirs: found,
        completeness,
    }
}

/// The directories that must be subscribed to for the ignore sources
/// themselves to be observable, beyond the ones the rules allow.
///
/// A `.gitignore` symlinked to `build/shared-rules` contributes the *target's*
/// contents, and [`handle_fs_event`] already recognises an event naming that
/// target rather than a name rules usually go by. But only if one arrives: on a
/// per-directory backend nothing subscribes to `build/` when the rules hide it,
/// so the edit that changes what the matcher enforces produces no event at all,
/// and the matcher stays stale until the hourly reconcile — the one case where
/// the source of the rules is invisible to the rules' own watcher.
///
/// One watch on the target's own directory, not its subtree: this is about
/// seeing a single file that the matcher was built from, not about indexing
/// anything under it. `should_skip_watcher_path` still discards everything else
/// delivered from there, and the target itself is matched by path against the
/// recorded stamps before any of that filtering runs.
///
/// Targets outside `root` are deliberately not covered. Watching them would
/// mean subscribing outside the tree the server was asked to serve, and the
/// periodic reconcile remains the backstop there.
fn ignore_target_dirs(root: &Path, sources: &[PathBuf]) -> std::collections::HashSet<PathBuf> {
    let mut dirs = std::collections::HashSet::new();
    let Ok(canonical_root) = std::fs::canonicalize(root) else {
        return dirs;
    };
    for source in sources {
        if !std::fs::symlink_metadata(source).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        let Ok(target) = std::fs::canonicalize(source) else {
            continue;
        };
        let Ok(rel) = target.strip_prefix(&canonical_root) else {
            continue;
        };
        // Re-anchored on `root` as given rather than kept canonical: the
        // registry compares paths literally, and a `\\?\` or `/private` prefix
        // would register a second subscription for a directory already watched.
        if let Some(parent) = root.join(rel).parent() {
            dirs.insert(parent.to_path_buf());
        }
    }
    dirs
}

/// Recompute the watcher's subscriptions against the ignore rules in force.
///
/// Called when the watcher starts and every time the ignore matcher is
/// published, so relaxing a rule subscribes to the tree it used to hide and
/// tightening one drops it.
///
/// Returns the directories that were newly subscribed to, and the moment the
/// walk behind that decision began. Until a directory is subscribed it cannot
/// report anything, so a file written to one of these between the walk and the
/// subscription is in neither the walk's results nor any event. The caller is
/// expected to hand both to [`reindex_files_in`] once its own bookkeeping is
/// settled; the timestamp bounds that window, which is what lets the scan tell
/// an ignore-rules file that landed inside it from the thousands that were
/// already there and are already reflected in the matcher.
fn sync_watch_registrations(state: &ServerState, root: &Path) -> (Vec<PathBuf>, SystemTime) {
    // Before the early returns as well as the walk: a caller that gets no
    // directories back still gets a usable bound.
    let since = SystemTime::now();
    if !PER_DIRECTORY_WATCHES {
        return (Vec::new(), since);
    }
    let mut registry = state.watch_registry.lock().unwrap();
    let Some(registry) = registry.as_mut() else {
        // The watcher has not started yet. It syncs once as it comes up, so
        // there is nothing to do and nothing to remember.
        return (Vec::new(), since);
    };

    let start = Instant::now();
    let mut desired = {
        let gitignore = state.gitignore.read().unwrap();
        watchable_dirs(root, root, &state.exclude_dirs, gitignore.as_ref())
    };
    if !state.no_ignore {
        let sources = state.ignore_sources.read().unwrap();
        desired.dirs.extend(ignore_target_dirs(root, &sources));
    }
    // An incomplete traversal cannot prove that omitted recorded watches are
    // live, so it does not get to consume the overflow repair request.
    let force = take_force_resubscribe(&state.watch_resubscribe, desired.completeness);
    let (added, removed) = registry.sync(&desired.dirs, desired.completeness, force);
    let total = registry.watched.len();
    if !added.is_empty() || removed > 0 {
        eprintln!(
            "[trace] watcher subscriptions: {total} directories \
             (+{}, -{removed}) in {:.1}ms",
            added.len(),
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    (added, since)
}

/// The first ignore-rules file in `dirs` that the published matcher did not
/// read, that has been replaced since it did, or that has been edited since
/// `since`.
///
/// Probing by name rather than inspecting listings, for three reasons. It is
/// how the walker itself discovers these files, so the two agree by
/// construction. `Path::is_file` follows symlinks, so a symlinked `.gitignore`
/// — which carries rules exactly like a real one — is seen, where
/// `DirEntry::file_type` does not resolve it and the entry gets dropped as "not
/// a regular file". And it is ordering-independent: `read_dir` offers no
/// ordering, and on macOS `.gitignore` routinely comes back *after* its
/// siblings, so a per-entry check indexes part of a directory under the stale
/// rules before it ever reaches the file that changes them. Answering for the
/// whole scan up front closes that window across directories as well.
///
/// Three tests, because none alone is enough.
///
/// Absence from the published sources is the exact question for an arrival —
/// this file did not feed the matcher in force — and it catches one however old
/// the file says it is, which matters because `git checkout`, `tar -x` and
/// `rsync -a` all restore mtimes from what they unpack and would sail past a
/// recency test.
///
/// A stamp mismatch answers the same question for a file that was *already* a
/// source: a pathname proves nothing about contents, and neither does
/// metadata. `rsync -a` and `tar -x` preserve mtime, and two different sets of
/// rules are easily the same length, so the comparison is against a hash of
/// what was actually read.
///
/// The mtime window then covers the gap the digests cannot: they are taken
/// when the matcher is published, which is after the builder read these files,
/// so a write landing between the two is recorded as if it had been read.
///
/// That window is widened by [`MTIME_GRANULARITY`] at the near end, because
/// `since` is a wall-clock instant with nanosecond precision and an mtime is
/// not. HFS+ and ext3 store whole seconds, FAT-derived filesystems two, so a
/// write that happens after `since` can be stamped before it and read as
/// historical. Over-triggering costs one rewalk that finds nothing; the slack
/// is bounded, so a file whose mtime keeps qualifying stops doing so as later
/// scans take later timestamps.
///
/// The far end is bounded too. On a network mount whose server clock runs
/// ahead of ours, every recently touched file carries a future mtime and would
/// pass a one-sided test — on every scan, including the one at the end of the
/// refresh this schedules, which walks the whole repository and then arms the
/// next. Treating a future mtime as skew rather than as an edit keeps that
/// loop closed.
fn changed_ignore_rules_in(
    root: &Path,
    dirs: &[PathBuf],
    known_sources: &IgnoreStamps,
    since: SystemTime,
) -> Option<(String, &'static str)> {
    let since = since.checked_sub(MTIME_GRANULARITY).unwrap_or(since);
    let mut probed: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for dir in dirs {
        let mut candidates = vec![
            dir.join(tgrep_core::gitignore::GITIGNORE_FILENAME),
            dir.join(tgrep_core::gitignore::DOT_IGNORE_FILENAME),
        ];
        // Root-scoped, mirroring the walker, which only reads the root file.
        if dir == root {
            candidates.push(root.join(tgrep_core::gitignore::P4IGNORE_FILENAME));
        }
        for candidate in candidates {
            if !probed.insert(candidate.clone()) || !candidate.is_file() {
                continue;
            }
            let Ok(rel) = candidate.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            let Some(read_as) = known_sources.get(&rel) else {
                return Some((rel, "not a known source"));
            };
            if ignore_digest_of(&candidate).as_ref() != Some(read_as) {
                return Some((rel, "not the file the matcher read"));
            }
            // Follows links, matching how the digest was taken.
            let Ok(meta) = std::fs::metadata(&candidate) else {
                continue;
            };
            if meta
                .modified()
                .is_ok_and(|m| m >= since && m <= SystemTime::now())
            {
                return Some((rel, "modified"));
            }
        }
    }
    None
}

/// Re-check the files directly inside `dirs`, indexing the ones that changed,
/// dropping the ones that are gone, and subscribing to subdirectories that
/// appeared while the subscriptions were being established.
/// Used to close the gap between a walk and the subscriptions that follow it:
/// [`reindex_file`] compares stamps first, so for a tree that did not change
/// under us this costs one `metadata` call per file and indexes nothing.
///
/// `since` is when the walk behind `dirs` began — the start of the window this
/// is closing. It is only consulted for ignore-rules files, where "did this
/// arrive after the matcher was decided" cannot be answered from the stamps:
/// the dot-prefixed ones are hidden, so they are never indexed and never have
/// one. The opposite case — one that was *deleted* in the window — is handled
/// separately, from `state.ignore_sources`, since a deleted file leaves nothing
/// to stat.
///
/// Callers must already hold `snapshot_gate`, and `state.file_stamps` must
/// already describe the index as published — a merge that replaces the stamps
/// afterwards would both discard what this records and make every file here
/// look changed.
fn reindex_files_in(state: &Arc<ServerState>, root: &Path, dirs: &[PathBuf], since: SystemTime) {
    if dirs.is_empty() {
        return;
    }
    let start = Instant::now();

    // An ignore file that was deleted during the window leaves nothing to stat,
    // so no test over what is on disk can see it. It is also the more damaging
    // direction: rules that no longer have a source keep being enforced, so the
    // subtree they hide stays unsubscribed and unindexed until an unrelated
    // rebuild happens along. Checking the sources the published matcher was
    // built from catches it at one stat apiece, once per scan.
    let known_sources: IgnoreStamps = if state.no_ignore {
        IgnoreStamps::new()
    } else {
        let vanished = {
            let sources = state.ignore_sources.read().unwrap();
            // `is_file` follows links, matching how the walker collected these
            // (`ignore_files_in` qualifies candidates with `Path::is_file`) — a
            // symlinked source whose target is gone has stopped contributing
            // rules just as surely as a deleted one.
            //
            // `is_file` rather than `exists`: a source replaced by a directory,
            // a FIFO or a socket still exists, but the walker would no longer
            // collect it and a rebuild would no longer read it. Testing only
            // for absence leaves the matcher enforcing rules from a file that
            // is not a file any more, with nothing else able to notice — the
            // digest check below only runs for candidates the scan walks past,
            // and a rule file that has become a directory is not one of them.
            sources.iter().find(|p| !p.is_file()).cloned()
        };
        if let Some(gone) = vanished {
            state.ignore_rules_dirty.store(true, Ordering::SeqCst);
            schedule_ignore_rules_refresh(Arc::clone(state), root.to_path_buf());
            eprintln!(
                "[trace] watcher: ignore rules source {} is gone or no longer a file; \
                 deferring to a refresh",
                gone.display()
            );
            return;
        }
        state.ignore_source_stamps.read().unwrap().clone()
    };

    // Before a single file is indexed: an ignore-rules file that landed in this
    // window was not seen by the walk that built the matcher in force, so every
    // file in this scan would be judged by rules that do not know about it, and
    // whatever was wrongly indexed would stay until something touched it again.
    if !state.no_ignore
        && let Some((rel, why)) = changed_ignore_rules_in(root, dirs, &known_sources, since)
    {
        // Abandon the scan: the refresh rewalks and republishes, which covers
        // these directories properly.
        state.ignore_rules_dirty.store(true, Ordering::SeqCst);
        schedule_ignore_rules_refresh(Arc::clone(state), root.to_path_buf());
        eprintln!(
            "[trace] watcher: ignore rules changed during recovery ({rel}, {why}); \
             deferring to a refresh"
        );
        return;
    }

    // Directories whose listing succeeded, and the files those listings
    // contained, for the removal sweep at the end. Directory names are kept
    // apart from the files: a directory that vanished with its contents leaves
    // indexed paths whose own parent was never enumerated, and its absence
    // from its parent's listing is the only evidence there is.
    let mut swept: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut present: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut present_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut unreadable_dirs: Vec<String> = Vec::new();

    for dir in dirs {
        let Ok(rel_dir) = dir.strip_prefix(root) else {
            continue;
        };
        let rel_dir = rel_dir.to_string_lossy().replace('\\', "/");
        let Ok(entries) = std::fs::read_dir(dir) else {
            // No listing means no evidence, and the sweep below must not treat
            // silence as absence. Whether the directory is gone or merely
            // unreadable is decided later, from its parent's listing.
            unreadable_dirs.push(rel_dir);
            continue;
        };
        let mut subdirs: Vec<PathBuf> = Vec::new();
        // A per-entry error is as much a gap in the evidence as a failed
        // listing: the name it would have yielded is simply absent from
        // `present`, and the sweep below would read that as a deletion. The
        // entry is skipped either way, but the directory then does not get to
        // claim it was enumerated.
        let mut listing_complete = true;
        for entry in entries {
            let Ok(entry) = entry else {
                listing_complete = false;
                continue;
            };
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            let Ok(file_type) = entry.file_type() else {
                // Unclassifiable, so nothing can be concluded about it —
                // least of all that it is gone.
                present.insert(rel);
                continue;
            };
            // `DirEntry::file_type` does not follow symlinks, so a symlinked
            // file or directory is neither indexed nor descended into, which
            // is what the walker does with `follow_links(false)`.
            if file_type.is_dir() {
                present_dirs.insert(rel);
                subdirs.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            present.insert(rel.clone());

            let skip = {
                let gitignore = state.gitignore.read().unwrap();
                should_skip_watcher_path(&rel, &state.exclude_dirs, gitignore.as_ref())
            };
            if !skip {
                reindex_file(state, &path, &rel, false);
            }
        }
        if listing_complete {
            swept.insert(rel_dir);
        }

        // A directory created in the same window is in neither `dirs` (the
        // walk did not see it) nor any event (its parent's subscription is
        // non-recursive, so notify does not extend to it), and would stay
        // invisible until the hourly reconcile.
        //
        // Only the ones not already subscribed: at startup `dirs` is every
        // directory in the repository and each one is a subdirectory of
        // another, so descending into all of them would re-walk the tree once
        // per level. The membership test reduces that to a hash lookup apiece.
        if !subdirs.is_empty() {
            let unwatched: Vec<PathBuf> = {
                let mut registry = state.watch_registry.lock().unwrap();
                match registry.as_mut() {
                    // Filtered under the lock but subscribed outside it:
                    // `watch_new_subtree` takes the same lock, and it is not
                    // reentrant.
                    Some(registry) => subdirs
                        .into_iter()
                        .filter(|p| !registry.is_watched(p))
                        .collect(),
                    None => Vec::new(),
                }
            };
            for subdir in &unwatched {
                watch_new_subtree(state, root, subdir);
            }
        }
    }

    // A directory that could not be listed is either gone or merely
    // unreadable, and only its parent's listing can tell the two apart. The
    // ones proven absent stand in for every file under them: those files have
    // a parent nothing enumerated, so the per-directory evidence above says
    // nothing about them at all, and a subtree deleted or moved away in this
    // window would keep answering searches until the hourly reconcile.
    let vanished_dirs: std::collections::HashSet<String> = unreadable_dirs
        .into_iter()
        .filter(|rel| {
            let parent = rel.rsplit_once('/').map_or("", |(dir, _)| dir);
            swept.contains(parent) && !present_dirs.contains(rel) && !present.contains(rel)
        })
        .collect();

    sweep_removed_files(state, &swept, &present, &vanished_dirs);

    eprintln!(
        "[trace] watcher: rechecked {} newly watched directories in {:.1}ms",
        dirs.len(),
        start.elapsed().as_secs_f64() * 1000.0
    );
}

/// Drop index entries for files that were deleted while subscriptions were
/// being established.
///
/// The counterpart to the indexing pass in [`reindex_files_in`]: a file removed
/// in that window produced no event either, and unlike a modified one nothing
/// later brings it back to the watcher's attention, so it keeps answering
/// searches until the hourly reconcile.
///
/// `swept` holds the relative directories whose listing succeeded — a failed
/// `read_dir` proves nothing and must not be read as an empty directory — and
/// `present` every file those listings contained, regardless of ignore rules or
/// eligibility. Filtering `present` would delete entries for files that are
/// still on disk and were indexed under a laxer configuration.
///
/// `vanished_dirs` are directories that could not be listed *and* were absent
/// from a parent listing that did succeed. Their descendants cannot be judged
/// by `swept` and `present`, which only speak for a file's immediate parent: a
/// directory deleted or moved away whole leaves indexed paths whose parent was
/// never enumerated, and no event names them either — a removal delivers one
/// event for the directory, and a move away delivers nothing at all for what
/// was inside it. Anything under one of these is swept on the strength of the
/// directory's absence.
///
/// Candidates come from everything that can answer a content or filename
/// query, not from `file_stamps` alone. A stamp is not a precondition for
/// being searchable:
/// `filestamps.json` is optional by design — missing or unreadable leaves the
/// map empty, and a build that predates a given file's stamp leaves it partial
/// — while the reader still holds that file's content. Sweeping only what has
/// a stamp would then delete nothing at all, and the deleted files would keep
/// answering searches until the hourly reconcile.
///
/// Reader paths already hidden by a tombstone are skipped. `delete_file`
/// tombstones unconditionally and counts a mutation for it, so re-deleting
/// them would make every scan over a directory with deletions in it look like
/// fresh churn and pull flushes forward for no reason.
///
/// The caller must already hold `snapshot_gate`.
fn sweep_removed_files(
    state: &ServerState,
    swept: &std::collections::HashSet<String>,
    present: &std::collections::HashSet<String>,
    vanished_dirs: &std::collections::HashSet<String>,
) {
    if swept.is_empty() {
        return;
    }
    // One pass per source rather than a lookup per swept directory: at startup
    // both sides of this span the whole repository, and anything proportional
    // to their product would not finish.
    let missing = |rel: &str| {
        let parent = rel.rsplit_once('/').map_or("", |(dir, _)| dir);
        if swept.contains(parent) && !present.contains(rel) {
            return true;
        }
        // Walking ancestors costs one lookup per level, so it is done only
        // when something actually vanished — which is rare, while this closure
        // runs once per indexed path in the repository.
        if vanished_dirs.is_empty() {
            return false;
        }
        let mut ancestor = parent;
        loop {
            if vanished_dirs.contains(ancestor) {
                return true;
            }
            match ancestor.rsplit_once('/') {
                Some((next, _)) => ancestor = next,
                None => return false,
            }
        }
    };
    let mut gone: std::collections::HashSet<String> = {
        let stamps = state.file_stamps.read().unwrap();
        stamps.keys().filter(|rel| missing(rel)).cloned().collect()
    };
    {
        let index = state.index.read().unwrap();
        gone.extend(index.reader_paths_matching(|rel| missing(rel) && !index.live.is_deleted(rel)));
        gone.extend(
            index
                .live
                .overlay_paths()
                .into_iter()
                .filter(|rel| missing(rel)),
        );
    }
    gone.extend(
        state
            .filename_extra_paths
            .read()
            .unwrap()
            .iter()
            .filter(|rel| missing(rel))
            .cloned(),
    );
    if gone.is_empty() {
        return;
    }
    // Per candidate, under the same lock `reindex_file` takes, and re-checked
    // against the filesystem rather than against the listing that produced
    // `gone`. That listing is from earlier in the scan; a file recreated since
    // then has already had its create event consumed by the watcher, so
    // deleting it here on the strength of a stale observation would lose it
    // until the next reconcile — and there is nothing left to replay.
    //
    // The lock is what makes the recheck mean anything: without it the file
    // could be reindexed between the check and the delete, which is the same
    // bug one instruction later.
    let mut dropped = 0usize;
    for rel in &gone {
        let _reindex = lock_reindex(state);
        // Through the same containment contract `reindex_file` opens under,
        // not a bare `symlink_metadata`. That call refuses to follow only the
        // *final* component: a directory that vanished and came back as a link
        // to somewhere else makes `root/gone-dir/a.rs` resolve to a perfectly
        // ordinary file outside the tree, and reading that as "it is back"
        // keeps the stale in-root entry forever — nothing under a link is
        // walked or watched, so no later event corrects it.
        //
        // Transient failures preserve, as they do in `reindex_file`: a
        // descriptor limit or a sharing violation says nothing about whether
        // the path belongs in the index, and the next reconcile will ask again.
        match open_within_root(&state.root, &state.root.join(rel)) {
            Ok(file) => match file.metadata() {
                // Back, and reachable without leaving the tree.
                Ok(meta) if meta.file_type().is_file() => continue,
                // There, but not something the index should hold.
                Ok(_) => {}
                Err(_) => continue,
            },
            Err(e) if proves_ineligible(&e) => {}
            Err(_) => continue,
        }
        drop_indexed_file(state, rel, "removed during watcher recovery");
        dropped += 1;
    }
    if dropped > 0 {
        eprintln!(
            "[trace] watcher: dropped {dropped} file(s) removed while subscriptions were \
             being established"
        );
    }
}

/// Index a directory that has just appeared and anything already inside it,
/// subscribing to it as well on a per-directory backend.
///
/// Two separate reasons to be here, and they apply on different platforms.
/// Non-recursive subscriptions are not extended by notify — it only auto-adds
/// watches beneath a watch that was registered as recursive — so on Linux a new
/// directory has to be subscribed here or its contents are invisible. And on
/// every backend, a directory that arrives already populated (a `mv` from
/// outside the root, a checkout, an unpacked archive) reports itself and
/// nothing else: the kernel does not enumerate what moved in. Both leave files
/// that appear in no walk and no event.
///
/// Files that landed between the directory's creation and its subscription
/// would be missed by definition, so the same pass indexes what it finds.
/// The caller must already hold `snapshot_gate`.
///
/// The descent subscribes to each level *before* reading it. Reading first
/// leaves a window in which a child created in between is in neither place:
/// not in what this pass enumerates, and not yet able to report itself. That
/// window is small but it is exactly the one a checkout or a build fills, and
/// anything lost in it stays invisible until the hourly reconcile.
fn watch_new_subtree(state: &Arc<ServerState>, root: &Path, dir: &Path) {
    // `is_dir` follows symlinks; the walker does not. Refuse a symlinked
    // directory here so we never subscribe to, or index, a tree the indexer
    // would not have walked into — and check every level, not just the last,
    // since a real directory inside a symlinked one is just as far outside the
    // served tree as the link itself.
    if !is_contained_dir(root, dir) {
        return;
    }
    let Ok(rel_dir) = dir.strip_prefix(root) else {
        return;
    };
    let rel_dir = rel_dir.to_string_lossy().replace('\\', "/");
    // The event that brought us here was filtered with file semantics, so a
    // `build/`-style rule that only ever matches directories has not been
    // applied to this path yet. Re-check before subscribing to it.
    if !rel_dir.is_empty() {
        let gitignore = state.gitignore.read().unwrap();
        if should_skip_watcher_dir(&rel_dir, &state.exclude_dirs, gitignore.as_ref()) {
            return;
        }
    }

    let mut level = vec![dir.to_path_buf()];
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    let mut found_ignore_rules = false;

    'descend: while !level.is_empty() {
        // Subscribing is per-directory work. On a recursive backend the root's
        // one subscription already covers everything below it, and taking a
        // watch per directory there would be the exhaustion this whole pass
        // exists to avoid — so only the enumeration below runs on those
        // platforms.
        if PER_DIRECTORY_WATCHES {
            let mut registry = state.watch_registry.lock().unwrap();
            let Some(registry) = registry.as_mut() else {
                return;
            };
            // Additive, not a sync: this covers only the new subtree, and
            // `sync` would read everything outside it as stale and unsubscribe
            // from the entire rest of the repository. Staying additive also
            // matters at scale — a monorepo can hold tens of thousands of
            // watched directories, and materialising a union for every newly
            // created directory would be quadratic over a checkout.
            //
            // Forced, because these directories have just appeared: a path
            // recreated where a watched one used to be is still recorded as
            // watched, but the kernel dropped its descriptor when the original
            // went away.
            registry.resubscribe_all(level.iter());
        }

        // The registry lock is released before any `read_dir`, so a large
        // subtree does not hold it across the I/O for a whole level.
        let mut next = Vec::new();
        for subdir in level.drain(..) {
            if !seen.insert(subdir.clone()) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&subdir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                let Ok(rel) = path.strip_prefix(root) else {
                    continue;
                };
                let rel = rel.to_string_lossy().replace('\\', "/");
                // A subtree that arrives whole — a clone, a `mv`, a branch
                // switch — can carry its own ignore rules. Those files are
                // dot-prefixed, so the scan below would silently drop them and
                // index the rest of the subtree against rules that do not know
                // about them.
                //
                // Ahead of the type dispatch, and following links: the walker
                // collects rule files with `Path::is_file`, which resolves
                // symlinks, whereas `DirEntry::file_type` does not — so a
                // symlinked `.gitignore` fell between the two branches below
                // and was never noticed, despite carrying rules the walker
                // would read.
                //
                // Abandon the descent immediately rather than finishing it.
                // Everything gathered from here on is discarded by the refresh
                // anyway, and the rules that are about to be published are the
                // ones that decide whether these directories should be watched
                // at all — continuing would subscribe to every level of, say, a
                // `node_modules/` that was just moved into place, which on
                // Linux is a watch descriptor apiece and the exhaustion this
                // pass exists to avoid. The refresh's `sync` would prune them,
                // but only after they had already been taken.
                if !state.no_ignore && is_ignore_rules_file(root, &path) && path.is_file() {
                    found_ignore_rules = true;
                    break 'descend;
                }
                // `DirEntry::file_type` does not follow symlinks, so a
                // symlinked directory is neither descended into nor indexed.
                if file_type.is_dir() {
                    let skip = {
                        let gitignore = state.gitignore.read().unwrap();
                        should_skip_watcher_dir(&rel, &state.exclude_dirs, gitignore.as_ref())
                    };
                    if !skip {
                        next.push(path);
                    }
                } else if file_type.is_file() {
                    let skip = {
                        let gitignore = state.gitignore.read().unwrap();
                        should_skip_watcher_path(&rel, &state.exclude_dirs, gitignore.as_ref())
                    };
                    if !skip {
                        files.push((path, rel));
                    }
                }
            }
        }
        level = next;
    }

    if found_ignore_rules {
        // Indexing now would apply the wrong rules to the whole subtree, and
        // anything wrongly indexed would stay until something touched it
        // again. The refresh rewalks and republishes, which covers these files
        // correctly; it runs on its own thread and takes `snapshot_gate`
        // there, so scheduling it while we hold the gate is safe.
        state.ignore_rules_dirty.store(true, Ordering::SeqCst);
        schedule_ignore_rules_refresh(Arc::clone(state), root.to_path_buf());
        return;
    }

    for (path, rel) in &files {
        reindex_file(state, path, rel, true);
    }
}

fn schedule_ignore_rules_refresh(state: Arc<ServerState>, root: PathBuf) {
    if state
        .ignore_refresh_scheduled
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    thread::spawn(move || {
        loop {
            if state.ignore_rules_dirty.swap(false, Ordering::SeqCst) {
                // Wait out a build first. `background_index_build` publishes its
                // matcher — and so reaches here — while it is still only
                // part-way through Phase 2, and it holds `snapshot_gate` for
                // none of that. The refresh would take the gate uncontended,
                // replace `file_stamps` wholesale from its own walk, and then
                // have the build overwrite them again from a walk that predates
                // the new rules: an index and a stamp map describing two
                // different trees, with no scan left to notice.
                //
                // A wait rather than a lock, so it cannot deadlock against the
                // build; and nothing is lost by waiting, because the build is
                // still walking the tree the refresh would walk.
                while state.indexing.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(200));
                }
                // The stale refresh walks the tree anyway and republishes the
                // matcher from that walk, so the reload costs one traversal
                // rather than a rebuild plus a re-scan.
                if !background_refresh_stale(&state, &root, &state.index_dir, true) {
                    state.ignore_rules_dirty.store(true, Ordering::SeqCst);
                    thread::sleep(Duration::from_secs(1));
                }
            }

            state
                .ignore_refresh_scheduled
                .store(false, Ordering::SeqCst);
            if !state.ignore_rules_dirty.load(Ordering::SeqCst)
                || state
                    .ignore_refresh_scheduled
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
            {
                break;
            }
        }
    });
}

fn schedule_pending_ignore_refresh(state: &Arc<ServerState>) {
    if state.ignore_rules_dirty.load(Ordering::SeqCst) {
        schedule_ignore_rules_refresh(Arc::clone(state), state.root.clone());
    }
}

/// Remember the paths in an event that arrived mid-build, for replay once the
/// build publishes. Returns whether it did — a `false` means the build finished
/// underneath us and the caller should handle the event normally.
///
/// The event cannot be applied now: until `indexing` clears, `file_stamps` does
/// not describe the index, so every path would compare as changed and the
/// watcher would re-read the repository alongside the build already reading it.
/// It cannot simply be dropped either — see [`ServerState::deferred_events`].
///
/// `indexing` is re-read *under the buffer lock*, and that is what makes the
/// handoff safe. The caller's own check is only a hint: between it and this
/// call the build can finish and [`replay_deferred_events`] can swap the buffer
/// out, and an insert landing after that swap is in a set nothing will ever
/// look at again. Because the replay cannot swap until `indexing` is false, and
/// cannot swap without this lock, seeing `indexing` set while holding it proves
/// the swap has not happened yet.
///
/// Capped, and the cap discards the whole set rather than truncating it: a
/// partial set is indistinguishable from a complete one at replay time, and
/// silently recovering nine tenths of a checkout is worse than knowing to fall
/// back on a full refresh.
fn defer_events_during_build(state: &ServerState, event: &Event) -> bool {
    /// A checkout of the Linux kernel is ~90k files. Above this the fallback
    /// refresh is cheaper than the replay would be anyway.
    const MAX_DEFERRED: usize = 100_000;

    let Ok(mut deferred) = state.deferred_events.lock() else {
        return false;
    };
    if !state.indexing.load(Ordering::SeqCst) {
        return false;
    }
    let Some(paths) = deferred.as_mut() else {
        // Already overflowed, so the fallback refresh will cover this path too.
        return true;
    };
    if paths.len().saturating_add(event.paths.len()) > MAX_DEFERRED {
        *deferred = None;
        eprintln!(
            "[trace] warning: too many file changes during the initial index build to replay \
             individually; a full reconcile will run instead"
        );
        return true;
    }
    // Only these kinds can put a directory somewhere, and only they should
    // trigger a subtree walk on replay. See `ServerState::deferred_events`.
    let introduces_dir = matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
    );
    for path in &event.paths {
        // A path seen both ways keeps the stronger claim: a directory that was
        // created and then chmod'd still needs its subtree picked up.
        let entry = paths.entry(path.clone()).or_insert(false);
        *entry |= introduces_dir;
    }
    true
}

/// Apply the events that arrived during the build, now that it has published.
///
/// Replayed as synthetic events rather than handled directly so they go through
/// exactly the filtering an ordinary event gets — ignore rules, the exclude
/// list, the index directory, new-subtree subscription. The kind is
/// reconstructed from the flag recorded with each path so the directory gate
/// still holds; beyond that gate, creations and modifications take the same
/// route, and `handle_fs_event` decides removal from whether the path still
/// exists, so one kind of each class covers arrivals, edits and deletions
/// alike.
///
/// The caller must *not* hold `snapshot_gate`; `handle_fs_event` takes it.
fn replay_deferred_events(state: &Arc<ServerState>, root: &Path) {
    let deferred = match state.deferred_events.lock() {
        // Leaves an empty map behind, so anything deferred by a later build
        // (a reconcile sets `indexing` again) is collected from scratch. The
        // caller has already waited out `indexing`, and `defer_events_during_
        // build` re-reads it under this same lock, so nothing can be inserted
        // into the old map after this point.
        Ok(mut guard) => guard.replace(std::collections::HashMap::new()),
        Err(_) => return,
    };
    let Some(paths) = deferred else {
        // Overflowed. A stale refresh rewalks the tree and diffs it against the
        // index, which covers every path the replay would have, and it is what
        // already runs when ignore rules change mid-build.
        eprintln!("[trace] watcher: reconciling after too many changes during the initial build");
        let state = Arc::clone(state);
        let root = root.to_path_buf();
        if thread::Builder::new()
            .name("tgrep-deferred-reconcile".into())
            .spawn(move || {
                if !background_refresh_stale(&state, &root, &state.index_dir, true) {
                    eprintln!(
                        "[trace] warning: the post-build reconcile did not complete; changes made \
                         during the build wait for the next one"
                    );
                }
            })
            .is_err()
        {
            eprintln!("[trace] warning: could not start the post-build reconcile");
        }
        return;
    };
    if paths.is_empty() {
        return;
    }

    let start = Instant::now();
    let count = paths.len();
    let mut missing_subtree = false;
    for (path, introduces_dir) in paths {
        if !is_real_dir(&path)
            && path.strip_prefix(root).is_ok_and(|rel| {
                let rel = rel.to_string_lossy().replace('\\', "/");
                let index = state.index.read().unwrap();
                index.reader_has_descendant_path(&rel) || index.live.has_descendant_path(&rel)
            })
        {
            // A single directory removal/rename event names no descendants.
            // Replaying it as a file event would drop only the directory path,
            // leaving every indexed child searchable. One coalesced full pass
            // supplies the missing subtree membership evidence.
            missing_subtree = true;
            continue;
        }
        let kind = if introduces_dir {
            EventKind::Create(notify::event::CreateKind::Any)
        } else {
            EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            ))
        };
        handle_fs_event(
            state,
            root,
            &Event {
                kind,
                paths: vec![path],
                attrs: Default::default(),
            },
        );
    }
    if missing_subtree {
        state.ignore_rules_dirty.store(true, Ordering::SeqCst);
        schedule_ignore_rules_refresh(Arc::clone(state), root.to_path_buf());
    }
    eprintln!(
        "[trace] watcher: replayed {count} change(s) deferred during the initial build in {:.1}ms",
        start.elapsed().as_secs_f64() * 1000.0
    );
}

/// For the publish sites that cannot scan inline. Until a directory is
/// subscribed it cannot report a write, and the walk that decided to subscribe
/// to it may have passed it before that write happened, so a file landing in
/// that window is in neither place and would wait for the hourly reconcile.
///
/// Spawned because at startup this is every directory in the repository and
/// the callers are on paths that must not block: `start_file_watcher` has to
/// get its worker running, and an index build must not stop to stat the tree
/// it is already reading.
///
/// Waits out `indexing` first. During a build the stamps do not describe the
/// index yet, so every file would look changed and the scan would re-read the
/// whole repository alongside the build that is already doing it. That wait is
/// also what makes this the right place to replay the events the build made the
/// watcher discard, which is why it runs even on a backend that has nothing
/// per-directory to recover.
fn spawn_recovery_scan(
    state: &Arc<ServerState>,
    root: &Path,
    dirs: Vec<PathBuf>,
    since: SystemTime,
) {
    let state = Arc::clone(state);
    let root = root.to_path_buf();
    let spawned = thread::Builder::new()
        .name("tgrep-watch-recovery".into())
        .spawn(move || {
            while state.indexing.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(200));
            }
            // Before the gate, not under it: this takes `snapshot_gate` for
            // read itself, and std's `RwLock` may deadlock on a recursive read
            // if a writer queues up in between.
            replay_deferred_events(&state, &root);

            let dirs = recovery_scan_dirs(&state, &root, dirs);
            if dirs.is_empty() {
                return;
            }
            // Read, not write: this does exactly what `handle_fs_event` does,
            // and that runs under the read side. Taking it at all is what
            // keeps the stamp check and the index update from interleaving
            // with a flush.
            let _gate = state.snapshot_gate.read().unwrap();
            reindex_files_in(&state, &root, &dirs, since);
        });
    if spawned.is_err() {
        eprintln!(
            "[trace] warning: could not start the watcher recovery scan; \
             files written while subscriptions were being established will \
             wait for the next reconcile"
        );
    }
}

/// The directories a recovery scan should recheck, given the ones a
/// subscription sync reported as newly watched.
///
/// The root is added because it is never in that list: it is subscribed as the
/// watcher starts, before any matcher exists, so every later sync sees it as
/// already watched. Nothing else covers it — a file written to the top level
/// while a build walk was deeper in the tree produced no event the build could
/// use and no event the watcher would keep — and it costs one directory
/// listing.
///
/// Empty on a whole-subtree backend, where there are no per-directory
/// subscriptions to have raced and `reindex_files_in`'s pickup of unwatched
/// subdirectories would take exactly the per-directory watches that backend
/// exists to avoid.
fn recovery_scan_dirs(state: &ServerState, root: &Path, mut dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    if !PER_DIRECTORY_WATCHES || !state.watch_enabled {
        return Vec::new();
    }
    if !dirs.iter().any(|d| d == root) {
        dirs.push(root.to_path_buf());
    }
    dirs
}

fn handle_fs_event(state: &Arc<ServerState>, root: &Path, event: &Event) {
    let dominated_kinds = matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    );
    if !dominated_kinds {
        return;
    }

    // Two ways an event can carry a rules change. The obvious one is a path the
    // walker would read as a rules file, recognised by name.
    //
    // The other is a path the published matcher actually read through a
    // symlink. `ignore_files_in` uses `Path::is_file`, which follows links, so
    // a `.gitignore` symlinked to `shared-rules` contributes the *target's*
    // contents — but editing the target produces an event naming `shared-rules`,
    // whose basename means nothing to `is_ignore_rules_file`, and touches
    // nothing whose name does. Recognising the paths that were read, and not
    // just the names rules usually go by, is what closes that.
    //
    // Only targets inside `root` can appear here, and [`ignore_target_dirs`] is
    // what makes them observable: their directory is subscribed to even when the
    // rules hide it, precisely so this lookup has an event to run against. For a
    // target outside the root none arrives and the periodic reconcile remains
    // the backstop.
    let ignore_rules_changed = !state.no_ignore && {
        let stamps = state.ignore_source_stamps.read().unwrap();
        event.paths.iter().any(|path| {
            is_ignore_rules_file(root, path)
                || path
                    .strip_prefix(root)
                    .is_ok_and(|rel| stamps.contains_key(&rel.to_string_lossy().replace('\\', "/")))
        })
    };
    if ignore_rules_changed {
        state.ignore_rules_dirty.store(true, Ordering::SeqCst);
        if state.indexing.load(Ordering::SeqCst) {
            return;
        }
        schedule_ignore_rules_refresh(Arc::clone(state), root.to_path_buf());
        return;
    }

    // Skip ordinary file events while the initial background index build is in
    // progress. The indexer will pick up those files itself — but only for the
    // parts of the tree it has not reached yet, so remember these and replay
    // them once it publishes.
    //
    // The load is a fast path that keeps the mutex out of the common case; the
    // decision is made under the lock, since the build can finish between the
    // two and an event deferred after that is never replayed.
    if state.indexing.load(Ordering::SeqCst) && defer_events_during_build(state, event) {
        return;
    }

    // Acquire the snapshot gate up-front for the whole event. While a
    // flush/auto-save is publishing (writer holds it), no reindex
    // *work* — file I/O, trigram extraction, even the [trace] line —
    // should happen, both for correctness (no overlay mutation between
    // snapshot and prune) and to avoid spending CPU/IO on work that
    // would just block the watcher thread anyway. We hold it for read
    // so multiple events can proceed concurrently outside any flush.
    let _gate = state.snapshot_gate.read().unwrap();

    // Stay off the index until the initial ignore matcher exists. This check
    // must happen *under* the gate: during startup the stale walk holds the
    // write side, so an event waits and is applied after publication instead
    // of being dropped after the walk may already have visited its path.
    if state.gitignore_pending.load(Ordering::SeqCst) {
        return;
    }

    for path in &event.paths {
        // Skip the index directory itself
        if path
            .to_string_lossy()
            .contains(&format!("{}.tgrep", std::path::MAIN_SEPARATOR))
        {
            continue;
        }

        let rel_path = match path.strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };

        // Mirror the walker's filtering so the watcher does not reindex
        // files the initial walk would have skipped — most notably
        // hidden directories like `.git/`, which fire frequent
        // `index.lock`/HEAD/refs writes during normal git operations.
        let should_skip = {
            let gitignore = state.gitignore.read().unwrap();
            should_skip_watcher_path(&rel_path, &state.exclude_dirs, gitignore.as_ref())
        };
        if should_skip {
            continue;
        }

        // Classified through the same contract the sweep uses, from one stat.
        // `Path::exists` and `Path::is_file` map every metadata error to
        // `false`, so a file held open by a build, a Windows sharing violation,
        // or a momentary `EACCES` used to read as "gone" and "not a regular
        // file" respectively — and both branches below then evicted content
        // that was still perfectly valid. `reindex_file` deliberately preserves
        // entries through exactly those failures, but it never got the chance:
        // the drop happens here, before it is ever called.
        let target = classify_event_target(&std::fs::metadata(path));
        if target == EventTarget::Unknown {
            // Unreadable right now is not proof of anything. Leave what is
            // indexed alone; the stale path keeps such files and retries them.
            continue;
        }

        let is_remove = matches!(event.kind, EventKind::Remove(_)) || target == EventTarget::Gone;

        if is_remove {
            // A watched directory that disappears takes its descriptor with
            // it, but not its entry in the registry. Clearing that entry is
            // what lets the path be subscribed again if it comes back — and
            // keeps `watched` from accumulating dead paths between syncs.
            // Done for every removed path rather than only known directories,
            // since by now there is nothing left to ask what it was; a path
            // that was never watched is a single failed hash lookup.
            if PER_DIRECTORY_WATCHES
                && let Some(registry) = state.watch_registry.lock().unwrap().as_mut()
            {
                registry.forget(path);
            }
            // notify can deliver Remove events for transient/unknown paths
            // (e.g. a build tool's temp file). Suppress the noisy log line
            // for those, but still apply the delete unconditionally — if
            // `file_stamps` is missing/out-of-date (e.g. first run after
            // an older index), skipping the delete entirely would leave
            // stale entries for files that no longer exist.
            //
            // Under `reindex_lock`, or a concurrent `reindex_file` that has
            // already read the file's bytes commits them *after* this delete
            // and resurrects a file that is gone — with a fresh stamp, so
            // nothing afterwards disagrees and no further event is coming to
            // correct it. The lock makes the two orderings the only two: the
            // delete lands on content that was committed, or the reindex opens
            // a path that is already gone and drops it.
            let _reindex = lock_reindex(state);
            // gate acquired at the function level — the entire event
            // is processed atomically with respect to flush/auto-save.
            drop_indexed_file(state, &rel_path, "removed");
            continue;
        }

        // `is_file` follows symlinks, so a link to a file lands in
        // `reindex_file` below rather than here — deliberately: that is where
        // it is recognised as ineligible and any content indexed under that
        // path before it became a link is dropped.
        if target != EventTarget::Regular {
            // `is_real_dir` rather than `is_dir`: the latter follows symlinks,
            // and a link to a directory is not something the walker descends
            // into, so subscribing to and indexing its target would pull in a
            // tree the index never contained — possibly outside `root`.
            if is_real_dir(path) {
                // Only for events that can actually introduce a directory. Any
                // `Modify` would include `Modify(Metadata)`, which a recursive
                // chmod or a checkout fires once per directory — and each one
                // would re-walk and re-subscribe that directory's whole subtree
                // on the single watcher worker, turning a linear operation into
                // quadratic work. inotify announces a new directory as `Create`
                // and one moved in as `Modify(Name)`; nothing else can.
                let introduces_dir = matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
                );
                if introduces_dir {
                    // A directory that just appeared can already be full — a
                    // `mv` of a populated tree from outside the root, a
                    // checkout, an unpacked archive — and nothing reports the
                    // contents it arrived with.
                    //
                    // On a per-directory backend that is because notify does
                    // not extend a non-recursive watch set for us. On a
                    // recursive backend it is the kernel's own doing: both
                    // `ReadDirectoryChangesW` and FSEvents report a moved-in
                    // tree as one event for the directory and say nothing about
                    // what is inside it. Either way those files are in no walk
                    // and no event, and stay unsearchable until the hourly
                    // reconcile — so the enumeration runs on every platform and
                    // only the subscribing part stays per-directory.
                    watch_new_subtree(state, root, path);
                }
                continue;
            }
            // Neither a regular file nor a directory: a fifo, a socket, a
            // device, or a symlink of any kind. An indexed `x.rs` atomically
            // replaced by one of those is not a removal — `path.exists()` is
            // still true and inotify may report only the rename destination —
            // so nothing above catches it, and without this the old contents
            // stay searchable indefinitely.
            //
            // Same lock as the removal above, for the same reason: a reindex
            // already holding the old bytes must not commit them after this.
            let _reindex = lock_reindex(state);
            drop_indexed_file(state, &rel_path, "no longer a regular file");
            continue;
        }

        reindex_file(state, path, &rel_path, true);
    }
}

/// Take the mutation lock, tolerating a previous holder's panic.
///
/// Poisoning here means some other indexer panicked partway through an update,
/// not that the index is unusable. Refusing to serialise from then on would
/// turn one failure into the resurrection race this lock exists to prevent.
fn lock_reindex(state: &ServerState) -> std::sync::MutexGuard<'_, ()> {
    match state.reindex_lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Drop everything the content and filename indexes hold for a path.
///
/// A stamp is evidence, but not a precondition: `filestamps.json` may be missing
/// while the active reader still holds the path. Conversely, a rejected file
/// absent from the reader, live overlay, and stamps was never indexed, so
/// recording a tombstone for it would dirty the overlay for no state change.
///
/// The caller must already hold `snapshot_gate` and `reindex_lock`. The lock is
/// the caller's rather than this function's because `reindex_file` calls in
/// while holding it, and a `Mutex` is not reentrant.
fn drop_indexed_file(state: &ServerState, rel_path: &str, reason: &str) {
    let removed_extra = state.filename_extra_paths.write().unwrap().remove(rel_path);
    if removed_extra {
        state.filename_index_dirty.store(true, Ordering::SeqCst);
    }
    let had_stamp = state
        .file_stamps
        .write()
        .unwrap()
        .remove(rel_path)
        .is_some();
    {
        let index = state.index.read().unwrap();
        if index.live.is_deleted(rel_path) {
            return;
        }
        if !had_stamp && !index.live.has_path(rel_path) && !index.reader_has_path(rel_path) {
            return;
        }
    }
    eprintln!("[trace] reindex: dropped {rel_path} ({reason})");
    let mut index = state.index.write().unwrap();
    index.live.delete_file(rel_path);
    invalidate_cached_paths_locked(state, std::iter::once(rel_path));
}

/// Move a listable path out of the content index.
///
/// The extra-path insertion is the membership transition. Only that first
/// transition creates a tombstone, so duplicate watcher notifications for a
/// binary path do not repeatedly dirty the live overlay.
fn mark_filename_only(state: &ServerState, rel_path: &str) {
    let inserted = {
        let mut index = state.index.write().unwrap();
        let mut extra = state.filename_extra_paths.write().unwrap();
        if !extra.insert(rel_path.to_string()) {
            false
        } else {
            if !index.live.is_deleted(rel_path) {
                index.live.delete_file(rel_path);
            }
            invalidate_cached_paths_locked(state, std::iter::once(rel_path));
            true
        }
    };
    if !inserted {
        return;
    }

    state.filename_index_dirty.store(true, Ordering::SeqCst);
    state.file_stamps.write().unwrap().remove(rel_path);
}

/// What an event's stat result says about the path it named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventTarget {
    /// A regular file: index it.
    Regular,
    /// Something that exists but is not a regular file — a directory, a fifo, a
    /// socket, a device. Whatever was indexed under this path has to go.
    NotRegular,
    /// Proven not to be there: absent, unreachable except through a symlink, or
    /// behind a component that is not a directory.
    Gone,
    /// Exists or not, this stat cannot say. Nothing may be concluded from it.
    Unknown,
}

/// Classify the target of an event from a stat of its path.
///
/// The `Unknown` case is the point of this. `Path::exists` and `Path::is_file`
/// fold every error into `false`, which turns "a build has this file open" and
/// "the directory was briefly unreadable" into "it is gone" — and the watcher
/// then evicts live content on the strength of it. [`proves_ineligible`] is the
/// contract that separates the two, and it is the same one the recovery sweep
/// and [`reindex_file`] answer to, so all three agree about what an I/O failure
/// is allowed to mean.
///
/// The stat follows symlinks, which is deliberate: a link to a regular file is
/// classified `Regular` here and refused by `open_within_root` in
/// [`reindex_file`], which is where content indexed under a path that has since
/// become a link is dropped.
fn classify_event_target(meta: &std::io::Result<std::fs::Metadata>) -> EventTarget {
    match meta {
        Ok(meta) if meta.is_file() => EventTarget::Regular,
        Ok(_) => EventTarget::NotRegular,
        Err(error) if proves_ineligible(error) => EventTarget::Gone,
        Err(_) => EventTarget::Unknown,
    }
}

/// Whether a failure to open a path establishes that it does not belong in the
/// index, as opposed to merely being unreadable right now.
///
/// The distinction decides whether the watcher drops what it has indexed. A
/// path that is gone, or that cannot be reached without traversing a symlink,
/// is genuinely ineligible and the entry has to go. A path that is locked,
/// unreadable, or lost to a descriptor limit is none of those things — dropping
/// on that would evict live content because a build held the file open for a
/// moment, and the stale path deliberately keeps unreadable files and retries
/// them later.
fn proves_ineligible(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    // `InvalidInput` and `NotADirectory` are what `open_within_root` itself
    // returns for a path that escapes the root, has a non-literal component, or
    // runs through something that is not a directory.
    if matches!(
        error.kind(),
        ErrorKind::NotFound | ErrorKind::NotADirectory | ErrorKind::InvalidInput
    ) {
        return true;
    }
    // A symlink met under `O_NOFOLLOW`, at any level. Not yet a stable
    // `ErrorKind`, so it has to be read from the raw code.
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return true;
    }
    false
}

/// Open a file without following a final symlink, so the handle is the path
/// itself rather than wherever it points.
///
/// Only the last component. For a path whose ancestors are not already trusted,
/// use [`open_within_root`] — which on unix has no use for this, since `openat`
/// resolves the final component the same way as every other one.
#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Opens the reparse point rather than its target. Unlike O_NOFOLLOW
        // this succeeds, so the caller's `is_file` check on the handle's
        // metadata is what rejects it — a reparse point is not a regular file.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::fs::File::open(path)
    }
}

/// Open a file under `root` without traversing a symlink at *any* level.
///
/// Refusing to follow the final component is not enough. A path arrives here
/// from an event or a replay as a name, and `root/a/file` reads the same
/// whether `a` is a directory or a link to one — so an intermediate link is
/// enough to hand back a file outside the served tree, which is exactly the
/// containment the walker's `follow_links(false)` promises and the index's
/// contract depends on.
///
/// `root` itself is the trust anchor and is opened normally: it is the
/// directory the user asked us to serve, so a link there is theirs to have.
///
/// On unix this is race-free. Each component is resolved with `openat` against
/// the handle for its parent, so the name is never re-resolved and there is no
/// window in which a directory can be swapped for a link between the check and
/// the use.
#[cfg(unix)]
fn open_within_root(root: &Path, path: &Path) -> std::io::Result<std::fs::File> {
    use std::io::{Error, ErrorKind};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let components = relative_components(root, path)?;
    let mut dir: OwnedFd = std::fs::File::open(root)?.into();
    let last = components.len() - 1;
    for (i, component) in components.iter().enumerate() {
        let name = std::ffi::CString::new(component.as_bytes())
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "path component contains a NUL"))?;
        // `O_DIRECTORY` on the intermediates so a *file* in the middle of the
        // path fails here rather than at the next `openat`, and `O_NOFOLLOW` on
        // every one of them, including the last. `O_NONBLOCK`: see
        // `open_no_follow`.
        let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        if i != last {
            flags |= libc::O_DIRECTORY;
        }
        // SAFETY: `dir` is a live directory descriptor for the parent, and
        // `name` is a NUL-terminated single path component that outlives the
        // call.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        // SAFETY: `openat` returned a fresh owned descriptor. Assigning it
        // drops the previous one, closing the parent we no longer need.
        dir = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    Ok(std::fs::File::from(dir))
}

/// As above. Windows has no `openat`, so containment is established after the
/// fact instead of during resolution: the file is opened without following a
/// final reparse point, and the *handle* is then asked where it actually ended
/// up. Anything that is not under the root's own resolved path is refused.
///
/// This is race-free in the way that matters. Checking each ancestor by path
/// first would only reject a junction that happened to be there at the time of
/// the check — one substituted between the check and the open would still be
/// followed. Asking the handle removes the second lookup entirely: whatever the
/// open resolved through, the answer describes the object we are actually
/// holding.
#[cfg(windows)]
fn open_within_root(root: &Path, path: &Path) -> std::io::Result<std::fs::File> {
    use std::io::{Error, ErrorKind};

    // Rejects escapes and non-literal components before anything is opened.
    relative_components(root, path)?;
    let file = open_no_follow(path)?;

    // The root's own resolved path, since it may itself sit under a junction or
    // a substituted drive — comparing against the path as given would then
    // reject every file in the tree. `canonicalize` is the same
    // `GetFinalPathNameByHandleW` query underneath, so the two agree on
    // verbatim prefix and casing.
    let anchor = std::fs::canonicalize(root)?;
    let opened = final_path_of(&file)?;
    if !opened.starts_with(&anchor) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "path resolves outside the served root",
        ));
    }
    Ok(file)
}

/// Where an open handle actually is, with every reparse point on the way
/// resolved.
#[cfg(windows)]
fn final_path_of(file: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
    };

    let handle = file.as_raw_handle() as isize;
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: `handle` is a live file handle borrowed from `file`, and the
        // buffer's length is passed as its capacity in `u16`s.
        let needed = unsafe {
            GetFinalPathNameByHandleW(
                handle as _,
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        if needed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // The return value excludes the NUL when it fits and includes it when
        // it does not, so a value at or past the capacity means "too small".
        if (needed as usize) < buf.len() {
            buf.truncate(needed as usize);
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(&buf)));
        }
        buf.resize(needed as usize + 1, 0);
    }
}

/// A fallback for platforms that are neither unix nor Windows, where there is
/// no way to do better than refusing a link that is actually there.
#[cfg(not(any(unix, windows)))]
fn open_within_root(root: &Path, path: &Path) -> std::io::Result<std::fs::File> {
    use std::io::{Error, ErrorKind};

    let components = relative_components(root, path)?;
    let mut walked = root.to_path_buf();
    for component in &components[..components.len() - 1] {
        walked.push(component);
        let meta = std::fs::symlink_metadata(&walked)?;
        if meta.file_type().is_symlink() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "path traverses a symlink",
            ));
        }
        if !meta.is_dir() {
            return Err(Error::new(ErrorKind::NotADirectory, "not a directory"));
        }
    }
    open_no_follow(path)
}

/// `path` split into the literal components below `root`.
///
/// Anything that is not a plain name — `..`, a root, a prefix — is refused
/// rather than interpreted, since resolving those is the whole business
/// [`open_within_root`] is avoiding.
fn relative_components(root: &Path, path: &Path) -> std::io::Result<Vec<std::ffi::OsString>> {
    use std::io::{Error, ErrorKind};

    let rel = path
        .strip_prefix(root)
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "path is outside the served root"))?;
    let mut components = Vec::new();
    for component in rel.components() {
        match component {
            std::path::Component::Normal(name) => components.push(name.to_os_string()),
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "path has a non-literal component",
                ));
            }
        }
    }
    if components.is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, "path is the root"));
    }
    Ok(components)
}

/// The outcome of reading a file whose stat'd size has already been approved.
enum CappedRead {
    Data(Vec<u8>),
    /// The file yielded more bytes than the cap allows, whatever its size said.
    TooLarge,
    Failed,
}

fn file_still_has_bytes(
    root: &Path,
    path: &Path,
    expected_version: &tgrep_core::builder::FileVersion,
    expected: &[u8],
) -> std::io::Result<bool> {
    use std::io::Read;

    let mut file = open_within_root(root, path)?;
    if tgrep_core::builder::file_version(&file.metadata()?) != *expected_version {
        return Ok(false);
    }
    let mut offset = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if expected.get(offset..offset + read) != Some(&buffer[..read]) {
            return Ok(false);
        }
        offset += read;
    }
    if offset != expected.len()
        || tgrep_core::builder::file_version(&file.metadata()?) != *expected_version
    {
        return Ok(false);
    }
    let current = open_within_root(root, path)?;
    Ok(tgrep_core::builder::file_version(&current.metadata()?) == *expected_version)
}

fn current_path_is_ineligible(state: &ServerState, path: &Path) -> bool {
    let file = match open_within_root(&state.root, path) {
        Ok(file) => file,
        Err(error) => return proves_ineligible(&error),
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => return proves_ineligible(&error),
    };
    !metadata.is_file()
        || tgrep_core::walker::is_binary_extension(path)
        || state
            .max_file_size
            .is_some_and(|limit| metadata.len() > limit)
}

/// Reads a file's contents, never pulling in more than one byte past the cap.
///
/// The size that qualified this file was stat'd before the read, and appending
/// between the two is exactly what a log or a build artifact does. An
/// unbounded read would then hold the whole of it in memory and index it past
/// the limit the user set. One byte over is enough to prove it no longer
/// qualifies, and is all that is ever read beyond the limit.
fn read_within_limit(file: &mut std::fs::File, limit: Option<u64>, capacity: usize) -> CappedRead {
    use std::io::Read;

    let mut data = Vec::with_capacity(capacity);
    match limit {
        Some(limit) => {
            if file
                .take(limit.saturating_add(1))
                .read_to_end(&mut data)
                .is_err()
            {
                return CappedRead::Failed;
            }
            if data.len() as u64 > limit {
                return CappedRead::TooLarge;
            }
        }
        None => {
            if file.read_to_end(&mut data).is_err() {
                return CappedRead::Failed;
            }
        }
    }
    CappedRead::Data(data)
}

/// Read a file and merge it into the live index, unless its stamp says the
/// content we already indexed is current.
///
/// The caller must hold `snapshot_gate`: the read, the commit, and the stamp
/// update have to be atomic with respect to a flush or auto-save.
fn reindex_file(state: &Arc<ServerState>, path: &Path, rel_path: &str, force: bool) {
    // Against other indexers, not against searches. The gate above is held for
    // read, so without this a recovery scan and the watcher worker can both be
    // here for the same path, both read, and the one that read the *older*
    // content can commit last. See `ServerState::reindex_lock`.
    let _reindex = lock_reindex(state);

    // One handle for the whole decision, resolved a component at a time from
    // the root so no part of the path can be a symlink, and every fact below —
    // type, size, mtime, bytes — read back off it. Nothing that happens to the
    // path in the meantime can then make the content we index disagree with the
    // metadata we judged it by, or put it outside the tree we serve.
    let file = match open_within_root(&state.root, path) {
        Ok(f) => f,
        Err(e) if proves_ineligible(&e) => {
            // Gone, or not a regular file reachable without traversing a link.
            // It may still be a path we indexed before it became one, so fall
            // through to the drop rather than returning.
            drop_indexed_file(state, rel_path, "no longer eligible");
            return;
        }
        Err(_) => {
            // A permission error, a Windows sharing violation, a descriptor
            // limit — none of which say anything about whether the file
            // belongs in the index. Dropping on those would evict live content
            // because a build held the file open for a moment. The stale path
            // already treats unreadable files this way, keeping what it has and
            // retrying later, and the watcher should not disagree with it.
            if force {
                retry_failed_forced_reindex(state, rel_path, "the file could not be opened");
            }
            return;
        }
    };
    let Ok(meta) = file.metadata() else {
        if force {
            retry_failed_forced_reindex(state, rel_path, "the opened file could not be inspected");
        }
        return;
    };
    let mut version = tgrep_core::builder::file_version(&meta);
    let mut current = version.stamp().clone();

    // The type and size rules `walk_file_metadata` applies, and for the same
    // reason: the walk is authoritative about what belongs in the index, so
    // anything it rejects must not be added here. Without this a file that grew
    // past the cap would be read whole and indexed, and the next reconcile
    // would silently delete it again. Binary extensions remain listable and
    // move into the filename-only set below.
    //
    // `is_file` is the third rule, and the one with teeth: the walker runs with
    // `follow_links(false)`, where a symlink is neither file nor dir and is
    // skipped outright. Indexing through one would put the target's bytes under
    // the link's path — and the target need not be under `root` at all, so a
    // link committed to a branch (or dropped in by a build) is enough to pull
    // `~/.ssh/id_rsa` into an index whose whole contract is that it covers the
    // served tree. On unix the open above has already failed for a link at any
    // level; this is what rejects a final one on Windows, where the reparse
    // point opens fine.
    let eligible = meta.is_file()
        && !state
            .max_file_size
            .is_some_and(|limit| current.size > limit);
    if !eligible {
        // It may have been eligible when it was last indexed — a file can grow
        // past the cap, and a real file can be replaced by a link to one. Drop
        // what we hold so the index matches the walk rather than keeping a
        // stale copy of the smaller version until the reconcile.
        drop_indexed_file(state, rel_path, "no longer eligible");
        return;
    }
    if tgrep_core::walker::is_binary_extension(path) {
        mark_filename_only(state, rel_path);
        return;
    }

    if !force && state.file_stamps.read().unwrap().get(rel_path) == Some(&current) {
        return;
    }

    // Read contents and extract trigrams OUTSIDE the index write lock
    // so a concurrent search (which needs a read lock) is not blocked
    // on our file I/O and trigram parsing. Windows' SRWLock is
    // writer-preferring: a single waiting writer here would otherwise
    // stall every subsequent search request.
    let data = if force {
        // A concrete event is stronger evidence than the persisted stamp. The
        // initial handle established eligibility, but its bytes need not be
        // read: use one fresh, containment-safe handle as the indexing snapshot
        // and retry only if full-resolution metadata changes during that read.
        drop(file);
        let mut stable = None;
        for _ in 0..2 {
            let mut verify = match open_within_root(&state.root, path) {
                Ok(file) => file,
                Err(e) if proves_ineligible(&e) => {
                    drop_indexed_file(state, rel_path, "no longer eligible");
                    return;
                }
                Err(_) => {
                    retry_failed_forced_reindex(
                        state,
                        rel_path,
                        "the verification handle could not be opened",
                    );
                    return;
                }
            };
            let Ok(meta) = verify.metadata() else {
                retry_failed_forced_reindex(
                    state,
                    rel_path,
                    "the verification handle could not be inspected",
                );
                return;
            };
            let verify_version = tgrep_core::builder::file_version(&meta);
            let verify_current = verify_version.stamp().clone();
            if !meta.is_file()
                || tgrep_core::walker::is_binary_extension(path)
                || state
                    .max_file_size
                    .is_some_and(|limit| verify_current.size > limit)
            {
                drop_indexed_file(state, rel_path, "no longer eligible");
                return;
            }
            let verified = match read_within_limit(
                &mut verify,
                state.max_file_size,
                verify_current.size.min(1 << 20) as usize,
            ) {
                CappedRead::Data(data) => data,
                CappedRead::TooLarge => {
                    drop_indexed_file(state, rel_path, "grew past the size limit while being read");
                    return;
                }
                CappedRead::Failed => {
                    retry_failed_forced_reindex(state, rel_path, "the verification read failed");
                    return;
                }
            };
            if verify.metadata().is_ok_and(|metadata| {
                tgrep_core::builder::file_version(&metadata) == verify_version
            }) {
                current = verify_current;
                version = verify_version;
                stable = Some(verified);
                break;
            }
        }
        let Some(verified) = stable else {
            retry_failed_forced_reindex(state, rel_path, "the file kept changing while read");
            return;
        };
        #[cfg(test)]
        run_stale_refresh_hook(state, StaleRefreshPhase::AfterConcreteRead);
        verified
    } else {
        // From the approved handle, not the path: re-opening here is what would
        // let a symlink take the place of the file we just approved.
        let mut file = file;
        match read_within_limit(
            &mut file,
            state.max_file_size,
            current.size.min(1 << 20) as usize,
        ) {
            CappedRead::Data(data) => data,
            CappedRead::TooLarge => {
                drop_indexed_file(state, rel_path, "grew past the size limit while being read");
                return;
            }
            CappedRead::Failed => {
                let already_indexed = state.index.read().unwrap().has_active_path(rel_path);
                if !already_indexed {
                    mark_filename_only(state, rel_path);
                }
                return;
            }
        }
    };
    let text = tgrep_core::encoding::decode_for_index(&data);
    let is_binary = tgrep_core::trigram::is_binary(&text);
    let per_tri = if is_binary {
        None
    } else {
        Some(tgrep_core::live::LiveIndex::compute_trigram_masks(&text))
    };
    #[cfg(test)]
    run_stale_refresh_hook(state, StaleRefreshPhase::BeforeConcreteCommit);
    if force {
        match file_still_has_bytes(&state.root, path, &version, &data) {
            Ok(true) => {}
            Ok(false) => {
                if current_path_is_ineligible(state, path) {
                    drop_indexed_file(state, rel_path, "no longer eligible");
                } else {
                    retry_failed_forced_reindex(state, rel_path, "the file changed before commit");
                }
                return;
            }
            Err(error) if proves_ineligible(&error) => {
                drop_indexed_file(state, rel_path, "no longer eligible");
                return;
            }
            Err(_) => {
                retry_failed_forced_reindex(state, rel_path, "the file changed before commit");
                return;
            }
        }
    }

    eprintln!("[trace] reindex: modified {rel_path}");
    let Some(per_tri) = per_tri else {
        mark_filename_only(state, rel_path);
        return;
    };
    // Gate held by the caller — the commit + stamp update is processed
    // atomically with respect to flush/auto-save.
    let removed_extra = {
        let mut index = state.index.write().unwrap();
        let mut extra = state.filename_extra_paths.write().unwrap();
        index.live.commit_upsert(rel_path, per_tri);
        let removed = extra.remove(rel_path);
        invalidate_cached_paths_locked(state, std::iter::once(rel_path));
        removed
    };
    if removed_extra {
        state.filename_index_dirty.store(true, Ordering::SeqCst);
    }
    state
        .file_stamps
        .write()
        .unwrap()
        .insert(rel_path.to_string(), current);
}

fn retry_failed_forced_reindex(state: &Arc<ServerState>, rel_path: &str, reason: &str) {
    // In-memory stamps override the persisted map during stale comparison.
    // A sentinel therefore records "this path must be read" without rewriting
    // filestamps.json for a transient event failure.
    state.file_stamps.write().unwrap().insert(
        rel_path.to_string(),
        tgrep_core::meta::FileStamp {
            mtime: u64::MAX,
            size: u64::MAX,
        },
    );
    eprintln!("[trace] warning: {rel_path} was not reindexed because {reason}; scheduling a retry");
    state.ignore_rules_dirty.store(true, Ordering::SeqCst);
    schedule_ignore_rules_refresh(Arc::clone(state), state.root.clone());
}

/// Whether a scheduled reconcile should run now.
///
/// Split out from the loop so the schedule can be exercised without waiting
/// hours for it.
fn reconcile_due(since_last: Duration, quiet_for: Duration, busy: bool) -> bool {
    // Indexing and flushing are already rewriting the index, and a reconcile
    // takes the snapshot gate for its whole walk-and-merge. Let them finish;
    // the next tick is a minute away.
    if busy {
        return false;
    }
    if since_last >= RECONCILE_DEADLINE {
        return true;
    }
    since_last >= RECONCILE_INTERVAL && quiet_for >= RECONCILE_QUIET_PERIOD
}

/// Periodically compare the whole tree against the index, so a change the
/// watcher never heard about cannot stay wrong indefinitely.
///
/// See [`RECONCILE_INTERVAL`] for why this is needed at all. It is deliberately
/// unhurried: it defers to indexing, to flushing, and to a server that is
/// being queried, and it does nothing at all on a tree that has not drifted —
/// the walk finds no differences and returns without touching the index.
fn periodic_reconcile_loop(state: Arc<ServerState>, root: PathBuf, index_dir: PathBuf) {
    let mut last = Instant::now();
    loop {
        thread::sleep(RECONCILE_POLL);

        let busy = state.indexing.load(Ordering::SeqCst) || state.flushing.load(Ordering::SeqCst);
        if !reconcile_due(last.elapsed(), state.quiet_for(), busy) {
            continue;
        }

        // Restart the interval before the walk rather than after it. On a large
        // repository the reconcile itself takes a while, and timing from its
        // completion would push each one further out than the last.
        last = Instant::now();
        eprintln!("[trace] periodic reconcile: looking for changes the watcher missed");
        // Same comparison the startup check makes, and for the same reason: a
        // lost event is a stamp that disagrees with the filesystem, a file with
        // no stamp, or a stamp with no file, and all three fall out of that.
        // Comparing index *membership* as well would additionally re-add any
        // file whose stamp says indexed but which the reader does not hold —
        // a publication bug rather than a lost event, and one that on an hourly
        // timer would rebuild the whole index every hour if it ever misfired.
        if !background_refresh_stale(&state, &root, &index_dir, false) {
            // It declined — an unreadable directory, or a walk that raced a
            // delete. The index is untouched and correct as far as it goes,
            // and the next interval tries again.
            eprintln!("[trace] periodic reconcile: declined, keeping the current index");
        }
    }
}

fn auto_save_loop(state: Arc<ServerState>) {
    let mut last_save = Instant::now();

    loop {
        thread::sleep(Duration::from_secs(60));

        // Don't auto-save while background indexing or a bulk flush is
        // active — those paths handle their own publication and an
        // auto-save fired in parallel would just snapshot the same
        // overlay redundantly.
        if state.indexing.load(Ordering::SeqCst) || state.flushing.load(Ordering::SeqCst) {
            continue;
        }

        let dirty = {
            let index = state.index.read().unwrap();
            index.live.dirty_count()
        };

        let elapsed = last_save.elapsed();
        if dirty >= state.auto_save_mutations || (dirty > 0 && elapsed >= AUTO_SAVE_INTERVAL) {
            let save_start = Instant::now();
            eprintln!("[trace] auto-save: {dirty} mutations, saving...");

            // Hold the gate through delta build → publish → prune so watcher
            // mutations cannot race publication. Recheck after acquiring it:
            // another publisher may have drained the overlay while we waited.
            let _gate = state.snapshot_gate.write().unwrap();
            if !state.index.read().unwrap().live.has_pending_changes() {
                continue;
            }

            let stamps = state.file_stamps.read().unwrap().clone();
            if stream_merge_stale_changes(
                &state,
                &[],
                &[],
                &[],
                &stamps,
                StaleMergePolicy {
                    preserved: &std::collections::HashSet::new(),
                    operation: "auto-save",
                    authoritative_membership: false,
                    authoritative_listed_files: None,
                },
            ) {
                last_save = Instant::now();
                eprintln!(
                    "[trace] auto-save complete in {:.1}s",
                    save_start.elapsed().as_secs_f64()
                );
            }
        }
    }
}

/// Check whether `path` passes the glob filter list.
///
/// Glob semantics:
/// - Patterns starting with `!` are **exclusion** patterns (path must NOT match).
/// - All other patterns are **inclusion** patterns (path must match at least one).
/// - If only exclusion patterns are present, the path passes unless it matches
///   an exclusion.
/// - If inclusion patterns are present, the path must match at least one AND
///   must not match any exclusion.
fn json_rpc_result(id: Option<serde_json::Value>, result: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "result": result,
        "id": id.unwrap_or(serde_json::Value::Null),
    })
    .to_string()
}

fn json_rpc_error(id: Option<serde_json::Value>, code: i32, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "error": {
            "code": code,
            "message": message,
        },
        "id": id.unwrap_or(serde_json::Value::Null),
    })
    .to_string()
}

/// Replace the filename-only delta from an authoritative filesystem path set.
///
/// Returns whether the delta changed and therefore needs to be persisted.
fn replace_filename_extra_paths(state: &ServerState, listed_files: &[String]) -> bool {
    let content_paths: std::collections::HashSet<String> = state
        .index
        .read()
        .unwrap()
        .all_paths()
        .into_iter()
        .collect();
    let next: std::collections::HashSet<String> = listed_files
        .iter()
        .filter(|path| !content_paths.contains(path.as_str()))
        .cloned()
        .collect();
    let mut current = state.filename_extra_paths.write().unwrap();
    let changed = *current != next || !state.filename_index_ready.load(Ordering::SeqCst);
    if changed {
        *current = next;
        state.filename_index_dirty.store(true, Ordering::SeqCst);
    }
    state.filename_index_ready.store(true, Ordering::SeqCst);
    changed
}

fn stage_filename_extra_paths(state: &ServerState, staging_dir: &Path) -> Result<()> {
    let mut paths: Vec<String> = state
        .filename_extra_paths
        .read()
        .unwrap()
        .iter()
        .cloned()
        .collect();
    paths.sort_unstable();
    tgrep_core::path_index::write_extra_paths(staging_dir, &paths)?;
    Ok(())
}

/// Publish just the filename sidecar after an authoritative walk that did not
/// otherwise need to rewrite the content index.
fn persist_filename_extra_paths(state: &ServerState, index_dir: &Path) -> bool {
    let staging_dir = index_dir.join(".filename-index-staging");
    let _ = std::fs::remove_dir_all(&staging_dir);
    if let Err(error) = stage_filename_extra_paths(state, &staging_dir) {
        eprintln!("[trace] warning: could not stage filename index: {error}");
        let _ = std::fs::remove_dir_all(&staging_dir);
        return false;
    }

    let published = {
        let _publish = state.publish_lock.lock().unwrap();
        let src = staging_dir.join(tgrep_core::path_index::EXTRA_PATHS_FILENAME);
        let dst = index_dir.join(tgrep_core::path_index::EXTRA_PATHS_FILENAME);
        match publish_file(&src, &dst) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("[trace] warning: could not publish filename index: {error}");
                false
            }
        }
    };
    let _ = std::fs::remove_dir_all(&staging_dir);
    if published {
        state.filename_index_dirty.store(false, Ordering::SeqCst);
    }
    published
}

fn refresh_filename_index(state: &ServerState, index_dir: &Path, listed_files: &[String]) {
    let changed = replace_filename_extra_paths(state, listed_files);
    if changed || state.filename_index_dirty.load(Ordering::SeqCst) {
        persist_filename_extra_paths(state, index_dir);
    }
}

/// Create a minimal empty on-disk index so HybridIndex::open() succeeds.
/// The actual data will be populated into the LiveIndex in the background.
fn create_empty_index(index_dir: &Path) -> Result<()> {
    use tgrep_core::meta::IndexMeta;
    // Own the directory precondition here rather than leaving it to each
    // caller: the recovery path in `reset_to_empty_index` runs after a failed
    // build, which is exactly when the directory is least likely to be intact.
    std::fs::create_dir_all(index_dir)?;
    // Empty lookup.bin, index.bin, files.bin
    std::fs::write(index_dir.join("lookup.bin"), b"")?;
    std::fs::write(index_dir.join("index.bin"), b"")?;
    std::fs::write(index_dir.join("files.bin"), b"")?;
    tgrep_core::path_index::remove_extra_paths(index_dir)?;
    let mut meta = IndexMeta::new("", 0, 0);
    meta.complete = false; // empty skeleton — not a complete index
    meta.save(index_dir)?;
    Ok(())
}

/// Stamps for the walked files that are actually in the index.
///
/// The build stamps its work from a *second* traversal, taken after the content
/// walk that fed the index, so a file created between the two appears here and
/// nowhere else. Publishing a stamp for it would be a lie the rest of the
/// server believes: `reindex_file` returns early when the stamp already matches
/// what is on disk, and the periodic reconcile runs with
/// `compare_index_membership` off, so it compares stamps alone too — the file
/// would stay unsearchable until something changed it again. Withholding the
/// stamp instead makes the very next event or scan treat it as new, which is
/// what it is.
fn stamps_for_index_members(
    files: Vec<tgrep_core::walker::FileMeta>,
    indexed: &std::collections::HashSet<String>,
) -> std::collections::HashMap<String, tgrep_core::meta::FileStamp> {
    files
        .into_iter()
        .filter(|fm| indexed.contains(&fm.relative_path))
        .map(|fm| {
            (
                fm.relative_path,
                tgrep_core::meta::FileStamp {
                    mtime: fm.mtime,
                    size: fm.size,
                },
            )
        })
        .collect()
}

/// Drop the stamps the build has no right to publish, because an event for
/// those paths arrived while it ran.
///
/// The stamp map describes what the index holds, and the build derives it from
/// a metadata walk taken *after* the content walk. For a file written between
/// the two the index holds the old bytes while the stamp describes the new
/// ones, so the stamp says "current" about content that is stale. That claim is
/// load-bearing in exactly the place that should have repaired it:
/// `reindex_file` returns early on a matching stamp, so the replay of the very
/// event that reported the write reads nothing, and the reconcile behind it
/// compares the same stamps and agrees. The old content stays searchable
/// indefinitely.
///
/// The deferred buffer already names those paths — it is what replay is about
/// to walk — so withholding their stamps costs one map lookup each and makes
/// the replay do the read it was deferred for.
///
/// When the buffer overflowed it names nothing, and nothing distinguishes the
/// files that changed from the ones that did not, so no stamp from this build
/// can be trusted and none is published. The reconcile that overflow already
/// schedules then re-reads the tree rather than believing a walk that raced
/// 100k changes.
///
/// Takes the deferred lock while `snapshot_gate` is held, which is the one
/// order in use: the watcher defers *before* it takes the gate, and replay
/// releases the buffer before handling anything.
fn withhold_stamps_for_deferred(
    state: &ServerState,
    root: &Path,
    stamps: std::collections::HashMap<String, tgrep_core::meta::FileStamp>,
) -> std::collections::HashMap<String, tgrep_core::meta::FileStamp> {
    let deferred = match state.deferred_events.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    withhold_stamps_for_deferred_snapshot(root, stamps, deferred.as_ref())
}

fn withhold_stamps_for_deferred_snapshot(
    root: &Path,
    mut stamps: std::collections::HashMap<String, tgrep_core::meta::FileStamp>,
    paths: Option<&std::collections::HashMap<PathBuf, bool>>,
) -> std::collections::HashMap<String, tgrep_core::meta::FileStamp> {
    let Some(paths) = paths else {
        eprintln!(
            "[trace] warning: too many changes during the initial build to say which files the \
             walk raced; publishing no stamps so the reconcile re-reads them"
        );
        return std::collections::HashMap::new();
    };
    let mut exact = std::collections::HashSet::new();
    let mut directories = std::collections::HashSet::new();
    for (path, introduces_dir) in paths {
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        exact.insert(rel.clone());
        if *introduces_dir {
            directories.insert(rel);
        }
    }
    let before = stamps.len();
    stamps.retain(|rel, _| {
        if exact.contains(rel) {
            return false;
        }
        let mut ancestor = rel.as_str();
        while let Some((parent, _)) = ancestor.rsplit_once('/') {
            if directories.contains(parent) {
                return false;
            }
            ancestor = parent;
        }
        true
    });
    let withheld = before - stamps.len();
    if withheld > 0 {
        eprintln!(
            "[trace] watcher: {withheld} file(s) changed during the initial build; their stamps \
             are withheld so the replay re-reads them"
        );
    }
    stamps
}

/// Detect files that changed while the server was not running.
/// Compares stored filestamps against current filesystem metadata, then upserts
/// changed/new files and removes deleted files from the LiveIndex.
fn classify_file_changes(
    current_meta: &[tgrep_core::walker::FileMeta],
    old_stamps: &std::collections::HashMap<String, tgrep_core::meta::FileStamp>,
    indexed_paths: &std::collections::HashSet<String>,
    compare_index_membership: bool,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    use tgrep_core::meta::FileStamp;

    let mut current_set = std::collections::HashSet::with_capacity(current_meta.len());
    let mut changed = Vec::new();
    let mut added = Vec::new();

    for fm in current_meta {
        current_set.insert(fm.relative_path.clone());
        let stamp = FileStamp {
            mtime: fm.mtime,
            size: fm.size,
        };
        if compare_index_membership && !indexed_paths.contains(&fm.relative_path) {
            added.push(fm.relative_path.clone());
            continue;
        }
        match old_stamps.get(&fm.relative_path) {
            Some(old) if *old == stamp => {}
            Some(_) => changed.push(fm.relative_path.clone()),
            None => added.push(fm.relative_path.clone()),
        }
    }

    let mut seen = std::collections::HashSet::new();
    let deleted = old_stamps
        .keys()
        .chain(indexed_paths)
        .filter(|path| seen.insert(path.as_str()))
        .filter(|path| !current_set.contains(path.as_str()))
        .cloned()
        .collect();

    (changed, added, deleted)
}

struct StaleMergePolicy<'a> {
    preserved: &'a std::collections::HashSet<String>,
    operation: &'a str,
    authoritative_membership: bool,
    authoritative_listed_files: Option<&'a [String]>,
}

/// Apply a stale diff without materializing the existing index in heap.
///
/// The ordinary incremental flush uses `HybridIndex::full_snapshot`, whose
/// memory is proportional to every posting already on disk. That is especially
/// harmful when a newer tgrep first opens an index built with an older file-size
/// cap: every formerly-oversized file appears as new at once. Build new and
/// replacement files into a bounded external-sort delta, then stream it together
/// with the old index while filtering replaced and deleted reader entries.
/// `preserved` contains candidates whose last read failed without proving a
/// deletion; they remain in the reader or live overlay until their metadata
/// changes and another read is attempted.
///
/// The caller holds `snapshot_gate` across the metadata walk and this merge, so
/// the walk's exact path set is newer than every live entry captured here.
fn stream_merge_stale_changes(
    state: &Arc<ServerState>,
    changed: &[String],
    added: &[String],
    deleted: &[String],
    stamps: &std::collections::HashMap<String, tgrep_core::meta::FileStamp>,
    policy: StaleMergePolicy<'_>,
) -> bool {
    let StaleMergePolicy {
        preserved,
        operation,
        authoritative_membership,
        authoritative_listed_files,
    } = policy;
    let root = &state.root;
    let index_dir = &state.index_dir;
    let (reader, overlay_paths, tombstone_paths) = {
        let index = state.index.read().unwrap();
        (
            index.reader_arc(),
            index.live.overlay_paths(),
            index.live.tombstone_paths(),
        )
    };

    state.flushing.store(true, Ordering::SeqCst);
    let start = Instant::now();
    eprintln!(
        "[trace] {operation}: building a memory-bounded delta \
         ({} changed, {} new, {} deleted)...",
        changed.len(),
        added.len(),
        deleted.len()
    );

    // Keep work directories inside the locked index directory. Sibling names
    // collide when two independent indexes share a parent directory.
    let delta_dir = index_dir.join(".stale-delta");
    let staging_dir = index_dir.join(".stale-merge");
    let _ = std::fs::remove_dir_all(&delta_dir);
    let _ = std::fs::remove_dir_all(&staging_dir);

    // Fold in every live mutation. A stale walk also treats its exact path set
    // as authoritative, removing reader entries missing from the walk; an
    // auto-save cannot do that because its filestamps may be incomplete.
    let mut seen = std::collections::HashSet::new();
    let mut candidates: Vec<String> = changed
        .iter()
        .chain(added)
        .chain(deleted)
        .chain(overlay_paths.iter())
        .chain(tombstone_paths.iter())
        .filter(|path| !preserved.contains(*path))
        .filter(|path| seen.insert((*path).clone()))
        .cloned()
        .collect();
    if authoritative_membership {
        candidates.extend(
            reader
                .all_paths()
                .iter()
                .filter(|path| !preserved.contains(path.as_str()))
                .filter(|path| !stamps.contains_key(path.as_str()))
                .filter(|path| seen.insert((*path).clone()))
                .cloned(),
        );
    }
    let desired_paths: Vec<String> = candidates
        .iter()
        .filter(|path| stamps.contains_key(path.as_str()))
        .cloned()
        .collect();
    let files: Vec<PathBuf> = desired_paths.iter().map(|path| root.join(path)).collect();
    // Every candidate is either removed or replaced by the delta. Including a
    // genuinely new path is harmless because it has no reader entry to filter.
    let mut removed: std::collections::HashSet<String> = candidates.iter().cloned().collect();

    let mut published_stamps = stamps.clone();
    let mut preserve_overlay_paths = preserved.clone();

    let result = (|| -> Result<PublishStatus> {
        let build = || {
            builder::build_index_for_files(
                root,
                &delta_dir,
                &files,
                builder::DEFAULT_INDEX_BUFFER_BYTES,
            )
        };
        let outcome = match rayon::ThreadPoolBuilder::new()
            .num_threads(state.index_threads)
            .thread_name(|i| format!("tgrep-stale-index-{i}"))
            .build()
        {
            Ok(pool) => pool.install(build)?,
            Err(_) => build()?,
        };
        let delta_count = outcome.indexed;

        // Withhold stamps for files the delta could not read. A published stamp
        // means "indexed at this version", so keeping one for a skipped file
        // would hide it from every later reconcile and make the miss permanent.
        // Dropping the stamp leaves it looking new, so the next pass retries it.
        //
        // Record what the file looked like when it failed, so a permanent
        // failure is retried when the file changes rather than on every pass.
        // See `ServerState::unreadable`.
        let unreadable = {
            let mut memo = state.unreadable.write().unwrap();
            // Anything this delta was asked to build is settled: either it was
            // read, or it is in `outcome.unreadable` and re-recorded below.
            for path in changed.iter().chain(added).chain(deleted) {
                memo.remove(path);
            }
            let mut unreadable = std::collections::HashSet::new();
            for path in &outcome.unreadable {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                unreadable.insert(rel.clone());
                if let Some(stamp) = published_stamps.remove(&rel) {
                    memo.insert(rel, stamp);
                }
            }
            unreadable
        };
        if !unreadable.is_empty() {
            // A missing delta entry must not be interpreted as a deletion. Keep
            // its old reader entry and any newer live overlay entry while the
            // rest of the merge publishes normally. Its withheld stamp and
            // memoized failed version make a later file change retry the read.
            candidates.retain(|path| !unreadable.contains(path));
            removed.retain(|path| !unreadable.contains(path));
            preserve_overlay_paths.extend(unreadable.iter().cloned());
            eprintln!(
                "[trace] {} file(s) were unreadable during the delta build; \
                 preserving their current index entries and retrying after they change",
                unreadable.len()
            );
        }

        let delta = tgrep_core::reader::IndexReader::open(&delta_dir)?;
        if delta.num_files() != delta_count {
            anyhow::bail!(
                "delta reopened with {} files after writing {delta_count}",
                delta.num_files()
            );
        }

        builder::merge_index_with_delta(root, &staging_dir, &reader, &delta, &removed, true)?;
        tgrep_core::meta::write_filestamps(&published_stamps, &staging_dir)?;
        let removed_reader_files = reader
            .all_paths()
            .iter()
            .filter(|path| removed.contains(path.as_str()))
            .count();
        let expected_files = reader.num_files() - removed_reader_files + delta.num_files();
        let filename_extra_paths = authoritative_listed_files.map(|listed_files| {
            let mut content_paths = std::collections::HashSet::with_capacity(expected_files);
            content_paths.extend(
                reader
                    .all_paths()
                    .iter()
                    .filter(|path| !removed.contains(path.as_str()))
                    .map(String::as_str),
            );
            content_paths.extend(delta.all_paths().iter().map(String::as_str));
            listed_files
                .iter()
                .filter(|path| !content_paths.contains(path.as_str()))
                .cloned()
                .collect::<std::collections::HashSet<_>>()
        });
        let published = publish_staged_index(
            state,
            index_dir,
            &staging_dir,
            expected_files,
            &candidates,
            &preserve_overlay_paths,
            filename_extra_paths.as_ref(),
        );
        if published.is_published() {
            // `publish_staged_index` prunes overlay entries represented by the
            // new reader. Also clear reconciled entries intentionally omitted
            // (newly ignored/binary/deleted) and old tombstones for files the
            // delta restored. The gate guarantees none is newer than this merge.
            state
                .index
                .write()
                .unwrap()
                .live
                .clear_reconciled_paths(&candidates);
            state
                .index_progress
                .store(expected_files as u64, Ordering::Relaxed);
            state
                .index_total
                .store(expected_files as u64, Ordering::Relaxed);
        }
        Ok(published)
    })();

    let _ = std::fs::remove_dir_all(&delta_dir);
    if !matches!(&result, Ok(PublishStatus::RollbackFailed)) {
        let _ = std::fs::remove_dir_all(&staging_dir);
    } else {
        eprintln!("[trace] warning: preserving {staging_dir:?} after rollback failure");
    }
    state.flushing.store(false, Ordering::SeqCst);
    if matches!(&result, Ok(PublishStatus::Published)) {
        *state.file_stamps.write().unwrap() = published_stamps;
    }
    match result {
        Ok(PublishStatus::Published) => {
            eprintln!(
                "[trace] {operation}: streamed {} changes into the index in {:.1}s",
                candidates.len(),
                start.elapsed().as_secs_f64()
            );
            true
        }
        Ok(PublishStatus::Failed) | Ok(PublishStatus::RollbackFailed) => {
            eprintln!(
                "[trace] warning: {operation} delta could not be published; \
                 keeping the old index"
            );
            false
        }
        Err(error) => {
            eprintln!(
                "[trace] warning: memory-bounded {operation} failed ({error}); \
                 keeping the old index"
            );
            false
        }
    }
}

/// Drop the candidates that failed to read last time and have not changed since.
///
/// Returns the paths removed, which the caller must also keep out of the
/// published stamps — see [`stamps_for_indexed`].
fn drop_memoized_failures(
    memo: &std::collections::HashMap<String, tgrep_core::meta::FileStamp>,
    current_meta: &[tgrep_core::walker::FileMeta],
    changed: &mut Vec<String>,
    added: &mut Vec<String>,
) -> std::collections::HashSet<String> {
    if memo.is_empty() {
        return std::collections::HashSet::new();
    }
    // One pass over the walk to pick out the memoized paths, rather than a scan
    // per candidate.
    let still_failing: std::collections::HashSet<String> = current_meta
        .iter()
        .filter(|fm| {
            memo.get(&fm.relative_path)
                .is_some_and(|a| a.mtime == fm.mtime && a.size == fm.size)
        })
        .map(|fm| fm.relative_path.clone())
        .collect();
    changed.retain(|path| !still_failing.contains(path));
    added.retain(|path| !still_failing.contains(path));
    still_failing
}

/// The stamps to publish for a walk, minus the files that were never built.
///
/// A published stamp means "indexed at this version". Stamping a file the delta
/// deliberately skipped would make every later reconcile see it as unchanged,
/// so it would never be indexed again — and because the stamp lands in
/// `filestamps.json`, not even a restart would recover it: every automatic
/// caller of the stale check passes `compare_index_membership = false`, which
/// is precisely the check that would have noticed the file is missing. Leaving
/// it unstamped keeps it looking new, which is what makes the retry-on-change
/// behaviour in [`drop_memoized_failures`] work at all.
fn stamps_for_indexed(
    current_meta: &[tgrep_core::walker::FileMeta],
    skipped: &std::collections::HashSet<String>,
) -> std::collections::HashMap<String, tgrep_core::meta::FileStamp> {
    current_meta
        .iter()
        .filter(|fm| !skipped.contains(&fm.relative_path))
        .map(|fm| {
            (
                fm.relative_path.clone(),
                tgrep_core::meta::FileStamp {
                    mtime: fm.mtime,
                    size: fm.size,
                },
            )
        })
        .collect()
}

/// Rebuild the ignore matcher and reconcile the index against the filesystem.
///
/// Returns whether the index can be trusted afterwards. A `false` means the
/// walk or the merge failed and the stamps are not describing the index.
fn background_refresh_stale(
    state: &Arc<ServerState>,
    root: &Path,
    index_dir: &Path,
    compare_index_membership: bool,
) -> bool {
    let refresh = state.stale_refresh_lock.lock().unwrap();
    // Keep watcher/auto-save mutations out for the complete walk → matcher →
    // merge → recovery cycle. Search queries do not take this gate and remain
    // available. Held here rather than inside so the recovery scan below is
    // still covered by it.
    let gate = state.snapshot_gate.write().unwrap();
    // One immutable tracked-file exemption is shared by the walk and the
    // matcher it publishes. Git-index rewrites cannot change ignore decisions
    // halfway through this pass.
    let ignorecase = frozen_tracked_membership(state, root);
    #[cfg(test)]
    run_stale_refresh_hook(state, StaleRefreshPhase::BeforeWalk);

    let mut newly_watched = Vec::new();
    // Before the walk, not after the subscriptions: this bounds the window the
    // recovery scan is closing, and the window opens the moment the traversal
    // that decided what to subscribe to begins.
    let since = SystemTime::now();
    let ok = refresh_stale_locked(
        state,
        root,
        index_dir,
        compare_index_membership,
        &mut newly_watched,
        ignorecase,
        true,
    );

    // Directories that were not subscribed while the walk ran could not report
    // a write, and the walk may have passed them before it happened, so a file
    // created in that window is in neither place. Recheck them now that the
    // subscriptions exist.
    //
    // Only on success, and only here at the end: a failed walk or merge leaves
    // `file_stamps` describing something other than the published index, and
    // `stream_merge_stale_changes` replaces the stamps wholesale, so scanning
    // any earlier would both be discarded and re-read every changed file.
    if ok {
        reindex_files_in(state, root, &newly_watched, since);
    }
    // Compare semantics, not index metadata. A→B→A needs no correction because
    // this pass used A throughout, while A→B schedules exactly one coalesced
    // refresh. Content-only staging cannot create an immediate refresh loop.
    let membership_changed = tracked_membership_changed(state);
    drop(gate);
    drop(refresh);
    schedule_tracked_membership_correction(state, root, membership_changed);
    ok
}

fn catch_up_unwatched_build(state: &Arc<ServerState>, root: &Path, index_dir: &Path) -> bool {
    let refresh = state.stale_refresh_lock.lock().unwrap();
    let gate = state.snapshot_gate.write().unwrap();
    let ignorecase = frozen_tracked_membership(state, root);
    let mut ignored_watches = Vec::new();
    let caught_up = refresh_stale_locked(
        state,
        root,
        index_dir,
        true,
        &mut ignored_watches,
        ignorecase,
        false,
    );
    let membership_changed = tracked_membership_changed(state);
    drop(gate);
    drop(refresh);
    schedule_tracked_membership_correction(state, root, membership_changed);
    caught_up
}

fn refresh_stale_locked(
    state: &Arc<ServerState>,
    root: &Path,
    index_dir: &Path,
    compare_index_membership: bool,
    newly_watched: &mut Vec<PathBuf>,
    ignorecase: Option<std::sync::Arc<tgrep_core::gitignore::CaseInsensitiveIgnore>>,
    run_test_hooks: bool,
) -> bool {
    use tgrep_core::meta;
    use tgrep_core::walker;

    let start = Instant::now();
    eprintln!("[trace] stale check: comparing index against filesystem...");

    // Walk first. This single traversal feeds both the stale diff and the
    // watcher's ignore matcher, and it must run before the early returns below
    // so the matcher can be published on every path out of this function.
    let walk = walker::walk_file_metadata_with_ignorecase(
        root,
        &walker::MetaWalkOptions {
            exclude_dirs: state.exclude_dirs.clone(),
            no_ignore: state.no_ignore,
            no_require_git: state.no_require_git,
            max_file_size: state.max_file_size,
        },
        ignorecase.clone(),
    );
    let walk_ms = start.elapsed().as_millis();

    // Publish the matcher immediately, before any early return can skip it.
    // Every exit below is a decision about the *index*; none of them is a
    // reason to leave the watcher gated. `gitignore_pending` is what keeps the
    // watcher off the index until a matcher exists, so leaking it past a return
    // disables the watcher permanently — and the overflow-repair path skips
    // reconciling while that flag is set, so nothing recovers it either. A
    // single unreadable directory, or one file whose `metadata()` lost a race
    // with a delete, would be enough.
    //
    // Committing here rather than at each exit is invisible to the watcher:
    // the caller holds `snapshot_gate` for write across this whole body, and
    // the only reader of `state.gitignore` takes the read side first, so no
    // event can observe the matcher before this function returns either way.
    // The caller's walk started before this publish; its timestamp is what the
    // recovery scan needs, so the one the subscription sync derives is dropped.
    *newly_watched = publish_ignore_matcher(
        state,
        root,
        ignore_sources_of(
            root,
            &walk.gitignore_files,
            &walk.ignore_files,
            state.no_require_git,
        ),
        || build_stale_matcher(state, root, &walk, ignorecase),
    );
    #[cfg(test)]
    if run_test_hooks {
        run_stale_refresh_hook(state, StaleRefreshPhase::AfterMatcherPublish);
    }
    #[cfg(not(test))]
    let _ = run_test_hooks;

    if walk.skipped_error > 0 {
        eprintln!(
            "[trace] warning: stale check could not inspect {} filesystem entries \
             (walk: {walk_ms}ms); keeping the old index",
            walk.skipped_error
        );
        return false;
    }
    let current_meta = &walk.files;
    let listed_files = &walk.listed_files;

    // Load stored per-file stamps from last index write
    let mut old_stamps = match meta::read_filestamps(index_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[trace] stale check: no filestamps found ({e}), comparing against reader");
            std::collections::HashMap::new()
        }
    };

    // Fold in stamps the watcher recorded since the last flush. `filestamps.json`
    // only advances when the index is written to disk, so mid-session it lags
    // the live overlay. Comparing against the on-disk copy alone would re-index
    // every file the watcher already handled since that write, and — worse —
    // a file created and then deleted inside that window appears in neither the
    // on-disk stamps nor the filesystem, so it would never be classified as
    // deleted and would linger in the index. The in-memory stamps are the
    // fresher record of what the index actually holds, so they win.
    for (path, stamp) in state.file_stamps.read().unwrap().iter() {
        old_stamps.insert(path.clone(), stamp.clone());
    }
    let indexed_paths = {
        let index = state.index.read().unwrap();
        let mut paths = index.reader_paths();
        paths.extend(index.live.overlay_paths());
        paths
    };
    if old_stamps.is_empty() && indexed_paths.is_empty() && current_meta.is_empty() {
        refresh_filename_index(state, index_dir, listed_files);
        eprintln!("[trace] stale check: no indexed files or filesystem files, skipping");
        return true;
    }

    let (mut changed, mut added, deleted) = classify_file_changes(
        current_meta,
        &old_stamps,
        &indexed_paths,
        compare_index_membership,
    );

    // Files that failed to read last time and have not changed since are not
    // worth another attempt; without this a single permanently locked file
    // makes every scheduled reconcile rebuild the index. `deleted` is exempt —
    // a file that is gone needs no read to evict.
    let skipped_unreadable = {
        let memo = state.unreadable.read().unwrap();
        drop_memoized_failures(&memo, current_meta, &mut changed, &mut added)
    };
    if !skipped_unreadable.is_empty() {
        eprintln!(
            "[trace] stale check: {} file(s) unchanged since they last failed to \
             read; not retrying them",
            skipped_unreadable.len()
        );
    }

    let total_changes = changed.len() + added.len() + deleted.len();
    let live_pending = state.index.read().unwrap().live.has_pending_changes();
    if total_changes == 0 && !live_pending {
        refresh_filename_index(state, index_dir, listed_files);
        eprintln!(
            "[trace] stale check: index is up-to-date ({} files checked in {}ms)",
            current_meta.len(),
            walk_ms
        );
        return true;
    }

    if total_changes == 0 {
        eprintln!(
            "[trace] stale check: metadata is unchanged, reconciling live mutations \
             (walk: {walk_ms}ms)"
        );
    } else {
        eprintln!(
            "[trace] stale check: {} changed, {} new, {} deleted (walk: {}ms)",
            changed.len(),
            added.len(),
            deleted.len(),
            walk_ms
        );
    }

    let new_stamps = stamps_for_indexed(current_meta, &skipped_unreadable);

    if !stream_merge_stale_changes(
        state,
        &changed,
        &added,
        &deleted,
        &new_stamps,
        StaleMergePolicy {
            preserved: &skipped_unreadable,
            operation: "stale check",
            authoritative_membership: true,
            authoritative_listed_files: Some(listed_files),
        },
    ) {
        return false;
    }

    true
}

/// Restore a known-empty on-disk index after a failed bootstrap.
///
/// `build_index_with_options` writes the index files in place, so a failure
/// partway through can leave truncated files that the currently mmap'd reader
/// no longer matches. Resetting gives the fallback build a clean base.
fn reset_to_empty_index(state: &ServerState, root: &Path, index_dir: &Path) {
    if let Err(e) = create_empty_index(index_dir) {
        eprintln!("[trace] warning: could not reset the index directory ({e})");
        return;
    }
    match HybridIndex::open(index_dir, root) {
        Ok(empty) => {
            let mut index = state.index.write().unwrap();
            let mut extra = state.filename_extra_paths.write().unwrap();
            let mut cache = state.cache.write().unwrap();
            *index = empty;
            extra.clear();
            state.filename_index_ready.store(false, Ordering::SeqCst);
            state.filename_index_dirty.store(false, Ordering::SeqCst);
            cache.clear();
            state.cache_generation.fetch_add(1, Ordering::SeqCst);
        }
        Err(e) => eprintln!("[trace] warning: could not reopen an empty index ({e})"),
    }
}

/// Bootstrap an empty index with the memory-bounded external merge sort.
///
/// The incremental path below accumulates every posting in the live overlay
/// before flushing, so a cold start on a large repository holds the whole
/// index in heap — on the Linux kernel tree that peaked at ~1.5 GiB. Handing a
/// true bootstrap to the builder with [`IndexStrategy::External`] bounds peak
/// memory to the arena budget instead, and is also faster, because it writes
/// the index once rather than growing an overlay and then flushing it.
///
/// The trade-off is that queries see an empty index until the build finishes
/// rather than a growing partial one. That is deliberate: results from a
/// fraction of the repository are misleading, and `status` already reports
/// that indexing is in progress.
///
/// Only used when nothing has been indexed yet. Resuming a partial index still
/// takes the incremental path, which can skip the files already on disk.
///
/// Returns `false` if the index could not be built and published, leaving the
/// caller to fall back.
fn bootstrap_index_build(state: &Arc<ServerState>, root: &Path, index_dir: &Path) -> bool {
    let start = Instant::now();
    let ignorecase = frozen_tracked_membership(state, root);
    // Anchors the recovery window at the start of the build's traversal, which
    // is the point from which writes could be missed: nothing under `root` is
    // subscribed yet, and the walk below has not reached most of it. Taking it
    // after the build — or letting the later subscription sync derive its own —
    // would exclude everything written while the build ran, which is precisely
    // the window that needs recovering.
    let since = SystemTime::now();
    eprintln!("[trace] bootstrapping index with the external merge sort (memory-bounded)...");

    // Dropped once the build is done so the sampled peak (on platforms without
    // a kernel high-water mark) covers the whole of it. Unlike the incremental
    // path below, nothing here polls memory on its own.
    let sampler = crate::mem::PrivatePeakSampler::start();
    let outcome = match builder::build_index_with_options_and_ignorecase(
        root,
        Some(index_dir),
        &builder::BuildOptions {
            include_hidden: false,
            no_ignore: state.no_ignore,
            no_require_git: state.no_require_git,
            max_file_size: state.max_file_size,
            exclude_dirs: state.exclude_dirs.clone(),
            // Match the walk `background_index_build` would have run, and the
            // dot-prefix rule `should_skip_watcher_path` applies, so the
            // watcher can maintain every file this build indexes. Also makes
            // the walk hand back the .gitignore paths for the matcher below.
            collect_gitignore_files: true,
            strategy: builder::IndexStrategy::External,
            buffer_bytes: builder::DEFAULT_INDEX_BUFFER_BYTES,
        },
        ignorecase.clone(),
    ) {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!(
                "[trace] warning: external bootstrap build failed ({e}); \
                 falling back to the in-heap build"
            );
            reset_to_empty_index(state, root, index_dir);
            return false;
        }
    };
    #[cfg(test)]
    run_stale_refresh_hook(state, StaleRefreshPhase::AfterBuildBeforeStampPublish);

    // Publish under the snapshot gate, and clear `indexing` before releasing
    // it. `handle_fs_event` only skips while `indexing` is true, so flipping
    // the flag outside the gate would let a watcher event mutate the overlay
    // against the reader we are in the middle of replacing.
    let gate = state.snapshot_gate.write().unwrap();
    let opened = match HybridIndex::open(index_dir, root) {
        Ok(index) => index,
        Err(e) => {
            drop(gate);
            eprintln!(
                "[trace] warning: bootstrapped index failed to open ({e}); \
                 falling back to the in-heap build"
            );
            reset_to_empty_index(state, root, index_dir);
            return false;
        }
    };
    let indexed = opened.num_files() as u64;
    let filename_extra_paths = match tgrep_core::path_index::read_extra_paths(index_dir) {
        Ok(Some(paths)) => Some(paths.into_iter().collect()),
        Ok(None) => {
            eprintln!("[trace] warning: bootstrapped index has no filename sidecar");
            None
        }
        Err(error) => {
            eprintln!("[trace] warning: bootstrapped filename index failed to load: {error}");
            None
        }
    };
    {
        let mut index = state.index.write().unwrap();
        let mut extra = state.filename_extra_paths.write().unwrap();
        let mut cache = state.cache.write().unwrap();
        *index = opened;
        if let Some(paths) = filename_extra_paths {
            *extra = paths;
            state.filename_index_ready.store(true, Ordering::SeqCst);
            state.filename_index_dirty.store(false, Ordering::SeqCst);
        }
        cache.clear();
        state.cache_generation.fetch_add(1, Ordering::SeqCst);
    }
    state.index_total.store(indexed, Ordering::Relaxed);
    state.index_progress.store(indexed, Ordering::Relaxed);

    // Everything the watcher consults must be in place before `indexing` goes
    // false, since that flag is the only thing keeping `handle_fs_event` off
    // the index. Without the stamps it would reindex on spurious events;
    // without the matcher it would happily index gitignored paths that the
    // build just skipped.
    //
    // Both come out of the build itself: the builder persisted filestamps.json,
    // and its walk handed back the .gitignore / .ignore paths. Building the
    // matcher from those is what keeps this cheap — `gitignore::build_matcher`
    // would rewalk the whole tree, which cost 49 s on a 289k-file repo.
    let stamps = match tgrep_core::meta::read_filestamps(index_dir) {
        // Minus the paths whose events arrived while the build ran: the builder
        // read those files at some point during its walk and stamped what it
        // saw, so for anything written afterwards the stamp describes bytes the
        // index does not hold. See `withhold_stamps_for_deferred`.
        Ok(stamps) if state.watch_enabled => withhold_stamps_for_deferred(state, root, stamps),
        Ok(_) => std::collections::HashMap::new(),
        Err(e) => {
            eprintln!(
                "[trace] warning: could not load file stamps ({e}); \
                 the watcher may reindex on spurious events"
            );
            std::collections::HashMap::new()
        }
    };
    if !state.watch_enabled
        && let Err(e) = tgrep_core::meta::write_filestamps(&stamps, index_dir)
    {
        drop(gate);
        eprintln!(
            "[trace] warning: could not prepare unwatched bootstrap catch-up ({e}); \
             falling back to the in-heap build"
        );
        reset_to_empty_index(state, root, index_dir);
        return false;
    }
    *state.file_stamps.write().unwrap() = stamps;
    let mut newly_watched = Vec::new();
    if !state.no_ignore {
        let t_gi = Instant::now();
        // "Newly watched" here is every directory in the repository, and the
        // build's walk ran before any of them were subscribed. Deferred rather
        // than skipped: the scan waits out `indexing` and then costs one
        // `metadata` call per file, since the stamps this build just wrote
        // describe the index exactly.
        newly_watched = publish_ignore_matcher(
            state,
            root,
            ignore_sources_of(
                root,
                &outcome.gitignore_files,
                &outcome.ignore_files,
                state.no_require_git,
            ),
            || {
                tgrep_core::walker::build_gitignore_matcher_from_files_with_ignorecase(
                    root,
                    &outcome.gitignore_files,
                    &outcome.ignore_files,
                    state.no_require_git,
                    ignorecase,
                )
            },
        );
        let found = state.gitignore.read().unwrap().is_some();
        eprintln!(
            "[trace] gitignore matcher built from {} file(s) in {:.1}ms{}",
            outcome.gitignore_files.len(),
            t_gi.elapsed().as_secs_f64() * 1000.0,
            if found { "" } else { " (no rules found)" }
        );
    }
    // Outside that block, because it also drains the events the watcher had to
    // discard while this build ran, and those pile up whether or not there are
    // ignore rules to publish.
    if state.watch_enabled {
        spawn_recovery_scan(state, root, newly_watched, since);
    }

    if state.watch_enabled {
        state.indexing.store(false, Ordering::SeqCst);
    }
    drop(gate);
    if !state.watch_enabled {
        if !catch_up_unwatched_build(state, root, index_dir) {
            eprintln!(
                "[trace] warning: unwatched bootstrap catch-up was incomplete; \
                 falling back to the in-heap build"
            );
            reset_to_empty_index(state, root, index_dir);
            return false;
        }
        state.indexing.store(false, Ordering::SeqCst);
    }
    let membership_changed = tracked_membership_changed(state);
    schedule_tracked_membership_correction(state, root, membership_changed);

    let elapsed = start.elapsed().as_secs_f64();
    drop(sampler);
    match crate::mem::format_peak_memory() {
        Some(peak) => eprintln!(
            "[trace] bootstrap complete: {indexed} files indexed in {elapsed:.1}s \
             (peak memory {peak})"
        ),
        None => eprintln!("[trace] bootstrap complete: {indexed} files indexed in {elapsed:.1}s"),
    }

    if state.ignore_rules_dirty.load(Ordering::SeqCst) {
        schedule_ignore_rules_refresh(Arc::clone(state), root.to_path_buf());
    }
    true
}

/// Walk the repo and populate the LiveIndex in batches in a background thread.
/// Uses rayon for parallel trigram extraction. The bulk build is held entirely
/// in the live overlay; only one final flush to disk happens once the walk
/// completes. This avoids the super-linear cost of repeatedly snapshotting an
/// ever-growing reader+overlay during indexing, and lets us release the live
/// overlay's allocations once the data is safely on disk.
///
/// Trade-off: a crash during the initial build loses all in-progress work
/// (no intermediate checkpoint to fall back to). The file watcher and
/// auto-save loop continue to protect ongoing changes after the initial
/// build completes.
fn background_index_build(state: &Arc<ServerState>, root: &Path, index_dir: &Path) {
    use rayon::prelude::*;
    use tgrep_core::walker::{self, WalkOptions};

    const BATCH_SIZE: usize = 500;

    let start = Instant::now();
    eprintln!("[trace] background indexing started...");

    // Build skip set from existing on-disk reader (for incremental indexing)
    let skip_paths = {
        let index = state.index.read().unwrap();
        let paths = index.reader_paths();
        if !paths.is_empty() {
            eprintln!(
                "[trace] seeding from existing index ({} files already indexed)",
                paths.len()
            );
        }
        paths
    };
    let seeded_count = skip_paths.len() as u64;

    // Nothing indexed yet: build straight to disk with bounded memory instead
    // of accumulating the whole repo in the live overlay. Resuming a partial
    // index falls through, since that path can skip what is already on disk.
    if skip_paths.is_empty() && bootstrap_index_build(state, root, index_dir) {
        return;
    }

    // Phase 1: Walk file paths (no content reads)
    let t_walk = Instant::now();
    let ignorecase = frozen_tracked_membership(state, root);
    // The recovery window opens with this traversal, not with the subscriptions
    // it later feeds: a nested `.ignore` written after the walk read its parent
    // directory but before the matcher is published is invisible to both, and a
    // timestamp taken any later would date it as already accounted for.
    let since = SystemTime::now();
    let walk = walker::walk_dir_with_ignorecase(
        root,
        &WalkOptions {
            include_hidden: false,
            no_ignore: state.no_ignore,
            no_require_git: state.no_require_git,
            max_file_size: state.max_file_size,
            collect_gitignore_files: !state.no_ignore,
            exclude_dirs: state.exclude_dirs.clone(),
            ..Default::default()
        },
        ignorecase.clone(),
    );

    let mut newly_watched = Vec::new();
    if !state.no_ignore {
        let start = Instant::now();
        // Subscriptions are taken here, partway through the build, so files
        // written to a directory the walk has already passed are in neither
        // the build's results nor any event. The scan waits for the build to
        // finish before looking, because until then the stamps describe
        // nothing and every file would read as changed.
        newly_watched = publish_ignore_matcher(
            state,
            root,
            ignore_sources_of(
                root,
                &walk.gitignore_files,
                &walk.ignore_files,
                state.no_require_git,
            ),
            || {
                walker::build_gitignore_matcher_from_files_with_ignorecase(
                    root,
                    &walk.gitignore_files,
                    &walk.ignore_files,
                    state.no_require_git,
                    ignorecase,
                )
            },
        );
        let has_matcher = state.gitignore.read().unwrap().is_some();
        eprintln!(
            "[trace] gitignore matcher built from index walk in {:.1}ms \
             ({} .gitignore + {} .ignore files{})",
            start.elapsed().as_secs_f64() * 1000.0,
            walk.gitignore_files.len(),
            walk.ignore_files.len(),
            if has_matcher { "" } else { ", no rules found" }
        );
    }
    // Outside that block: the scan also drains the events discarded while this
    // build ran, which accumulate with or without ignore rules.
    if state.watch_enabled {
        spawn_recovery_scan(state, root, newly_watched, since);
    }

    // Filter out already-indexed files
    let new_files: Vec<_> = if skip_paths.is_empty() {
        walk.files
    } else {
        walk.files
            .into_iter()
            .filter(|path| {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                !skip_paths.contains(&rel)
            })
            .collect()
    };

    let new_count = new_files.len() as u64;
    let total = seeded_count + new_count;
    state
        .index_total
        .store(total, std::sync::atomic::Ordering::Relaxed);
    state
        .index_progress
        .store(seeded_count, std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "[trace] walk complete: {} new files to index ({} already indexed, {} binary skipped, {} too large, {} errors) in {:.1}ms",
        new_count,
        seeded_count,
        walk.skipped_binary,
        walk.skipped_too_large,
        walk.skipped_error,
        t_walk.elapsed().as_secs_f64() * 1000.0
    );

    // Phase 2: Process new files in parallel batches.
    //
    // Confine the CPU-heavy file-read + trigram-extraction work to a bounded
    // worker pool (sized from the `--max-cpu` budget) so the initial build
    // doesn't saturate every core and starve the host. Falls back to the
    // global rayon pool if a dedicated pool can't be built.
    let index_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(state.index_threads)
        .thread_name(|i| format!("tgrep-index-{i}"))
        .build()
        .ok();
    if index_pool.is_some() {
        eprintln!(
            "[trace] indexing with {} worker thread(s)",
            state.index_threads
        );
    }

    let mut incremental_flushes = 0u32;
    for (batch_idx, batch) in new_files.chunks(BATCH_SIZE).enumerate() {
        // Parallel: read files + extract trigrams (no locks held). Run inside
        // the bounded pool when available so indexing CPU stays capped.
        let extract = || {
            batch
                .par_iter()
                .filter_map(|path| {
                    let data = std::fs::read(path).ok()?;
                    let data = tgrep_core::encoding::decode_for_index(&data);
                    if tgrep_core::trigram::is_binary(&data) {
                        return None;
                    }
                    let rel_path = path
                        .strip_prefix(root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/");

                    let mut trigrams = tgrep_core::trigram::extract(&data);
                    let lower = data.to_ascii_lowercase();
                    if lower != *data {
                        trigrams.extend(tgrep_core::trigram::extract(&lower));
                    }
                    Some((rel_path, trigrams))
                })
                .collect::<Vec<(String, Vec<u32>)>>()
        };
        let batch_results: Vec<(String, Vec<u32>)> = match &index_pool {
            Some(pool) => pool.install(extract),
            None => extract(),
        };

        // Sequential: insert into LiveIndex (brief write lock per batch)
        {
            let mut index = state.index.write().unwrap();
            for (rel_path, trigrams) in batch_results {
                index.live.upsert_file_with_trigrams(&rel_path, trigrams);
            }
        }

        let progress =
            seeded_count as usize + ((batch_idx + 1) * BATCH_SIZE).min(new_count as usize);
        state
            .index_progress
            .store(progress as u64, std::sync::atomic::Ordering::Relaxed);

        if progress % 5000 < BATCH_SIZE {
            eprintln!(
                "[trace] indexing progress: ~{progress}/{total} files ({:.1}s elapsed)",
                start.elapsed().as_secs_f64()
            );
        }

        // Memory-bounded build: if the in-heap overlay has pushed memory past
        // the budget, persist what we've indexed so far to disk and reclaim the
        // heap before continuing. This keeps peak memory bounded (the flush
        // copies existing on-disk postings verbatim from mmap rather than into
        // heap) while still converging to a *complete* index — unlike simply
        // stopping, which would leave a partial index.
        //
        // Charged against private bytes, not the working set: the overlay is
        // heap, and that is what a flush can give back. Mapped index pages sit
        // in the working set too but are file-backed, so counting them would
        // fire the cap on memory no flush can reclaim.
        if let Some(used) = crate::mem::budgeted_memory_bytes()
            && used > state.memory_cap_bytes
        {
            eprintln!(
                "[trace] memory cap reached ({} MB in use > {} MB cap) — flushing \
                 overlay to disk to reclaim memory and continuing",
                used / (1024 * 1024),
                state.memory_cap_bytes / (1024 * 1024),
            );
            if flush_append_only_overlay(state, index_dir, false, None) {
                incremental_flushes += 1;
                let mut index = state.index.write().unwrap();
                index.live.shrink_to_fit();
            } else {
                eprintln!(
                    "[trace] warning: incremental flush did not reclaim memory; \
                     continuing (build may still exceed the budget)"
                );
            }
        }
    }

    eprintln!(
        "[trace] background indexing complete: {} total files ({} new, {} seeded, \
         {} incremental flushes) in {:.1}s",
        total,
        new_count,
        seeded_count,
        incremental_flushes,
        start.elapsed().as_secs_f64()
    );
    #[cfg(test)]
    run_stale_refresh_hook(state, StaleRefreshPhase::AfterBuildBeforeStampPublish);

    // Walk filesystem metadata BEFORE the flush so we can publish the
    // resulting per-file stamps atomically with the index files. Writing
    // them after a successful flush would leave a multi-minute window where
    // the index looks fully published but `filestamps.json` is missing — a
    // server kill in that window disables incremental stale detection on
    // the next start.
    let walk_meta = tgrep_core::walker::walk_file_metadata(
        root,
        &tgrep_core::walker::MetaWalkOptions {
            exclude_dirs: state.exclude_dirs.clone(),
            no_ignore: state.no_ignore,
            no_require_git: state.no_require_git,
            max_file_size: state.max_file_size,
        },
    );
    let listed_files = walk_meta.listed_files;
    let stamps: std::collections::HashMap<String, tgrep_core::meta::FileStamp> = {
        let indexed = {
            let index = state.index.read().unwrap();
            let mut paths = index.reader_paths();
            paths.extend(index.live.overlay_paths());
            paths
        };
        stamps_for_index_members(walk_meta.files, &indexed)
    };

    // The in-memory build is done — surface "complete" in status now even
    // though the final disk flush below can take minutes for very large
    // repos. Set `flushing` *before* clearing `indexing` so the auto-save
    // loop never observes both flags as false during the handoff and
    // races us into a redundant parallel snapshot of the bulk overlay.
    //
    // Acquire the publish gate *before* clearing `indexing`. `handle_fs_event`
    // only skips while `indexing` is true; once it's false a watcher event can
    // run, and if it grabbed `snapshot_gate.read()` before our flush grabbed
    // the write lock it could mutate the overlay in the gap between the flag
    // flip and the final snapshot — updating/deleting a path already on disk in
    // the reader and violating `append_overlay_to_index`'s brand-new-paths
    // precondition. Holding the gate across the flip makes any such event block
    // (not skip) until the flush publishes, after which it applies safely to
    // the newly published reader; no event is lost.
    let gate = state.snapshot_gate.write().unwrap();
    replace_filename_extra_paths(state, &listed_files);
    state.flushing.store(true, Ordering::SeqCst);

    // Publish the stamps *before* clearing `indexing`, not after the flush.
    // The recovery scan started at publish time waits for `indexing` to clear
    // and then blocks on this gate, so it runs the instant the gate drops. If
    // the stamps were still unpublished at that point every file it walked
    // would compare as changed and it would re-read the entire repository —
    // the exact work the wait exists to avoid — and the assignment would then
    // overwrite the stamps it had just recorded for anything that really did
    // change during the build, losing them until the next reconcile.
    //
    // Done even if the flush below fails: the live overlay already reflects
    // what was just indexed, and the stamps describe that.
    //
    // Minus whatever changed underneath the walk, which the stamps would
    // otherwise describe as indexed when the index holds the older bytes.
    *state.file_stamps.write().unwrap() = if state.watch_enabled {
        withhold_stamps_for_deferred(state, root, stamps)
    } else {
        std::collections::HashMap::new()
    };
    if state.watch_enabled {
        state.indexing.store(false, Ordering::SeqCst);
    }

    // Final flush to disk for the bulk build. Use the same streaming
    // append-only path as incremental flushes so the final complete publish
    // does not materialize the whole reader+overlay in heap and violate the
    // memory cap. This always publishes with `complete = true`; any
    // intermediate incremental flushes published `complete = false` so a
    // mid-build kill would resume rather than be treated as finished.
    eprintln!("[trace] persisting final index to disk...");
    let pruned = {
        // A read guard rather than a clone: these maps hold an entry per file
        // in the repo. Nothing reachable from the flush takes this lock, and
        // every other writer is behind the publish gate we hold.
        let stamps = state.file_stamps.read().unwrap();
        flush_append_only_overlay_locked(state, index_dir, true, Some(&stamps))
    };
    drop(gate);

    state.flushing.store(false, Ordering::SeqCst);
    if !state.watch_enabled {
        if !catch_up_unwatched_build(state, root, index_dir) {
            state.ignore_rules_dirty.store(true, Ordering::SeqCst);
            eprintln!(
                "[trace] warning: unwatched background-build catch-up was incomplete; \
                 leaving stamps invalid for the scheduled stale check"
            );
        }
        state.indexing.store(false, Ordering::SeqCst);
    }

    // Reclaim memory held by the indexing-time live overlay — but only when
    // the flush actually completed and `prune_persisted_entries` ran. If the
    // flush failed, the overlay is still the source of truth and shrinking
    // the indexing-sized maps would just waste the write lock with no benefit.
    if pruned {
        let mut index = state.index.write().unwrap();
        index.live.shrink_to_fit();
    }

    let membership_changed = tracked_membership_changed(state);
    schedule_tracked_membership_correction(state, root, membership_changed);
    if state.ignore_rules_dirty.load(Ordering::SeqCst) {
        schedule_ignore_rules_refresh(Arc::clone(state), root.to_path_buf());
    }
}

/// Memory-bounded append-only flush used during the initial bulk build.
///
/// Unlike building a full reader+overlay snapshot in heap (which costs
/// O(total index size) memory), this streams the live overlay onto disk via
/// [`builder::append_overlay_to_index`]: the existing postings are copied
/// verbatim from the reader's mmap and never enter the heap. Peak heap stays
/// bounded to the overlay snapshot, so repeated checkpoint flushes and the
/// final complete publish keep the whole build under the memory budget.
///
/// Relies on the bulk-build invariant that the overlay is **append-only**
/// (watcher + auto-save suppressed while `indexing == true`), so every overlay
/// file is new and the merge is a pure append.
///
/// Checkpoint flushes pass `complete = false`: a kill mid-build must leave the
/// index marked partial so the next start resumes indexing the remaining files.
/// The final end-of-build flush passes `complete = true` and may pass file
/// stamps to publish alongside the index.
///
/// Returns `true` if the new reader was published and the overlay pruned.
fn flush_append_only_overlay(
    state: &ServerState,
    index_dir: &Path,
    complete: bool,
    stamps: Option<&std::collections::HashMap<String, tgrep_core::meta::FileStamp>>,
) -> bool {
    // Hold the snapshot gate for the whole snapshot → publish → prune cycle.
    // During the bulk build the watcher is already suppressed, but auto-save
    // coordination and future-proofing make the gate the right call.
    let _gate = state.snapshot_gate.write().unwrap();
    flush_append_only_overlay_locked(state, index_dir, complete, stamps)
}

/// Body of [`flush_append_only_overlay`] that assumes `snapshot_gate` is
/// **already held for write** by the caller. Split out so the final bulk-build
/// handoff can acquire the gate *before* clearing the `indexing` flag, closing
/// the window where a filesystem event could observe `indexing == false`, take
/// the gate first, and mutate the overlay between the flag flip and the final
/// snapshot (which would break the append-only precondition).
fn flush_append_only_overlay_locked(
    state: &ServerState,
    index_dir: &Path,
    complete: bool,
    stamps: Option<&std::collections::HashMap<String, tgrep_core::meta::FileStamp>>,
) -> bool {
    let flush_start = Instant::now();

    // Snapshot the overlay (bounded heap) and the current reader (cheap Arc).
    let (overlay_paths, overlay_inverted, reader) = {
        let index = state.index.read().unwrap();
        let (paths, inverted) = index.live.snapshot_for_disk();
        (paths, inverted, index.reader_arc())
    };
    if overlay_paths.is_empty() && !complete && stamps.is_none() {
        return false;
    }
    let num_files = reader.num_files() + overlay_paths.len();

    let staging_dir = index_dir.with_file_name(".tgrep_flush_staging");
    let _ = std::fs::remove_dir_all(&staging_dir);

    // Stream-merge overlay onto the existing on-disk index. Incremental
    // checkpoint flushes publish `complete = false`; the final bulk-build flush
    // republishes the same stream with `complete = true` and stamps.
    if let Err(e) = builder::append_overlay_to_index(
        &state.root,
        &staging_dir,
        &reader,
        &overlay_paths,
        &overlay_inverted,
        complete,
    ) {
        eprintln!("[trace] warning: append-only flush write failed: {e}");
        let _ = std::fs::remove_dir_all(&staging_dir);
        return false;
    }

    // Stage filestamps alongside the final complete index. If this fails we
    // still publish the index: losing incremental stale-check state on next
    // start is preferable to dropping the completed build.
    if let Some(stamps) = stamps
        && let Err(e) = tgrep_core::meta::write_filestamps(stamps, &staging_dir)
    {
        eprintln!("[trace] warning: failed to write staging filestamps: {e}");
    }

    let pruned = publish_staged_index(
        state,
        index_dir,
        &staging_dir,
        num_files,
        &[],
        &std::collections::HashSet::new(),
        None,
    )
    .is_published();
    eprintln!(
        "[trace] append-only flush: {num_files} files on disk (complete={complete}) in {:.1}s",
        flush_start.elapsed().as_secs_f64()
    );
    pruned
}

/// Publish a staged index directory: move the staged files into `index_dir`,
/// reopen the on-disk reader (with Windows stale-NTFS-metadata retries),
/// validate + warm it, swap it in without blocking searches, and prune the
/// now-persisted overlay entries.
///
/// Shared by stale refresh and [`flush_append_only_overlay`]. The `publish_lock`
/// is held across move + open + swap so concurrent publishers cannot interleave
/// renames or swap readers out of order. `num_files` is the expected on-disk
/// file count used to reject a partially-published reader.
///
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublishStatus {
    Published,
    Failed,
    RollbackFailed,
}

impl PublishStatus {
    fn is_published(self) -> bool {
        matches!(self, Self::Published)
    }
}

/// Returns the publication outcome. A rollback failure preserves the staging
/// directory so its backups remain available for recovery.
fn publish_staged_index(
    state: &ServerState,
    index_dir: &Path,
    staging_dir: &Path,
    num_files: usize,
    invalidate_paths: &[String],
    preserve_overlay_paths: &std::collections::HashSet<String>,
    filename_extra_paths: Option<&std::collections::HashSet<String>>,
) -> PublishStatus {
    let stage_filename_index =
        filename_extra_paths.is_some() || state.filename_index_ready.load(Ordering::SeqCst);
    let filename_stage_result = if let Some(paths) = filename_extra_paths {
        let mut paths: Vec<String> = paths.iter().cloned().collect();
        paths.sort_unstable();
        tgrep_core::path_index::write_extra_paths(staging_dir, &paths).map_err(Into::into)
    } else if state.filename_index_ready.load(Ordering::SeqCst) {
        stage_filename_extra_paths(state, staging_dir)
    } else {
        Ok(())
    };
    if let Err(error) = filename_stage_result {
        eprintln!("[trace] warning: filename index staging failed: {error}");
        let _ = std::fs::remove_dir_all(staging_dir);
        return PublishStatus::Failed;
    }

    // Held across move + open + swap so concurrent publishers (auto-save /
    // background-build / watcher reindex flush) cannot interleave renames
    // or swap readers out of order. Searches do not take this lock.
    let _publish = state.publish_lock.lock().unwrap();
    let mut moved = match move_staged_files(staging_dir, index_dir) {
        Ok(moved) => moved,
        Err(e) => {
            eprintln!("[trace] warning: flush move failed: {e}");
            return if e.rollback_failed() {
                PublishStatus::RollbackFailed
            } else {
                PublishStatus::Failed
            };
        }
    };
    let Some((new_reader, reader_files, reader_trigrams)) =
        open_published_reader(index_dir, num_files)
    else {
        return match moved.rollback() {
            Ok(()) => {
                let _ = std::fs::remove_dir_all(staging_dir);
                PublishStatus::Failed
            }
            Err(e) => {
                moved.preserve();
                eprintln!("[trace] warning: failed to roll back index publication: {e}");
                PublishStatus::RollbackFailed
            }
        };
    };

    // Hold the index write lock through cache invalidation so searches cannot
    // observe the new posting set with bytes from the old cache generation.
    {
        let mut index = state.index.write().unwrap();
        if let Some(paths) = filename_extra_paths {
            *state.filename_extra_paths.write().unwrap() = paths.clone();
            state.filename_index_ready.store(true, Ordering::SeqCst);
        }
        if stage_filename_index {
            state.filename_index_dirty.store(false, Ordering::SeqCst);
        }
        let mut cache = state.cache.write().unwrap();
        index.swap_reader(new_reader);
        index.prune_persisted_entries_except(preserve_overlay_paths);
        index.live.reset_dirty_count();
        for path in invalidate_paths {
            cache.pop(path);
        }
        if !invalidate_paths.is_empty() {
            state.cache_generation.fetch_add(1, Ordering::SeqCst);
        }
    }
    moved.commit();
    eprintln!(
        "[trace] flush: reader reopened ({reader_files} files, \
         {reader_trigrams} trigrams), overlay pruned"
    );
    let _ = std::fs::remove_dir_all(staging_dir);
    PublishStatus::Published
}

fn publish_reloaded_index(
    state: &ServerState,
    index_dir: &Path,
    staging_dir: &Path,
    num_files: usize,
) -> bool {
    let filename_extra_paths = match tgrep_core::path_index::read_extra_paths(staging_dir) {
        Ok(Some(paths)) => paths.into_iter().collect(),
        Ok(None) => {
            eprintln!("[trace] warning: reloaded index has no filename sidecar");
            let _ = std::fs::remove_dir_all(staging_dir);
            return false;
        }
        Err(error) => {
            eprintln!("[trace] warning: reloaded filename index failed to open: {error}");
            let _ = std::fs::remove_dir_all(staging_dir);
            return false;
        }
    };
    let _publish = state.publish_lock.lock().unwrap();
    let mut moved = match move_staged_files(staging_dir, index_dir) {
        Ok(moved) => moved,
        Err(e) => {
            eprintln!("[trace] warning: reload move failed: {e}");
            return false;
        }
    };
    let Some((new_reader, reader_files, reader_trigrams)) =
        open_published_reader(index_dir, num_files)
    else {
        match moved.rollback() {
            Ok(()) => {
                let _ = std::fs::remove_dir_all(staging_dir);
            }
            Err(e) => {
                moved.preserve();
                eprintln!("[trace] warning: failed to roll back reload publication: {e}");
            }
        }
        return false;
    };

    // Searches take the outer index lock before consulting the cache. Holding
    // both in that order makes the complete reload visible as one generation.
    {
        let mut index = state.index.write().unwrap();
        *state.filename_extra_paths.write().unwrap() = filename_extra_paths;
        state.filename_index_ready.store(true, Ordering::SeqCst);
        state.filename_index_dirty.store(false, Ordering::SeqCst);
        let mut cache = state.cache.write().unwrap();
        index.swap_reader(new_reader);
        let mut reconciled = index.live.overlay_paths();
        reconciled.extend(index.live.tombstone_paths());
        index.live.clear_reconciled_paths(&reconciled);
        index.live.reset_dirty_count();
        cache.clear();
        state.cache_generation.fetch_add(1, Ordering::SeqCst);
    }
    moved.commit();
    eprintln!(
        "[trace] reload: reader reopened ({reader_files} files, \
         {reader_trigrams} trigrams), overlay and cache cleared"
    );
    let _ = std::fs::remove_dir_all(staging_dir);
    true
}

fn open_published_reader(
    index_dir: &Path,
    num_files: usize,
) -> Option<(tgrep_core::reader::IndexReader, usize, usize)> {
    const READER_OPEN_RETRIES: u32 = 5;
    const READER_OPEN_BACKOFF: Duration = Duration::from_millis(200);

    for attempt in 0..READER_OPEN_RETRIES {
        match tgrep_core::reader::IndexReader::open(index_dir) {
            Ok(new_reader) => {
                let reader_files = new_reader.num_files();
                let reader_trigrams = new_reader.num_trigrams();
                if new_reader.is_degenerate() {
                    eprintln!(
                        "[trace] warning: reader has {reader_files} files but 0 trigrams \
                         (attempt {}/{READER_OPEN_RETRIES}, likely stale NTFS metadata)",
                        attempt + 1
                    );
                } else if let Err(msg) = new_reader.validate_lookup() {
                    eprintln!(
                        "[trace] warning: reader validation failed \
                         (attempt {}/{READER_OPEN_RETRIES}): {msg}",
                        attempt + 1
                    );
                } else if reader_files >= num_files {
                    return Some((new_reader, reader_files, reader_trigrams));
                } else {
                    eprintln!(
                        "[trace] warning: reader has {reader_files} files \
                         (expected {num_files}), keeping live overlay as fallback"
                    );
                    return None;
                }
            }
            Err(e) => {
                eprintln!(
                    "[trace] warning: reader open failed (attempt {}/{READER_OPEN_RETRIES}): {e}",
                    attempt + 1
                );
            }
        }
        if attempt + 1 < READER_OPEN_RETRIES {
            thread::sleep(READER_OPEN_BACKOFF * (attempt + 1));
        }
    }
    eprintln!(
        "[trace] warning: failed to validate reader after \
         {READER_OPEN_RETRIES} attempts, keeping the previous reader"
    );
    None
}

const INDEX_FILE_NAMES: &[&str] = &[
    "index.bin",
    "lookup.bin",
    "files.bin",
    tgrep_core::path_index::EXTRA_PATHS_FILENAME,
    "filestamps.json",
    "meta.json",
];

struct StagedFileMove {
    staging: PathBuf,
    target: PathBuf,
    backed_up: Vec<&'static str>,
    published: Vec<&'static str>,
    finished: bool,
}

impl StagedFileMove {
    fn backup_path(&self, name: &str) -> PathBuf {
        self.staging.join(format!(".previous-{name}"))
    }

    fn rollback(&mut self) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }
        let mut first_error = None;
        let mut pending_published = Vec::new();
        for name in std::mem::take(&mut self.published).into_iter().rev() {
            let published = self.target.join(name);
            if let Err(e) = std::fs::remove_file(&published)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                if first_error.is_none() {
                    first_error = Some(e);
                }
                pending_published.push(name);
            }
        }
        pending_published.reverse();
        self.published = pending_published;

        let mut pending_backups = Vec::new();
        for name in std::mem::take(&mut self.backed_up).into_iter().rev() {
            match publish_file(&self.backup_path(name), &self.target.join(name)) {
                Ok(()) => self.published.retain(|published| *published != name),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.published.retain(|published| *published != name);
                }
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                    pending_backups.push(name);
                }
            }
        }
        pending_backups.reverse();
        self.backed_up = pending_backups;
        if let Some(e) = first_error {
            return Err(e);
        }
        self.finished = true;
        Ok(())
    }

    fn commit(&mut self) {
        self.finished = true;
    }

    fn preserve(&mut self) {
        // Leave every remaining backup and published file exactly where the
        // failed rollback left it so an operator or later recovery can use it.
        self.finished = true;
    }

    fn fail(mut self, publish: std::io::Error) -> MoveStagedFilesError {
        let rollback = self.rollback().err();
        if rollback.is_some() {
            self.preserve();
        }
        MoveStagedFilesError { publish, rollback }
    }
}

impl Drop for StagedFileMove {
    fn drop(&mut self) {
        if let Err(e) = self.rollback() {
            eprintln!("[trace] warning: failed to roll back staged index files: {e}");
        }
    }
}

#[derive(Debug)]
struct MoveStagedFilesError {
    publish: std::io::Error,
    rollback: Option<std::io::Error>,
}

impl MoveStagedFilesError {
    fn rollback_failed(&self) -> bool {
        self.rollback.is_some()
    }
}

impl std::fmt::Display for MoveStagedFilesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.publish)?;
        if let Some(rollback) = &self.rollback {
            write!(f, "; rollback also failed: {rollback}")?;
        }
        Ok(())
    }
}

impl std::error::Error for MoveStagedFilesError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.publish)
    }
}

/// Move index files from staging to the target directory, retaining backups
/// until the caller validates and commits the new reader.
///
/// Files are published in a fixed order, with `meta.json` last. Existing files
/// are retained in staging until reader validation succeeds, so dropping the
/// returned transaction rolls back a partial or rejected publication.
///
/// Performance note: this function runs under the server's `publish_lock`
/// (which serializes concurrent publishers) but does NOT take the
/// `state.index` write lock, so search queries continue to be served
/// throughout. Each per-file move uses `std::fs::rename` — on the same
/// volume this is an O(microseconds) directory entry update, vs
/// `std::fs::copy` which is O(file_size) and on a large `index.bin`
/// (hundreds of MB) can take tens of seconds. Staging dirs are always
/// created next to the target (same parent) so cross-volume cases should
/// not arise; if rename truly fails, the error is surfaced rather than
/// silently falling back to a slow copy (see `publish_file`).
fn move_staged_files(
    staging: &Path,
    target: &Path,
) -> Result<StagedFileMove, MoveStagedFilesError> {
    std::fs::create_dir_all(target).map_err(|publish| MoveStagedFilesError {
        publish,
        rollback: None,
    })?;
    let mut moved = StagedFileMove {
        staging: staging.to_path_buf(),
        target: target.to_path_buf(),
        backed_up: Vec::new(),
        published: Vec::new(),
        finished: false,
    };
    for &name in INDEX_FILE_NAMES {
        let src = staging.join(name);
        let dst = target.join(name);
        if !src.exists() {
            continue;
        }
        if dst.exists() {
            if let Err(error) = publish_file(&dst, &moved.backup_path(name)) {
                return Err(moved.fail(error));
            }
            moved.backed_up.push(name);
        }
        if let Err(error) = publish_file(&src, &dst) {
            return Err(moved.fail(error));
        }
        moved.published.push(name);
    }
    Ok(moved)
}

/// Publish a single staged file at `src` to `dst`.
///
/// Uses `std::fs::rename`, which on the same volume is an O(microseconds)
/// directory entry update — this is the property that keeps the server's
/// index write lock from being held for the duration of a multi-hundred-MB
/// file copy (which previously blocked all search queries).
///
/// On Windows, transient sharing violations (`ERROR_SHARING_VIOLATION` = 32,
/// `ERROR_LOCK_VIOLATION` = 33) can occur after dropping an mmap (cache
/// manager / AV / indexers may briefly hold a reference), so retry only
/// those specific error codes for a short window. All other errors fail
/// fast — a broader retry surface would needlessly extend the publish
/// window for non-transient failures.
///
/// Deliberately does NOT fall back to `std::fs::copy` on persistent failure:
/// the caller holds the index write lock and a multi-hundred-MB copy is
/// exactly the pathology we are fixing. Staging is always created next to
/// the target, so cross-volume cases should not arise; if rename truly
/// cannot succeed, surfacing the error lets the caller abort cleanly
/// rather than silently regress search latency.
/// Context wrapper that preserves the original `std::io::Error` as the
/// `source()` of the returned error so callers can downcast through the
/// chain to inspect `raw_os_error()` for diagnostics.
#[derive(Debug)]
struct PublishError {
    ctx: String,
    source: std::io::Error,
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Include the underlying error in the formatted message for
        // human-readable logging; structured access remains via `source()`.
        write!(f, "{}: {}", self.ctx, self.source)
    }
}

impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn publish_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    const RENAME_RETRIES: u32 = 30;
    const RENAME_BACKOFF: Duration = Duration::from_millis(50);
    // Windows error codes that can transiently occur when another handle
    // (mmap section, AV scanner, indexer) still references the target file:
    //   ERROR_SHARING_VIOLATION = 32
    //   ERROR_LOCK_VIOLATION    = 33
    // Other errors (NotFound, permission/ACL issues, disk full, …) are
    // structural and should fail fast so we don't extend the publish window.
    #[cfg(windows)]
    const TRANSIENT_WIN_ERRORS: &[i32] = &[32, 33];

    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..RENAME_RETRIES {
        match std::fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) => {
                #[cfg(windows)]
                let transient =
                    matches!(e.raw_os_error(), Some(c) if TRANSIENT_WIN_ERRORS.contains(&c));
                #[cfg(not(windows))]
                let transient = false;

                if !transient || attempt + 1 == RENAME_RETRIES {
                    // Wrap with a context error that preserves the original
                    // `std::io::Error` as the `source()` of the returned
                    // error, so callers can downcast through the chain to
                    // recover `raw_os_error()` for diagnostics.
                    let ctx = format!(
                        "publish_file: rename({}, {}) failed after {} attempt(s)",
                        src.display(),
                        dst.display(),
                        attempt + 1,
                    );
                    let kind = e.kind();
                    return Err(std::io::Error::new(kind, PublishError { ctx, source: e }));
                }
                last_err = Some(e);
                thread::sleep(RENAME_BACKOFF);
            }
        }
    }
    // Unreachable: the loop either returns Ok, or returns Err on the last
    // iteration. Defensive return preserves the last error.
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::other("publish_file: rename retries exhausted with no error recorded")
    }))
}

fn ctrlc_handler<F: Fn() + Send + Sync + 'static>(handler: F) {
    #[cfg(windows)]
    {
        use std::sync::OnceLock;
        static HANDLER: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
        HANDLER.get_or_init(|| Box::new(handler));

        unsafe extern "system" fn console_handler(_ctrl_type: u32) -> i32 {
            if let Some(h) = HANDLER.get() {
                h();
            }
            1 // TRUE - we handled the event
        }

        unsafe extern "system" {
            fn SetConsoleCtrlHandler(
                handler: unsafe extern "system" fn(u32) -> i32,
                add: i32,
            ) -> i32;
        }

        // SAFETY: SetConsoleCtrlHandler is a stable Win32 API. The handler function
        // is extern "system" with correct signature, and HANDLER is 'static.
        unsafe {
            SetConsoleCtrlHandler(console_handler, 1);
        }
    }

    #[cfg(not(windows))]
    {
        use std::sync::OnceLock;
        static HANDLER: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
        HANDLER.get_or_init(|| Box::new(handler));

        unsafe extern "C" fn signal_handler(_sig: std::ffi::c_int) {
            if let Some(h) = HANDLER.get() {
                h();
            }
        }

        unsafe extern "C" {
            fn signal(sig: std::ffi::c_int, handler: unsafe extern "C" fn(std::ffi::c_int));
        }

        // SAFETY: signal() is a POSIX API. The handler has the correct extern "C"
        // signature, and HANDLER is 'static. SIGINT (2) is valid on all Unix.
        // SIGINT = 2 on all Unix platforms
        unsafe {
            signal(2, signal_handler);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cached(len: usize) -> Arc<DecodedFile> {
        // `String::from_utf8` preserves the Vec's capacity, so `heap_bytes`
        // is exactly `len` and the assertions below can use round numbers.
        let mut bytes = Vec::with_capacity(len);
        bytes.resize(len, b'a');
        Arc::new(DecodedFile {
            text: String::from_utf8(bytes).unwrap(),
            fixups: Default::default(),
        })
    }

    #[test]
    fn content_cache_evicts_to_stay_under_byte_budget() {
        let mut cache = ContentCache::new(CACHE_CAPACITY, 1000, 1000);
        for i in 0..10 {
            cache.put(format!("f{i}"), cached(200));
        }
        assert!(cache.byte_len() <= 1000, "bytes = {}", cache.byte_len());
        assert_eq!(cache.len(), 5);
        // Oldest entries went first; the newest survive.
        assert!(cache.peek("f0").is_none());
        assert!(cache.peek("f9").is_some());
    }

    #[test]
    fn content_cache_refuses_oversized_entries() {
        let mut cache = ContentCache::new(CACHE_CAPACITY, 1000, 100);
        cache.put("small".into(), cached(50));
        cache.put("huge".into(), cached(500));
        assert!(cache.peek("huge").is_none(), "oversized entry was admitted");
        // Admitting it would also have evicted the useful entry.
        assert!(cache.peek("small").is_some());
        assert_eq!(cache.byte_len(), 50);
    }

    /// The byte total must stay exact across every path that removes an entry,
    /// including the entry-count eviction that `LruCache` performs internally.
    #[test]
    fn content_cache_byte_accounting_stays_exact() {
        let mut cache = ContentCache::new(2, u64::MAX, u64::MAX);
        cache.put("a".into(), cached(100));
        cache.put("b".into(), cached(100));
        assert_eq!(cache.byte_len(), 200);

        // Capacity is 2, so this evicts "a" inside the LRU.
        cache.put("c".into(), cached(100));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.byte_len(), 200, "count-eviction leaked bytes");

        // Replacing an existing key must not double-count.
        cache.put("c".into(), cached(300));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.byte_len(), 400);

        cache.pop("c");
        assert_eq!(cache.byte_len(), 100);
        cache.clear();
        assert_eq!(cache.byte_len(), 0);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn content_cache_touch_promotes_recency() {
        let mut cache = ContentCache::new(CACHE_CAPACITY, 300, 300);
        cache.put("a".into(), cached(100));
        cache.put("b".into(), cached(100));
        cache.touch("a");
        // "b" is now least-recently-used, so it is the one that goes.
        cache.put("c".into(), cached(100));
        cache.put("d".into(), cached(100));
        assert!(cache.peek("a").is_some(), "touched entry was evicted first");
        assert!(cache.peek("b").is_none());
    }

    /// A `ServerState` over an empty index, for exercising the stale path
    /// directly. Mirrors the defaults `run` uses with a watcher and ignore
    /// rules enabled, which is the configuration `gitignore_pending` gates.
    fn test_server_state(root: &Path, index_dir: &Path) -> Arc<ServerState> {
        create_empty_index(index_dir).expect("create empty index");
        let hybrid = HybridIndex::open(index_dir, root).expect("open empty index");
        Arc::new(ServerState {
            index: RwLock::new(hybrid),
            filename_extra_paths: RwLock::new(Default::default()),
            filename_index_ready: std::sync::atomic::AtomicBool::new(false),
            filename_index_dirty: std::sync::atomic::AtomicBool::new(false),
            cache: RwLock::new(ContentCache::new(
                CACHE_CAPACITY,
                CACHE_MAX_BYTES,
                CACHE_MAX_ENTRY_BYTES,
            )),
            cache_generation: std::sync::atomic::AtomicU64::new(0),
            root: root.to_path_buf(),
            watcher_active: std::sync::atomic::AtomicBool::new(false),
            indexing: std::sync::atomic::AtomicBool::new(false),
            flushing: std::sync::atomic::AtomicBool::new(false),
            gitignore_pending: std::sync::atomic::AtomicBool::new(true),
            ignore_rules_dirty: std::sync::atomic::AtomicBool::new(false),
            ignore_refresh_scheduled: std::sync::atomic::AtomicBool::new(false),
            tracked_membership: Mutex::new(None),
            watch_resubscribe: std::sync::atomic::AtomicBool::new(false),
            ignore_sources: RwLock::new(Vec::new()),
            ignore_source_stamps: RwLock::new(IgnoreStamps::new()),
            reindex_lock: Mutex::new(()),
            deferred_events: Mutex::new(Some(std::collections::HashMap::new())),
            index_progress: std::sync::atomic::AtomicU64::new(0),
            index_total: std::sync::atomic::AtomicU64::new(0),
            watch_enabled: true,
            watch_registry: Mutex::new(None),
            exclude_dirs: Vec::new(),
            no_ignore: false,
            no_require_git: false,
            max_file_size: None,
            index_dir: index_dir.to_path_buf(),
            publish_lock: Mutex::new(()),
            file_stamps: RwLock::new(Default::default()),
            snapshot_gate: RwLock::new(()),
            stale_refresh_lock: Mutex::new(()),
            gitignore: RwLock::new(None),
            memory_cap_bytes: 16 * 1024 * 1024 * 1024,
            index_threads: 1,
            auto_save_mutations: 0,
            unreadable: RwLock::new(std::collections::HashMap::new()),
            started: Instant::now(),
            last_search_ms: std::sync::atomic::AtomicU64::new(0),
            stale_refresh_hook: Mutex::new(None),
        })
    }

    fn test_git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn files_rpc_unions_content_and_filename_only_paths() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("root");
        let index_dir = temp.path().join("index");
        std::fs::create_dir_all(&root).unwrap();
        let state = test_server_state(&root, &index_dir);
        state
            .index
            .write()
            .unwrap()
            .live
            .upsert_file("src/main.rs", b"fn main() {}\n");
        state
            .filename_extra_paths
            .write()
            .unwrap()
            .insert("asset.bin".to_string());
        state.filename_index_ready.store(true, Ordering::SeqCst);

        let response = process_request(r#"{"jsonrpc":"2.0","method":"files","id":1}"#, &state);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            value.pointer("/result/files").unwrap(),
            &serde_json::json!(["asset.bin", "src/main.rs"])
        );
    }

    #[test]
    fn reload_replaces_content_and_filename_paths_together() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("root");
        let index_dir = temp.path().join("index");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("new.bin"), [1, 2, 3]).unwrap();
        let state = test_server_state(&root, &index_dir);
        state
            .filename_extra_paths
            .write()
            .unwrap()
            .insert("removed.bin".to_string());

        let response = handle_reload(Some(serde_json::json!(1)), &state);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value.pointer("/result/status").unwrap(), "reloaded");
        assert!(state.filename_index_ready.load(Ordering::SeqCst));

        let response = handle_files(Some(serde_json::json!(2)), &state);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            value.pointer("/result/files").unwrap(),
            &serde_json::json!(["main.rs", "new.bin"])
        );
    }

    #[test]
    fn watcher_keeps_binary_extensions_in_the_filename_index() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("root");
        let index_dir = temp.path().join("index");
        std::fs::create_dir_all(&root).unwrap();
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);
        state.filename_index_ready.store(true, Ordering::SeqCst);

        let asset = root.join("asset.bin");
        std::fs::write(&asset, [1, 2, 3]).unwrap();
        let create =
            Event::new(EventKind::Create(notify::event::CreateKind::File)).add_path(asset.clone());
        handle_fs_event(&state, &root, &create);
        assert!(
            state
                .filename_extra_paths
                .read()
                .unwrap()
                .contains("asset.bin")
        );
        assert!(state.index.read().unwrap().all_paths().is_empty());
        let dirty = state.index.read().unwrap().live.dirty_count();

        let duplicate =
            Event::new(EventKind::Modify(notify::event::ModifyKind::Any)).add_path(asset.clone());
        handle_fs_event(&state, &root, &duplicate);
        assert_eq!(
            state.index.read().unwrap().live.dirty_count(),
            dirty,
            "a duplicate event dirtied the binary path again"
        );

        std::fs::remove_file(&asset).unwrap();
        let remove = Event::new(EventKind::Remove(notify::event::RemoveKind::File)).add_path(asset);
        handle_fs_event(&state, &root, &remove);
        assert!(
            !state
                .filename_extra_paths
                .read()
                .unwrap()
                .contains("asset.bin")
        );
    }

    #[test]
    fn authoritative_filename_refresh_retries_a_dirty_sidecar() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("root");
        let index_dir = temp.path().join("index");
        std::fs::create_dir_all(&root).unwrap();
        let state = test_server_state(&root, &index_dir);
        state
            .filename_extra_paths
            .write()
            .unwrap()
            .insert("asset.bin".to_string());
        state.filename_index_ready.store(true, Ordering::SeqCst);
        state.filename_index_dirty.store(true, Ordering::SeqCst);
        tgrep_core::path_index::write_extra_paths(&index_dir, &[]).unwrap();

        refresh_filename_index(state.as_ref(), &index_dir, &["asset.bin".to_string()]);

        assert_eq!(
            tgrep_core::path_index::read_extra_paths(&index_dir).unwrap(),
            Some(vec!["asset.bin".to_string()])
        );
        assert!(!state.filename_index_dirty.load(Ordering::SeqCst));
    }

    #[test]
    fn watcher_recovery_sweeps_missing_filename_only_paths() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("root");
        let index_dir = temp.path().join("index");
        std::fs::create_dir_all(root.join("assets")).unwrap();
        let state = test_server_state(&root, &index_dir);
        state
            .filename_extra_paths
            .write()
            .unwrap()
            .insert("assets/missing.bin".to_string());
        state.filename_index_ready.store(true, Ordering::SeqCst);

        sweep_removed_files(
            state.as_ref(),
            &std::collections::HashSet::from(["assets".to_string()]),
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        );

        assert!(
            !state
                .filename_extra_paths
                .read()
                .unwrap()
                .contains("assets/missing.bin")
        );
        assert!(
            state
                .index
                .read()
                .unwrap()
                .live
                .is_deleted("assets/missing.bin")
        );
        assert!(state.filename_index_dirty.load(Ordering::SeqCst));
    }

    /// A binary marker's offset is a position in the file, not in the repaired
    /// text.
    ///
    /// Lossy decoding widens every invalid byte to a three-byte U+FFFD, so a
    /// NUL preceded by invalid UTF-8 sits further along the searched text than
    /// it does on disk. `rg 15.2.0` reports the on-disk byte (verified: this
    /// fixture gives `found "\0" byte around offset 9`).
    ///
    /// The mapping has to happen here because the client never reads a file the
    /// server searched, so it has no fixups of its own to map with.
    #[test]
    fn binary_marker_offset_is_mapped_back_to_the_source_bytes() {
        let mut bytes = vec![0xFF, 0xFF];
        bytes.extend_from_slice(b"needle\n");
        bytes.push(0);
        bytes.extend_from_slice(b"tail\n");
        let nul_on_disk = bytes.iter().position(|&b| b == 0).unwrap();
        assert_eq!(nul_on_disk, 9, "fixture changed");

        let file = DecodedFile::new(bytes, tgrep_core::encoding::EncodingMode::Auto);
        assert_eq!(
            file.text.as_bytes().iter().position(|&b| b == 0),
            Some(13),
            "the fixture must actually shift the offset, or this proves nothing"
        );

        let matcher = crate::matching::build_search_matcher(
            &["needle".to_string()],
            &crate::matching::MatcherConfig::default(),
        )
        .unwrap();

        let rows = search_file_matches("f.txt", &file, &matcher, &SearchOpts::default()).unwrap();

        let marker = rows
            .iter()
            .find(|r| r["type"] == "binary")
            .expect("a file containing a NUL is reported as binary");
        assert_eq!(
            marker["offset"].as_u64(),
            Some(nul_on_disk as u64),
            "offset must be the byte on disk (9), not the decoded position \
             (13): {marker}"
        );
    }

    /// `reset_to_empty_index` runs after a failed build, when the index
    /// directory is least likely to be intact, so it must not assume the
    /// directory survived.
    #[test]
    fn create_empty_index_makes_its_own_directory() {
        let tmp = TempDir::new().unwrap();
        let index_dir = tmp.path().join("missing").join("idx");
        assert!(!index_dir.exists());

        create_empty_index(&index_dir).expect("should create the directory it writes into");

        assert!(index_dir.join("lookup.bin").is_file());
        assert!(index_dir.join("index.bin").is_file());
        assert!(index_dir.join("files.bin").is_file());
        // A caller that already created the directory must still succeed.
        create_empty_index(&index_dir).expect("should be idempotent");
    }

    /// A truncated index left by a failed build must be replaced, not reused.
    #[test]
    fn create_empty_index_replaces_partially_written_files() {
        let tmp = TempDir::new().unwrap();
        let index_dir = tmp.path().join("idx");
        std::fs::create_dir_all(&index_dir).unwrap();
        std::fs::write(index_dir.join("lookup.bin"), b"truncated garbage").unwrap();

        create_empty_index(&index_dir).unwrap();

        assert_eq!(
            std::fs::read(index_dir.join("lookup.bin")).unwrap().len(),
            0
        );
        HybridIndex::open(&index_dir, tmp.path()).expect("reset index should reopen cleanly");
    }

    #[test]
    fn skip_watcher_path_skips_dot_components() {
        let no_exclude: Vec<String> = Vec::new();
        // A leading dot dir is the canonical case (.git, .hg, .svn, ...).
        assert!(should_skip_watcher_path(
            ".git/index.lock",
            &no_exclude,
            None
        ));
        assert!(should_skip_watcher_path(".git/HEAD", &no_exclude, None));
        assert!(should_skip_watcher_path(
            ".hg/store/data",
            &no_exclude,
            None
        ));
        // A dot component anywhere in the path skips, not just the leading one.
        assert!(should_skip_watcher_path(
            "src/.cache/build.tmp",
            &no_exclude,
            None
        ));
        assert!(should_skip_watcher_path(
            "a/b/.hidden/c.txt",
            &no_exclude,
            None
        ));
    }

    #[test]
    fn skip_watcher_path_keeps_non_hidden_paths() {
        let no_exclude: Vec<String> = Vec::new();
        assert!(!should_skip_watcher_path("src/main.rs", &no_exclude, None));
        assert!(!should_skip_watcher_path("README.md", &no_exclude, None));
        // A dot mid-segment (e.g. "foo.bar") is NOT a hidden component —
        // only segments that *start* with `.` are hidden.
        assert!(!should_skip_watcher_path("src/foo.bar", &no_exclude, None));
        assert!(!should_skip_watcher_path("a/b/c", &no_exclude, None));
    }

    #[test]
    fn skip_watcher_path_honors_exclude_dirs() {
        let exclude = vec!["target".to_string(), "node_modules".to_string()];
        // Excluded name as an ancestor directory => skip (matches what the
        // walker would do — it skips the whole subtree).
        assert!(should_skip_watcher_path("target/debug/foo", &exclude, None));
        assert!(should_skip_watcher_path(
            "node_modules/react/index.js",
            &exclude,
            None
        ));
        assert!(should_skip_watcher_path("a/target/b", &exclude, None));
        // Substring match should NOT trigger — "targets" != "target".
        assert!(!should_skip_watcher_path("targets/foo", &exclude, None));
        // Unrelated paths are not skipped.
        assert!(!should_skip_watcher_path("src/main.rs", &exclude, None));
    }

    #[test]
    fn skip_watcher_path_does_not_match_basename_against_exclude_dirs() {
        // A regular file whose basename happens to equal an excluded
        // directory name (e.g. a file literally called `vendor` at the
        // repo root, or `src/target`) is still indexed by the walker —
        // walker only treats `exclude_dirs` as directory subtree filters.
        // The watcher must match that, otherwise the in-memory index and
        // the on-disk index would disagree.
        let exclude = vec!["target".to_string(), "vendor".to_string()];
        assert!(!should_skip_watcher_path("vendor", &exclude, None));
        assert!(!should_skip_watcher_path("src/target", &exclude, None));
        assert!(!should_skip_watcher_path("a/b/vendor", &exclude, None));
    }

    #[test]
    fn skip_watcher_path_handles_dot_segments_and_empty() {
        let no_exclude: Vec<String> = Vec::new();
        // `.` and `..` are not "hidden" components — they're path-relative
        // markers and should not trigger a skip on their own.
        assert!(!should_skip_watcher_path("./foo.txt", &no_exclude, None));
        assert!(!should_skip_watcher_path("a/./b", &no_exclude, None));
        assert!(!should_skip_watcher_path("a/../b", &no_exclude, None));
        // An empty rel_path (root-level event) shouldn't panic or skip.
        assert!(!should_skip_watcher_path("", &no_exclude, None));
    }

    #[test]
    fn skip_watcher_path_honors_gitignore_matcher() {
        // Build the matcher via the public tgrep-core helper so this test
        // also exercises the shared loading logic.
        let tmp = TempDir::new().unwrap();
        // `.gitignore` is git-gated, matching the indexing walk, so the
        // matcher only picks it up inside a repo.
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let gi_path = tmp.path().join(".gitignore");
        std::fs::write(&gi_path, "*.log\ntarget/\n").unwrap();
        let gi = tgrep_core::gitignore::build_matcher(tmp.path())
            .expect("matcher should build from a non-empty .gitignore");

        let no_exclude: Vec<String> = Vec::new();
        // Files matched by the gitignore are skipped.
        assert!(should_skip_watcher_path(
            "build/output.log",
            &no_exclude,
            Some(&gi)
        ));
        assert!(should_skip_watcher_path(
            "target/release/foo",
            &no_exclude,
            Some(&gi)
        ));
        // Files NOT matched by the gitignore are not skipped.
        assert!(!should_skip_watcher_path(
            "src/main.rs",
            &no_exclude,
            Some(&gi)
        ));
        assert!(!should_skip_watcher_path(
            "README.md",
            &no_exclude,
            Some(&gi)
        ));
    }

    #[test]
    fn incomplete_watch_sync_preserves_existing_subscriptions_until_a_complete_pass() {
        // These two must not be confused. `watch_new_subtree` learns only
        // about the subtree that just appeared, so if it went through `sync`
        // every directory outside that subtree would look stale and the server
        // would unsubscribe from the entire rest of the repository — turning a
        // new folder into a silent, total loss of file watching.
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let c = tmp.path().join("c");
        for dir in [&a, &b, &c] {
            std::fs::create_dir(dir).unwrap();
        }

        let watcher = notify::recommended_watcher(|_: notify::Result<Event>| {}).unwrap();
        let mut registry = WatchRegistry {
            watcher,
            root: tmp.path().to_path_buf(),
            watched: std::collections::HashSet::new(),
        };

        let added = registry.add_all(&[a.clone(), b.clone()]);
        assert_eq!(added.len(), 2);

        // Adding a subtree leaves existing subscriptions untouched, and
        // re-adding one already present is a no-op rather than a duplicate.
        let added = registry.add_all(&[b.clone(), c.clone()]);
        assert_eq!(
            added,
            vec![c.clone()],
            "b was already watched and must not be re-added"
        );
        assert_eq!(
            registry.watched,
            [a.clone(), b.clone(), c.clone()].into_iter().collect(),
            "add_all dropped a subscription outside the set it was given"
        );

        // A traversal that missed entries is additive only: absence from its
        // partial result is not evidence that an existing watch became stale.
        let partial: std::collections::HashSet<PathBuf> = std::iter::once(c.clone()).collect();
        let pending = std::sync::atomic::AtomicBool::new(true);
        let force = take_force_resubscribe(&pending, TraversalCompleteness::Incomplete);
        assert!(
            !force,
            "the incomplete pass must not re-register known entries"
        );
        assert!(
            pending.load(Ordering::SeqCst),
            "an incomplete pass must leave forced resubscription pending"
        );
        let (added, removed) = registry.sync(&partial, TraversalCompleteness::Incomplete, force);
        assert_eq!((added.len(), removed), (0, 0));
        assert_eq!(
            registry.watched,
            [a.clone(), b.clone(), c.clone()].into_iter().collect(),
            "an incomplete desired set retired valid existing subscriptions"
        );

        // A later complete traversal is authoritative and may prune them.
        let force = take_force_resubscribe(&pending, TraversalCompleteness::Complete);
        assert!(
            force,
            "the next complete pass must inherit the force request"
        );
        assert!(
            !pending.load(Ordering::SeqCst),
            "a complete forced pass consumes the request"
        );
        let (added, removed) = registry.sync(&partial, TraversalCompleteness::Complete, force);
        assert_eq!((added.len(), removed), (0, 2));
        assert_eq!(registry.watched, [c].into_iter().collect());
    }

    #[test]
    fn a_recreated_directory_is_subscribed_again_rather_than_assumed_watched() {
        // The kernel releases an inotify watch by itself when its directory is
        // deleted or moved away, and says nothing about it. A path recreated at
        // the same location therefore *looks* subscribed while receiving no
        // events — and because it is in `desired` as well as in `watched`, no
        // later `sync` can tell the difference either. The entry stays poisoned
        // for the life of the process, so the directory silently stops being
        // watched forever.
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir(&a).unwrap();

        let watcher = notify::recommended_watcher(|_: notify::Result<Event>| {}).unwrap();
        let mut registry = WatchRegistry {
            watcher,
            root: tmp.path().to_path_buf(),
            watched: std::collections::HashSet::new(),
        };
        assert_eq!(registry.add_all(std::slice::from_ref(&a)).len(), 1);

        // What the removal event does. Without it the entry below survives.
        registry.forget(&a);
        assert!(
            !registry.watched.contains(&a),
            "a removed directory must not be left recorded as watched"
        );
        assert_eq!(
            registry.add_all(std::slice::from_ref(&a)).len(),
            1,
            "a directory recreated after removal must be subscribed again"
        );

        // And the belt-and-braces half: even with the entry still present —
        // a move away delivers no event for the descendants it takes with it —
        // a directory that has just appeared gets its subscription re-issued.
        // Already-known paths are not reported as new, so the recovery scan
        // does not treat the whole subtree as freshly watched.
        assert!(
            registry
                .resubscribe_all(std::slice::from_ref(&a))
                .is_empty(),
            "re-issuing a subscription must not report an existing path as new"
        );
        assert!(registry.watched.contains(&a));
    }

    #[cfg(unix)]
    #[test]
    fn is_real_dir_rejects_a_symlink_to_a_directory() {
        // `Path::is_dir` follows links, so it would report a symlinked
        // directory as a directory and the watcher would subscribe to and
        // index the link's target — a tree the walker never descends into, and
        // one that can sit entirely outside the repository root.
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(is_real_dir(&real));
        assert!(
            link.is_dir(),
            "precondition: is_dir follows the link, which is the trap"
        );
        assert!(!is_real_dir(&link));
        assert!(!is_real_dir(&tmp.path().join("missing")));
        let file = tmp.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(!is_real_dir(&file));
    }

    #[test]
    fn watchable_dirs_prunes_ignored_and_hidden_subtrees() {
        // The point of the subscription set: an ignored directory costs one
        // inotify watch descriptor per directory inside it, so pruning has to
        // happen before the subtree is walked, not after its events arrive.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();

        for dir in [
            "src",
            "src/nested",
            "build",
            "build/a",
            "build/a/deep",
            ".git/objects",
            "vendor",
            "vendor/pkg",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }

        let gi = tgrep_core::gitignore::build_matcher(root).expect("matcher should build");
        let exclude = vec!["vendor".to_string()];
        let dirs = watchable_dirs(root, root, &exclude, Some(&gi));

        let rel: std::collections::HashSet<String> = dirs
            .dirs
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();

        // The root itself is always watched, plus the directories the indexer
        // would descend into.
        assert!(rel.contains(""), "root must always be watched: {rel:?}");
        assert!(rel.contains("src"));
        assert!(rel.contains("src/nested"));

        // A gitignored directory and everything beneath it.
        assert!(
            !rel.contains("build"),
            "gitignored dir was watched: {rel:?}"
        );
        assert!(!rel.contains("build/a"));
        assert!(!rel.contains("build/a/deep"));

        // Hidden directories, which the walker skips too.
        assert!(!rel.contains(".git"));
        assert!(!rel.contains(".git/objects"));

        // `--exclude` names prune the directory itself, not just its children.
        assert!(!rel.contains("vendor"), "excluded dir was watched: {rel:?}");
        assert!(!rel.contains("vendor/pkg"));
    }

    #[test]
    fn watchable_dirs_without_a_matcher_keeps_everything_visible() {
        // `--no-ignore` publishes no matcher. The subscription set must then
        // be the whole tree minus hidden paths, matching what the walk indexes;
        // silently narrowing it would drop events for files that ARE indexed.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("build/a")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();

        let dirs = watchable_dirs(root, root, &[], None);
        let rel: std::collections::HashSet<String> = dirs
            .dirs
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();

        assert!(rel.contains("build"));
        assert!(rel.contains("build/a"));
        assert!(!rel.contains(".hidden"));
    }

    #[test]
    fn watchable_dirs_anchors_rules_at_the_root_not_the_start_directory() {
        // A subtree that appears at runtime is walked from itself, but the
        // ignore rules are written against paths relative to the repository
        // root. Walking with the subtree as the anchor would test "nested"
        // against a rule meant for "src/nested" and prune the wrong things.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "src/fresh/skipped/\n/keep/\n").unwrap();

        for dir in ["src/fresh/skipped", "src/fresh/keep", "src/fresh/kept"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }

        let gi = tgrep_core::gitignore::build_matcher(root).expect("matcher should build");
        let dirs = watchable_dirs(root, &root.join("src/fresh"), &[], Some(&gi));
        let rel: std::collections::HashSet<String> = dirs
            .dirs
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();

        assert!(
            rel.contains("src/fresh"),
            "start dir must be watched: {rel:?}"
        );
        assert!(rel.contains("src/fresh/kept"));
        assert!(
            !rel.contains("src/fresh/skipped"),
            "a root-anchored rule was not applied: {rel:?}"
        );
        // `keep/` is anchored at the root, so it must NOT prune
        // `src/fresh/keep` just because the walk started at `src/fresh`.
        assert!(
            rel.contains("src/fresh/keep"),
            "a root-anchored rule was applied at the wrong depth: {rel:?}"
        );
    }

    #[test]
    fn rejected_never_indexed_files_do_not_dirty_the_overlay() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        let rejected_extension = root.join("asset.png");
        let rejected_content = root.join("binary.rs");
        std::fs::write(
            &rejected_extension,
            "not indexed despite textual contents\n",
        )
        .unwrap();
        std::fs::write(&rejected_content, b"fn looks_textual() {}\n\0binary\n").unwrap();

        let _gate = state.snapshot_gate.read().unwrap();
        reindex_file(&state, &rejected_extension, "asset.png", false);
        reindex_file(&state, &rejected_content, "binary.rs", false);

        let index = state.index.read().unwrap();
        assert_eq!(
            index.live.dirty_count(),
            0,
            "paths absent from reader, overlay, and stamps must not create tombstones"
        );
        assert!(!index.live.is_deleted("asset.png"));
        assert!(!index.live.is_deleted("binary.rs"));
    }

    #[test]
    fn unreadable_stale_delta_preserves_reader_and_overlay_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        let path = root.join("raced.rs");
        std::fs::write(&path, "fn old_reader_marker() {}\n").unwrap();
        builder::build_index_for_files(&root, &index_dir, std::slice::from_ref(&path), 1024)
            .unwrap();
        *state.index.write().unwrap() = HybridIndex::open(&index_dir, &root).unwrap();
        assert!(state.index.read().unwrap().reader_has_path("raced.rs"));

        std::fs::write(&path, "fn preserved_overlay_marker() {}\n").unwrap();
        {
            let _gate = state.snapshot_gate.read().unwrap();
            reindex_file(&state, &path, "raced.rs", false);
        }
        assert!(state.index.read().unwrap().live.has_path("raced.rs"));
        let stamps = state.file_stamps.read().unwrap().clone();
        std::fs::remove_file(&path).unwrap();
        {
            let _gate = state.snapshot_gate.write().unwrap();
            assert!(stream_merge_stale_changes(
                &state,
                &["raced.rs".to_string()],
                &[],
                &[],
                &stamps,
                StaleMergePolicy {
                    preserved: &std::collections::HashSet::new(),
                    operation: "test stale check",
                    authoritative_membership: true,
                },
            ));
        }
        {
            let index = state.index.read().unwrap();
            assert!(index.reader_has_path("raced.rs"));
            assert!(index.live.has_path("raced.rs"));
        }
        assert!(state.unreadable.read().unwrap().contains_key("raced.rs"));

        // A later merge may still need to publish unrelated live work. The
        // memoized failure must remain excluded from its authoritative removal
        // set even though its stamp was deliberately withheld.
        let other = root.join("other.rs");
        std::fs::write(&other, "fn unrelated_overlay_marker() {}\n").unwrap();
        {
            let _gate = state.snapshot_gate.read().unwrap();
            reindex_file(&state, &other, "other.rs", false);
        }
        let stamps = state.file_stamps.read().unwrap().clone();
        let preserved = std::collections::HashSet::from(["raced.rs".to_string()]);
        {
            let _gate = state.snapshot_gate.write().unwrap();
            assert!(stream_merge_stale_changes(
                &state,
                &[],
                &[],
                &[],
                &stamps,
                StaleMergePolicy {
                    preserved: &preserved,
                    operation: "test retry",
                    authoritative_membership: true,
                },
            ));
        }

        let index = state.index.read().unwrap();
        assert!(index.reader_has_path("raced.rs"));
        assert!(index.live.has_path("raced.rs"));
        assert!(!index.live.is_deleted("raced.rs"));
        assert!(index.reader_has_path("other.rs"));
    }

    #[test]
    fn reader_path_without_a_stamp_is_still_tombstoned() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        let indexed = root.join("legacy.rs");
        std::fs::write(&indexed, "fn legacy_reader_entry() {}\n").unwrap();
        builder::build_index_for_files(&root, &index_dir, std::slice::from_ref(&indexed), 1024)
            .unwrap();
        *state.index.write().unwrap() = HybridIndex::open(&index_dir, &root).unwrap();
        assert!(
            state.file_stamps.read().unwrap().is_empty(),
            "fixture requires a reader entry with no stamp"
        );
        assert!(
            state.index.read().unwrap().reader_has_path("legacy.rs"),
            "fixture did not put the path in the active reader"
        );

        let _gate = state.snapshot_gate.read().unwrap();
        let _reindex = lock_reindex(&state);
        drop_indexed_file(&state, "legacy.rs", "test rejection");

        let index = state.index.read().unwrap();
        assert!(
            index.live.is_deleted("legacy.rs"),
            "reader membership must remain sufficient evidence to tombstone"
        );
        assert_eq!(index.live.dirty_count(), 1);
    }

    #[test]
    fn content_only_git_index_rewrite_is_ignored_but_membership_changes_reconcile() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["config", "core.ignorecase", "true"]);
        std::fs::write(root.join(".gitignore"), "IGNORED/\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn original() {}\n").unwrap();
        std::fs::create_dir_all(root.join("ignored")).unwrap();
        std::fs::write(root.join("ignored/forced.rs"), "fn forced() {}\n").unwrap();
        test_git(&root, &["add", "--", ".gitignore", "src/lib.rs"]);

        let state = test_server_state(&root, &root.join(".tgrep"));
        let matcher =
            tgrep_core::gitignore::build_matcher(&root).expect("case-insensitive matcher");
        let baseline = matcher
            .tracked_membership_fingerprint()
            .expect("tracked exemption active");
        *state.gitignore.write().unwrap() = Some(matcher);
        *state.tracked_membership.lock().unwrap() = Some(baseline);

        std::fs::write(root.join("src/lib.rs"), "fn content_changed() {}\n").unwrap();
        test_git(&root, &["add", "--", "src/lib.rs"]);
        assert!(
            !tracked_membership_changed(&state),
            "rewriting index metadata with the same tracked paths must not request a full scan"
        );

        std::fs::write(root.join("src/new.rs"), "fn newly_tracked() {}\n").unwrap();
        test_git(&root, &["add", "--", "src/new.rs"]);
        assert!(
            tracked_membership_changed(&state),
            "conservative polling must notice every tracked-membership change"
        );
        assert!(
            !tracked_membership_changed(&state),
            "an observed membership change must be coalesced"
        );

        test_git(&root, &["add", "-f", "--", "ignored/forced.rs"]);
        assert!(
            tracked_membership_changed(&state),
            "adding a tracked-path exemption must request reconciliation"
        );
        assert!(
            !tracked_membership_changed(&state),
            "an observed membership change must be coalesced"
        );

        test_git(
            &root,
            &[
                "rm",
                "--cached",
                "--quiet",
                "--force",
                "--",
                "ignored/forced.rs",
            ],
        );
        assert!(
            tracked_membership_changed(&state),
            "removing a tracked-path exemption must request reconciliation"
        );
    }

    #[test]
    fn first_matcher_publication_uses_one_snapshot_across_git_index_aba() {
        use std::sync::Barrier;
        use std::sync::atomic::AtomicUsize;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["config", "core.ignorecase", "true"]);
        std::fs::write(root.join(".gitignore"), "IGNORED/\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn ordinary() {}\n").unwrap();
        let ignored = root.join("ignored");
        std::fs::create_dir_all(&ignored).unwrap();
        std::fs::write(ignored.join("forced.rs"), "fn aba_marker() {}\n").unwrap();
        test_git(&root, &["add", "--", ".gitignore", "src/lib.rs"]);

        let mut state = test_server_state(&root, &root.join(".tgrep"));
        Arc::get_mut(&mut state).unwrap().watch_enabled = false;
        state.gitignore_pending.store(false, Ordering::SeqCst);
        assert!(
            state.gitignore.read().unwrap().is_none(),
            "the race must cover first matcher publication"
        );

        if PER_DIRECTORY_WATCHES {
            let watcher = notify::recommended_watcher(|_: notify::Result<Event>| {}).unwrap();
            *state.watch_registry.lock().unwrap() = Some(WatchRegistry {
                watcher,
                root: root.clone(),
                watched: std::iter::once(root.clone()).collect(),
            });
        }

        let before_walk_entered = Arc::new(Barrier::new(2));
        let before_walk_release = Arc::new(Barrier::new(2));
        let after_publish_entered = Arc::new(Barrier::new(2));
        let after_publish_release = Arc::new(Barrier::new(2));
        let passes = Arc::new(AtomicUsize::new(0));
        let hook: StaleRefreshHook = {
            let before_walk_entered = Arc::clone(&before_walk_entered);
            let before_walk_release = Arc::clone(&before_walk_release);
            let after_publish_entered = Arc::clone(&after_publish_entered);
            let after_publish_release = Arc::clone(&after_publish_release);
            let passes = Arc::clone(&passes);
            Arc::new(move |phase| match phase {
                StaleRefreshPhase::BeforeWalk => {
                    if passes.fetch_add(1, Ordering::SeqCst) == 0 {
                        before_walk_entered.wait();
                        before_walk_release.wait();
                    }
                }
                StaleRefreshPhase::AfterMatcherPublish => {
                    if passes.load(Ordering::SeqCst) == 1 {
                        after_publish_entered.wait();
                        after_publish_release.wait();
                    }
                }
                StaleRefreshPhase::AfterBuildBeforeStampPublish => {}
                StaleRefreshPhase::AfterConcreteRead => {}
                StaleRefreshPhase::BeforeConcreteCommit => {}
            })
        };
        *state.stale_refresh_hook.lock().unwrap() = Some(hook);

        let refresh_state = Arc::clone(&state);
        let refresh_root = root.clone();
        let index_dir = state.index_dir.clone();
        let refresh = thread::spawn(move || {
            background_refresh_stale(&refresh_state, &refresh_root, &index_dir, true)
        });

        // A: untracked and hidden when the pass captures its immutable set.
        before_walk_entered.wait();
        // B: the index changes, but this pass must continue using A throughout.
        test_git(&root, &["add", "-f", "--", "ignored/forced.rs"]);
        before_walk_release.wait();
        after_publish_entered.wait();
        assert!(
            state
                .gitignore
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_ignored(Path::new("ignored"), true),
            "the published matcher must use the same A snapshot as the walk"
        );
        if PER_DIRECTORY_WATCHES {
            assert!(
                !state
                    .watch_registry
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .watched
                    .contains(&ignored),
                "the B transition must not change subscriptions mid-pass"
            );
        }
        // A again: the pass already represents the final membership, so no
        // corrective pass is necessary.
        test_git(
            &root,
            &[
                "rm",
                "--cached",
                "--quiet",
                "--force",
                "--",
                "ignored/forced.rs",
            ],
        );
        after_publish_release.wait();
        assert!(
            refresh.join().unwrap(),
            "the raced pass itself should finish"
        );

        thread::sleep(Duration::from_millis(100));
        assert!(
            passes.load(Ordering::SeqCst) == 1,
            "A→B→A should not need a retry when the first pass used A throughout"
        );
        assert!(
            !state.ignore_refresh_scheduled.load(Ordering::SeqCst)
                && !state.ignore_rules_dirty.load(Ordering::SeqCst),
            "the semantic baseline returned to A"
        );

        let matcher = state.gitignore.read().unwrap();
        assert!(
            matcher
                .as_ref()
                .unwrap()
                .is_ignored(Path::new("ignored"), true),
            "the final matcher must restore the final untracked state"
        );
        drop(matcher);
        let index = state.index.read().unwrap();
        assert!(
            !index.live.has_path("ignored/forced.rs")
                && (!index.reader_has_path("ignored/forced.rs")
                    || index.live.is_deleted("ignored/forced.rs")),
            "the immutable A walk must not index content from intermediate B"
        );
        drop(index);
        if PER_DIRECTORY_WATCHES {
            assert!(
                !state
                    .watch_registry
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .watched
                    .contains(&ignored),
                "the immutable A matcher must leave the ignored tree unsubscribed"
            );
        }
    }

    #[test]
    fn content_only_index_churn_during_refresh_does_not_chain_reconciles() {
        use std::sync::atomic::AtomicUsize;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["config", "core.ignorecase", "true"]);
        std::fs::write(root.join(".gitignore"), "IGNORED/\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn original() {}\n").unwrap();
        test_git(&root, &["add", "--", ".gitignore", "src/lib.rs"]);

        let state = test_server_state(&root, &root.join(".tgrep"));
        state.gitignore_pending.store(false, Ordering::SeqCst);
        let passes = Arc::new(AtomicUsize::new(0));
        let hook: StaleRefreshHook = {
            let root = root.clone();
            let passes = Arc::clone(&passes);
            Arc::new(move |phase| match phase {
                StaleRefreshPhase::BeforeWalk => {
                    passes.fetch_add(1, Ordering::SeqCst);
                }
                StaleRefreshPhase::AfterMatcherPublish => {
                    let pass = passes.load(Ordering::SeqCst);
                    if pass <= 4 {
                        std::fs::write(
                            root.join("src/lib.rs"),
                            format!("fn content_only_{pass}() {{}}\n"),
                        )
                        .unwrap();
                        test_git(&root, &["add", "--", "src/lib.rs"]);
                    }
                }
                StaleRefreshPhase::AfterBuildBeforeStampPublish => {}
                StaleRefreshPhase::AfterConcreteRead => {}
                StaleRefreshPhase::BeforeConcreteCommit => {}
            })
        };
        *state.stale_refresh_hook.lock().unwrap() = Some(hook);

        assert!(background_refresh_stale(
            &state,
            &root,
            &state.index_dir,
            true
        ));
        thread::sleep(Duration::from_millis(250));

        assert_eq!(
            passes.load(Ordering::SeqCst),
            1,
            "metadata churn with unchanged relevant membership must not chain full scans"
        );
        assert!(
            !state.ignore_refresh_scheduled.load(Ordering::SeqCst)
                && !state.ignore_rules_dirty.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn membership_change_during_refresh_schedules_one_corrective_pass() {
        use std::sync::Barrier;
        use std::sync::atomic::AtomicUsize;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["config", "core.ignorecase", "true"]);
        std::fs::write(root.join(".gitignore"), "IGNORED/\n").unwrap();
        let ignored = root.join("ignored");
        std::fs::create_dir_all(&ignored).unwrap();
        std::fs::write(ignored.join("forced.rs"), "fn newly_exempt() {}\n").unwrap();
        test_git(&root, &["add", "--", ".gitignore"]);

        let state = test_server_state(&root, &root.join(".tgrep"));
        state.gitignore_pending.store(false, Ordering::SeqCst);
        if PER_DIRECTORY_WATCHES {
            let watcher = notify::recommended_watcher(|_: notify::Result<Event>| {}).unwrap();
            *state.watch_registry.lock().unwrap() = Some(WatchRegistry {
                watcher,
                root: root.clone(),
                watched: std::iter::once(root.clone()).collect(),
            });
        }

        let before_walk_entered = Arc::new(Barrier::new(2));
        let before_walk_release = Arc::new(Barrier::new(2));
        let passes = Arc::new(AtomicUsize::new(0));
        let hook: StaleRefreshHook = {
            let before_walk_entered = Arc::clone(&before_walk_entered);
            let before_walk_release = Arc::clone(&before_walk_release);
            let passes = Arc::clone(&passes);
            Arc::new(move |phase| {
                if matches!(phase, StaleRefreshPhase::BeforeWalk)
                    && passes.fetch_add(1, Ordering::SeqCst) == 0
                {
                    before_walk_entered.wait();
                    before_walk_release.wait();
                }
            })
        };
        *state.stale_refresh_hook.lock().unwrap() = Some(hook);

        let refresh_state = Arc::clone(&state);
        let refresh_root = root.clone();
        let index_dir = state.index_dir.clone();
        let refresh = thread::spawn(move || {
            background_refresh_stale(&refresh_state, &refresh_root, &index_dir, true)
        });

        before_walk_entered.wait();
        test_git(&root, &["add", "-f", "--", "ignored/forced.rs"]);
        before_walk_release.wait();
        assert!(refresh.join().unwrap());

        let deadline = Instant::now() + Duration::from_secs(10);
        while (passes.load(Ordering::SeqCst) < 2
            || state.ignore_refresh_scheduled.load(Ordering::SeqCst)
            || state.ignore_rules_dirty.load(Ordering::SeqCst))
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(
            passes.load(Ordering::SeqCst),
            2,
            "a semantic A→B change must schedule exactly one corrective pass"
        );
        assert!(
            !state
                .gitignore
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_ignored(Path::new("ignored"), true)
        );
        let index = state.index.read().unwrap();
        assert!(
            index.reader_has_path("ignored/forced.rs") || index.live.has_path("ignored/forced.rs"),
            "the corrective pass must index the newly exempt file"
        );
        drop(index);
        if PER_DIRECTORY_WATCHES {
            assert!(
                state
                    .watch_registry
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .watched
                    .contains(&ignored),
                "the corrective pass must subscribe the newly exempt directory"
            );
        }
    }

    #[test]
    fn rpc_reload_uses_one_snapshot_across_git_index_aba() {
        use std::sync::Barrier;
        use std::sync::atomic::AtomicUsize;

        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["config", "core.ignorecase", "true"]);
        std::fs::write(root.join(".gitignore"), "IGNORED/\n").unwrap();
        let ignored = root.join("ignored");
        std::fs::create_dir_all(&ignored).unwrap();
        std::fs::write(ignored.join("forced.rs"), "fn reload_aba() {}\n").unwrap();
        test_git(&root, &["add", "--", ".gitignore"]);

        let mut state = test_server_state(&root, &root.join(".tgrep"));
        Arc::get_mut(&mut state).unwrap().watch_enabled = false;
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let before_walk_entered = Arc::new(Barrier::new(2));
        let before_walk_release = Arc::new(Barrier::new(2));
        let after_publish_entered = Arc::new(Barrier::new(2));
        let after_publish_release = Arc::new(Barrier::new(2));
        let passes = Arc::new(AtomicUsize::new(0));
        let hook: StaleRefreshHook = {
            let before_walk_entered = Arc::clone(&before_walk_entered);
            let before_walk_release = Arc::clone(&before_walk_release);
            let after_publish_entered = Arc::clone(&after_publish_entered);
            let after_publish_release = Arc::clone(&after_publish_release);
            let passes = Arc::clone(&passes);
            Arc::new(move |phase| match phase {
                StaleRefreshPhase::BeforeWalk => {
                    if passes.fetch_add(1, Ordering::SeqCst) == 0 {
                        before_walk_entered.wait();
                        before_walk_release.wait();
                    }
                }
                StaleRefreshPhase::AfterMatcherPublish => {
                    if passes.load(Ordering::SeqCst) == 1 {
                        after_publish_entered.wait();
                        after_publish_release.wait();
                    }
                }
                StaleRefreshPhase::AfterBuildBeforeStampPublish => {}
                StaleRefreshPhase::AfterConcreteRead => {}
                StaleRefreshPhase::BeforeConcreteCommit => {}
            })
        };
        *state.stale_refresh_hook.lock().unwrap() = Some(hook);

        let reload_state = Arc::clone(&state);
        let reload = thread::spawn(move || handle_reload(None, &reload_state));
        before_walk_entered.wait();
        test_git(&root, &["add", "-f", "--", "ignored/forced.rs"]);
        before_walk_release.wait();
        after_publish_entered.wait();
        test_git(
            &root,
            &[
                "rm",
                "--cached",
                "--quiet",
                "--force",
                "--",
                "ignored/forced.rs",
            ],
        );
        after_publish_release.wait();

        let response = reload.join().unwrap();
        assert!(response.contains("\"status\":\"reloaded\""), "{response}");
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            passes.load(Ordering::SeqCst),
            1,
            "A→B→A must not need a correction when reload used A throughout"
        );
        assert!(
            state
                .gitignore
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_ignored(Path::new("ignored"), true)
        );
        let index = state.index.read().unwrap();
        assert!(
            !index.reader_has_path("ignored/forced.rs")
                && !index.live.has_path("ignored/forced.rs"),
            "reload index and matcher must both represent final A"
        );
        assert!(
            !state.ignore_refresh_scheduled.load(Ordering::SeqCst)
                && !state.ignore_rules_dirty.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn rpc_reload_membership_change_schedules_one_corrective_pass() {
        use std::sync::Barrier;
        use std::sync::atomic::AtomicUsize;

        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["config", "core.ignorecase", "true"]);
        std::fs::write(root.join(".gitignore"), "IGNORED/\n").unwrap();
        std::fs::create_dir_all(root.join("ignored")).unwrap();
        std::fs::write(
            root.join("ignored/forced.rs"),
            "fn reload_membership_change() {}\n",
        )
        .unwrap();
        test_git(&root, &["add", "--", ".gitignore"]);

        let mut state = test_server_state(&root, &root.join(".tgrep"));
        Arc::get_mut(&mut state).unwrap().watch_enabled = false;
        state.gitignore_pending.store(false, Ordering::SeqCst);
        let before_walk_entered = Arc::new(Barrier::new(2));
        let before_walk_release = Arc::new(Barrier::new(2));
        let passes = Arc::new(AtomicUsize::new(0));
        let hook: StaleRefreshHook = {
            let before_walk_entered = Arc::clone(&before_walk_entered);
            let before_walk_release = Arc::clone(&before_walk_release);
            let passes = Arc::clone(&passes);
            Arc::new(move |phase| {
                if matches!(phase, StaleRefreshPhase::BeforeWalk)
                    && passes.fetch_add(1, Ordering::SeqCst) == 0
                {
                    before_walk_entered.wait();
                    before_walk_release.wait();
                }
            })
        };
        *state.stale_refresh_hook.lock().unwrap() = Some(hook);

        let reload_state = Arc::clone(&state);
        let reload = thread::spawn(move || handle_reload(None, &reload_state));
        before_walk_entered.wait();
        test_git(&root, &["add", "-f", "--", "ignored/forced.rs"]);
        before_walk_release.wait();
        let response = reload.join().unwrap();
        assert!(response.contains("\"status\":\"reloaded\""), "{response}");

        let deadline = Instant::now() + Duration::from_secs(10);
        while (passes.load(Ordering::SeqCst) < 2
            || state.ignore_refresh_scheduled.load(Ordering::SeqCst)
            || state.ignore_rules_dirty.load(Ordering::SeqCst))
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(
            passes.load(Ordering::SeqCst),
            2,
            "A→B during reload must schedule exactly one corrective pass"
        );
        assert!(
            !state
                .gitignore
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_ignored(Path::new("ignored"), true)
        );
        let index = state.index.read().unwrap();
        assert!(
            index.reader_has_path("ignored/forced.rs") || index.live.has_path("ignored/forced.rs"),
            "the correction must make reload's index agree with final B"
        );
    }

    #[test]
    fn rpc_reload_without_watcher_repairs_change_after_extraction() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        test_git(&root, &["init", "--quiet"]);
        let path = root.join("raced.rs");
        std::fs::write(&path, "fn old_reload_marker() {}\n").unwrap();

        let mut state = test_server_state(&root, &root.join(".tgrep"));
        Arc::get_mut(&mut state).unwrap().watch_enabled = false;
        state.gitignore_pending.store(false, Ordering::SeqCst);
        let hook_path = path.clone();
        *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::AfterBuildBeforeStampPublish) {
                std::fs::write(&hook_path, "fn new_reload_marker() {}\n").unwrap();
            }
        }));

        let response = handle_reload(None, &state);
        assert!(response.contains("\"status\":\"reloaded\""), "{response}");
        let result = handle_search(
            None,
            &serde_json::json!({"pattern": "new_reload_marker"}),
            &state,
        );
        assert!(
            result.contains("new_reload_marker"),
            "the no-watch catch-up must index the bytes written after extraction: {result}"
        );
    }

    #[test]
    fn unwatched_external_bootstrap_repairs_change_after_extraction() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        test_git(&root, &["init", "--quiet"]);
        let path = root.join("raced.rs");
        std::fs::write(&path, "fn old_bootstrap_marker() {}\n").unwrap();

        let mut state = test_server_state(&root, &root.join(".tgrep"));
        Arc::get_mut(&mut state).unwrap().watch_enabled = false;
        state.indexing.store(true, Ordering::SeqCst);
        state.gitignore_pending.store(false, Ordering::SeqCst);
        let hook_path = path.clone();
        *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::AfterBuildBeforeStampPublish) {
                std::fs::write(&hook_path, "fn new_bootstrap_marker() {}\n").unwrap();
            }
        }));

        assert!(bootstrap_index_build(&state, &root, &state.index_dir));
        let result = handle_search(
            None,
            &serde_json::json!({"pattern": "new_bootstrap_marker"}),
            &state,
        );
        assert!(
            result.contains("new_bootstrap_marker"),
            "the no-watch bootstrap catch-up must index the final bytes: {result}"
        );
    }

    #[test]
    fn unwatched_resumed_build_repairs_change_after_extraction() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        test_git(&root, &["init", "--quiet"]);
        std::fs::write(root.join("seeded.rs"), "fn seeded() {}\n").unwrap();
        let mut state = test_server_state(&root, &root.join(".tgrep"));
        Arc::get_mut(&mut state).unwrap().watch_enabled = false;
        builder::build_index_with_options(
            &root,
            Some(&state.index_dir),
            &builder::BuildOptions {
                no_require_git: false,
                ..Default::default()
            },
        )
        .unwrap();
        *state.index.write().unwrap() = HybridIndex::open(&state.index_dir, &root).unwrap();

        let path = root.join("raced.rs");
        std::fs::write(&path, "fn old_resumed_marker() {}\n").unwrap();
        state.indexing.store(true, Ordering::SeqCst);
        state.gitignore_pending.store(false, Ordering::SeqCst);
        let hook_path = path.clone();
        *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::AfterBuildBeforeStampPublish) {
                std::fs::write(&hook_path, "fn new_resumed_marker() {}\n").unwrap();
            }
        }));

        background_index_build(&state, &root, &state.index_dir);
        let result = handle_search(
            None,
            &serde_json::json!({"pattern": "new_resumed_marker"}),
            &state,
        );
        assert!(
            result.contains("new_resumed_marker"),
            "the no-watch resumed-build catch-up must index the final bytes: {result}"
        );
    }

    #[test]
    fn rpc_reload_replays_event_received_after_extraction() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        test_git(&root, &["init", "--quiet"]);
        let path = root.join("raced.rs");
        std::fs::write(&path, "fn old_watched_marker() {}\n").unwrap();
        let state = test_server_state(&root, &root.join(".tgrep"));
        state.gitignore_pending.store(false, Ordering::SeqCst);
        let prior = root.join("prior.rs");
        state
            .deferred_events
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .insert(prior.clone(), false);

        let hook_path = path.clone();
        let hook_state = Arc::clone(&state);
        *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::AfterBuildBeforeStampPublish) {
                assert!(
                    hook_state
                        .deferred_events
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .contains_key(&prior),
                    "reload must preserve events awaiting an earlier build replay"
                );
                std::fs::write(&hook_path, "fn new_watched_marker() {}\n").unwrap();
                assert!(defer_events_during_build(
                    &hook_state,
                    &Event {
                        kind: EventKind::Modify(notify::event::ModifyKind::Data(
                            notify::event::DataChange::Any,
                        )),
                        paths: vec![hook_path.clone()],
                        attrs: Default::default(),
                    }
                ));
            }
        }));

        let response = handle_reload(None, &state);
        assert!(response.contains("\"status\":\"reloaded\""), "{response}");
        let result = handle_search(
            None,
            &serde_json::json!({"pattern": "new_watched_marker"}),
            &state,
        );
        assert!(
            result.contains("new_watched_marker"),
            "the replay must force the concrete event despite the later matching stamp: {result}"
        );
    }

    #[test]
    fn rpc_reload_schedules_ignore_rule_change_received_during_build() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        test_git(&root, &["init", "--quiet"]);
        let rules = root.join(".gitignore");
        std::fs::write(&rules, "").unwrap();
        std::fs::create_dir(root.join("ignored")).unwrap();
        std::fs::write(root.join("ignored/file.rs"), "fn should_disappear() {}\n").unwrap();
        let state = test_server_state(&root, &root.join(".tgrep"));
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let hook_rules = rules.clone();
        let hook_root = root.clone();
        let hook_state = Arc::clone(&state);
        *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::AfterBuildBeforeStampPublish) {
                std::fs::write(&hook_rules, "ignored/\n").unwrap();
                handle_fs_event(
                    &hook_state,
                    &hook_root,
                    &Event {
                        kind: EventKind::Modify(notify::event::ModifyKind::Data(
                            notify::event::DataChange::Any,
                        )),
                        paths: vec![hook_rules.clone()],
                        attrs: Default::default(),
                    },
                );
            }
        }));

        let response = handle_reload(None, &state);
        assert!(response.contains("\"status\":\"reloaded\""), "{response}");
        let deadline = Instant::now() + Duration::from_secs(10);
        while (state.ignore_refresh_scheduled.load(Ordering::SeqCst)
            || state.ignore_rules_dirty.load(Ordering::SeqCst))
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            !state.ignore_refresh_scheduled.load(Ordering::SeqCst)
                && !state.ignore_rules_dirty.load(Ordering::SeqCst),
            "the rule change observed during reload must run a serialized refresh"
        );
        let index = state.index.read().unwrap();
        assert!(
            !index.reader_has_path("ignored/file.rs") || index.live.is_deleted("ignored/file.rs"),
            "the refresh must remove content hidden by the new rule"
        );
    }

    #[test]
    fn concrete_event_reindexes_equal_length_rewrite_with_matching_stamp() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let path = root.join("same.rs");
        std::fs::write(&path, "fn old_marker() {}\n").unwrap();
        let state = test_server_state(&root, &root.join(".tgrep"));
        state.gitignore_pending.store(false, Ordering::SeqCst);

        {
            let _gate = state.snapshot_gate.read().unwrap();
            reindex_file(&state, &path, "same.rs", false);
        }
        std::fs::write(&path, "fn new_marker() {}\n").unwrap();
        let current = tgrep_core::meta::collect_filestamps(&root, &["same.rs".to_string()])
            .remove("same.rs")
            .unwrap();
        state
            .file_stamps
            .write()
            .unwrap()
            .insert("same.rs".to_string(), current);

        // Mutation control: the speculative path still trusts the persisted
        // stamp and therefore leaves the old posting set in place.
        {
            let _gate = state.snapshot_gate.read().unwrap();
            reindex_file(&state, &path, "same.rs", false);
        }
        let stale = handle_search(None, &serde_json::json!({"pattern": "new_marker"}), &state);
        assert!(
            !stale.contains("\"content\":\"fn new_marker"),
            "the control must demonstrate that stamp-only filtering misses the rewrite"
        );

        handle_fs_event(
            &state,
            &root,
            &Event {
                kind: EventKind::Modify(notify::event::ModifyKind::Data(
                    notify::event::DataChange::Any,
                )),
                paths: vec![path],
                attrs: Default::default(),
            },
        );
        let repaired = handle_search(None, &serde_json::json!({"pattern": "new_marker"}), &state);
        assert!(
            repaired.contains("\"content\":\"fn new_marker"),
            "a concrete event must bypass the coarse matching stamp: {repaired}"
        );
    }

    #[test]
    fn concrete_event_rejects_change_after_verification_before_commit() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let path = root.join("raced.rs");
        std::fs::write(&path, "fn old_marker() {}\n").unwrap();
        let state = test_server_state(&root, &root.join(".tgrep"));
        state.gitignore_pending.store(false, Ordering::SeqCst);
        {
            let _gate = state.snapshot_gate.read().unwrap();
            reindex_file(&state, &path, "raced.rs", false);
        }
        let dirty_before = state.index.read().unwrap().live.dirty_count();
        std::fs::write(&path, "fn first_new_() {}\n").unwrap();

        state.indexing.store(true, Ordering::SeqCst);
        let changed = path.clone();
        *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::BeforeConcreteCommit) {
                std::fs::write(&changed, "fn final_new_() {}\n").unwrap();
            }
        }));
        {
            let _gate = state.snapshot_gate.read().unwrap();
            reindex_file(&state, &path, "raced.rs", true);
        }

        assert_eq!(
            state.index.read().unwrap().live.dirty_count(),
            dirty_before,
            "bytes invalidated after verification must not be committed"
        );
        assert_eq!(
            state.file_stamps.read().unwrap().get("raced.rs"),
            Some(&tgrep_core::meta::FileStamp {
                mtime: u64::MAX,
                size: u64::MAX,
            }),
            "a rejected final version must remain scheduled for correction"
        );

        *state.stale_refresh_hook.lock().unwrap() = None;
        state.indexing.store(false, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(10);
        while state.ignore_refresh_scheduled.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn rpc_reload_build_failure_preserves_active_index() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state
            .index
            .write()
            .unwrap()
            .live
            .upsert_file("active.rs", b"fn active_before_reload() {}\n");

        std::fs::write(index_dir.join(".reload-build"), b"blocks staging directory").unwrap();
        let response = handle_reload(None, &state);

        assert!(response.contains("rebuild failed"), "{response}");
        assert!(
            state.index.read().unwrap().live.has_path("active.rs"),
            "a failed staged build must not disturb the active index"
        );
    }

    #[test]
    fn rpc_reload_waits_for_initial_build() {
        use std::sync::mpsc::{RecvTimeoutError, channel};

        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let state = test_server_state(&root, &root.join(".tgrep"));
        state.indexing.store(true, Ordering::SeqCst);
        let (entered_tx, entered_rx) = channel();
        *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::BeforeWalk) {
                entered_tx.send(()).unwrap();
            }
        }));

        let reload_state = Arc::clone(&state);
        let reload = thread::spawn(move || handle_reload(None, &reload_state));
        assert!(matches!(
            entered_rx.recv_timeout(Duration::from_millis(150)),
            Err(RecvTimeoutError::Timeout)
        ));

        state.indexing.store(false, Ordering::SeqCst);
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let response = reload.join().unwrap();
        assert!(response.contains("\"status\":\"reloaded\""), "{response}");
    }

    #[test]
    fn index_mutation_keeps_write_lock_until_cache_invalidation() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let path = root.join("atomic.rs");
        std::fs::write(&path, "fn atomic_marker() {}\n").unwrap();
        let state = test_server_state(&root, &root.join(".tgrep"));
        state
            .cache
            .write()
            .unwrap()
            .put("atomic.rs".to_string(), cached(10));
        let generation = state.cache_generation.load(Ordering::SeqCst);

        // Prevent invalidation from completing after the index commit. The
        // writer must retain the index lock while it waits for this guard.
        let cache_guard = state.cache.read().unwrap();
        let before_commit = Arc::new(std::sync::Barrier::new(2));
        let continue_commit = Arc::new(std::sync::Barrier::new(2));
        let hook_before = Arc::clone(&before_commit);
        let hook_continue = Arc::clone(&continue_commit);
        *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::BeforeConcreteCommit) {
                hook_before.wait();
                hook_continue.wait();
            }
        }));

        let worker_state = Arc::clone(&state);
        let worker_path = path.clone();
        let worker = thread::spawn(move || {
            let _gate = worker_state.snapshot_gate.read().unwrap();
            reindex_file(&worker_state, &worker_path, "atomic.rs", true);
        });
        before_commit.wait();
        continue_commit.wait();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match state.index.try_read() {
                Err(std::sync::TryLockError::WouldBlock) => break,
                Err(std::sync::TryLockError::Poisoned(error)) => panic!("{error}"),
                Ok(index) => {
                    assert!(
                        !index.live.has_path("atomic.rs"),
                        "new postings became visible while stale cached bytes were still readable"
                    );
                }
            }
            assert!(
                Instant::now() < deadline,
                "index writer never reached commit"
            );
            thread::yield_now();
        }
        thread::sleep(Duration::from_millis(50));
        assert!(
            matches!(
                state.index.try_read(),
                Err(std::sync::TryLockError::WouldBlock)
            ),
            "index lock was released before cache invalidation completed"
        );

        drop(cache_guard);
        worker.join().unwrap();
        *state.stale_refresh_hook.lock().unwrap() = None;
        assert!(state.cache.read().unwrap().peek("atomic.rs").is_none());
        assert!(state.cache_generation.load(Ordering::SeqCst) > generation);
    }

    #[test]
    fn pre_reload_disk_read_cannot_repopulate_content_cache() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let state = test_server_state(&root, &root.join(".tgrep"));
        let before_reload = state.cache_generation.load(Ordering::SeqCst);
        let stale = vec![(
            "stale.rs".to_string(),
            Arc::new(DecodedFile::new(
                b"old content".to_vec(),
                tgrep_core::encoding::EncodingMode::Auto,
            )),
        )];

        let _index = state.index.read().unwrap();
        invalidate_cached_paths_locked(&state, std::iter::once("stale.rs"));
        update_content_cache(&state, before_reload, &[], &stale);
        assert!(
            state.cache.read().unwrap().peek("stale.rs").is_none(),
            "a disk read from the previous index generation must not refill the cache"
        );

        let current = state.cache_generation.load(Ordering::SeqCst);
        update_content_cache(&state, current, &[], &stale);
        assert!(state.cache.read().unwrap().peek("stale.rs").is_some());
    }

    #[test]
    fn tracked_membership_poll_waits_for_reconcile_publication() {
        use std::sync::mpsc::{RecvTimeoutError, channel};

        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        test_git(&root, &["init", "--quiet"]);
        test_git(&root, &["config", "core.ignorecase", "true"]);
        std::fs::write(root.join(".gitignore"), "IGNORED/\n").unwrap();
        std::fs::create_dir_all(root.join("ignored")).unwrap();
        std::fs::write(root.join("ignored/forced.rs"), "fn transient() {}\n").unwrap();
        test_git(&root, &["add", "--", ".gitignore"]);

        let state = test_server_state(&root, &root.join(".tgrep"));
        state.gitignore_pending.store(false, Ordering::SeqCst);
        assert!(background_refresh_stale(
            &state,
            &root,
            &state.index_dir,
            true
        ));

        let gate = state.snapshot_gate.write().unwrap();
        test_git(&root, &["add", "-f", "--", "ignored/forced.rs"]);
        let (result_tx, result_rx) = channel();
        let poll_state = Arc::clone(&state);
        let poll = thread::spawn(move || {
            result_tx
                .send(poll_tracked_membership_changed(&poll_state))
                .unwrap();
        });
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_millis(150)),
            Err(RecvTimeoutError::Timeout)
        ));

        test_git(
            &root,
            &[
                "rm",
                "--cached",
                "--quiet",
                "--force",
                "--",
                "ignored/forced.rs",
            ],
        );
        drop(gate);
        assert!(!result_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        poll.join().unwrap();
    }

    /// A transient stat failure is not a deletion. `Path::exists` said it was,
    /// which meant one `EACCES` — or a Windows sharing violation from a build
    /// holding the file open — tombstoned content that was still valid, and
    /// bypassed the preservation policy `reindex_file` applies to exactly those
    /// errors.
    #[test]
    fn an_unreadable_path_is_not_treated_as_a_deletion() {
        use std::io::{Error, ErrorKind};

        let unreadable: std::io::Result<std::fs::Metadata> =
            Err(Error::from(ErrorKind::PermissionDenied));
        assert_eq!(
            classify_event_target(&unreadable),
            EventTarget::Unknown,
            "a locked or unreadable file must leave the index alone"
        );

        // Windows reports a file opened without FILE_SHARE_* this way, and it
        // is the single most common way a stat fails on a live repository.
        let sharing: std::io::Result<std::fs::Metadata> = Err(Error::from_raw_os_error(32));
        assert_eq!(classify_event_target(&sharing), EventTarget::Unknown);

        // The other direction still has to work, or a real deletion is never
        // applied.
        let absent: std::io::Result<std::fs::Metadata> = Err(Error::from(ErrorKind::NotFound));
        assert_eq!(classify_event_target(&absent), EventTarget::Gone);

        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("real.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        assert_eq!(
            classify_event_target(&std::fs::metadata(&file)),
            EventTarget::Regular
        );
        assert_eq!(
            classify_event_target(&std::fs::metadata(tmp.path())),
            EventTarget::NotRegular
        );
    }

    /// Create a symlink, or return `false` where the platform will not allow
    /// one — an unprivileged Windows runner without Developer Mode.
    fn try_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (target, link);
            false
        }
    }

    /// The matcher reads a symlinked source through the link, so an edit to the
    /// target changes the rules in force. `handle_fs_event` recognises an event
    /// naming that target — but on a per-directory backend no event ever
    /// arrived, because the directory holding it is one the rules hide and
    /// nothing subscribed to it.
    #[test]
    fn a_rule_file_symlinked_into_a_hidden_directory_is_still_watched() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("build")).unwrap();
        std::fs::write(root.join("build").join("shared-rules"), "target/\n").unwrap();
        if !try_symlink(
            &root.join("build").join("shared-rules"),
            &root.join(".gitignore"),
        ) {
            return;
        }

        let sources = vec![root.join(".gitignore")];
        let dirs = ignore_target_dirs(root, &sources);

        assert!(
            dirs.contains(&root.join("build")),
            "the directory holding a rule file the matcher read must be watched \
             even when the rules hide it: {dirs:?}"
        );
    }

    /// A source that is a plain file needs nothing extra, and one whose target
    /// is outside the root must not pull a subscription outside the tree the
    /// server was asked to serve.
    #[test]
    fn ordinary_and_outside_rule_files_add_no_subscriptions() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        assert!(ignore_target_dirs(root, &[root.join(".gitignore")]).is_empty());

        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("rules"), "target/\n").unwrap();
        if !try_symlink(&outside.path().join("rules"), &root.join(".ignore")) {
            return;
        }
        assert!(
            ignore_target_dirs(root, &[root.join(".ignore")]).is_empty(),
            "a target outside the root must not be subscribed to"
        );
    }

    #[test]
    fn skip_watcher_dir_applies_directory_semantics() {
        // A directory-only gitignore rule (`build/`) does not match the path
        // `build` when it is tested as a file, which is why the subscription
        // set needs its own dir-aware entry point.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "build/\n").unwrap();
        let gi = tgrep_core::gitignore::build_matcher(tmp.path()).expect("matcher should build");

        assert!(should_skip_watcher_dir("build", &[], Some(&gi)));
        assert!(!should_skip_watcher_path("build", &[], Some(&gi)));

        // `--exclude` prunes the named directory itself...
        let exclude = vec!["target".to_string()];
        assert!(should_skip_watcher_dir("target", &exclude, None));
        assert!(should_skip_watcher_dir("src/target", &exclude, None));
        // ...but a *file* of that name is still indexed, so it must not be
        // skipped. This is the invariant `should_skip_watcher_path` already
        // held, and sharing an implementation must not have changed it.
        assert!(!should_skip_watcher_path("target", &exclude, None));
        assert!(!should_skip_watcher_path("src/target", &exclude, None));
    }

    #[test]
    fn identifies_live_ignore_rule_changes() {
        let root = Path::new("workspace");
        assert!(is_ignore_rules_file(
            root,
            Path::new("workspace/nested/.gitignore")
        ));
        assert!(is_ignore_rules_file(
            root,
            Path::new("workspace/p4ignore.ini")
        ));
        // `.ignore` is a first-class ignore source (and, unlike `.gitignore`,
        // applies outside a git repo), so live edits to it must refresh the
        // matcher too — at the root and nested.
        assert!(is_ignore_rules_file(root, Path::new("workspace/.ignore")));
        assert!(is_ignore_rules_file(
            root,
            Path::new("workspace/nested/.ignore")
        ));
        assert!(!is_ignore_rules_file(
            root,
            Path::new("workspace/nested/p4ignore.ini")
        ));
        assert!(!is_ignore_rules_file(
            root,
            Path::new("workspace/.git/info/exclude")
        ));
        assert!(!is_ignore_rules_file(
            root,
            Path::new("workspace/src/main.rs")
        ));
    }

    #[test]
    fn ignore_reconcile_compares_against_actual_indexed_paths() {
        use std::collections::{HashMap, HashSet};
        use tgrep_core::meta::FileStamp;
        use tgrep_core::walker::FileMeta;

        let current = vec![
            FileMeta {
                relative_path: "kept.txt".to_string(),
                mtime: 1,
                size: 10,
            },
            FileMeta {
                relative_path: "newly-unignored.txt".to_string(),
                mtime: 2,
                size: 20,
            },
        ];
        let stamps = HashMap::from([
            ("kept.txt".to_string(), FileStamp { mtime: 1, size: 10 }),
            (
                "newly-unignored.txt".to_string(),
                FileStamp { mtime: 2, size: 20 },
            ),
        ]);
        let indexed = HashSet::from(["kept.txt".to_string(), "newly-ignored.txt".to_string()]);

        let (changed, added, deleted) = classify_file_changes(&current, &stamps, &indexed, true);
        assert!(changed.is_empty());
        assert_eq!(added, vec!["newly-unignored.txt"]);
        assert_eq!(deleted, vec!["newly-ignored.txt"]);
    }

    #[test]
    fn stale_classification_uses_reader_paths_for_deletions_and_case_renames() {
        use std::collections::{HashMap, HashSet};
        use tgrep_core::walker::FileMeta;

        let current = vec![FileMeta {
            relative_path: "case.txt".to_string(),
            mtime: 1,
            size: 10,
        }];
        let indexed = HashSet::from(["Case.txt".to_string(), "reader-only.txt".to_string()]);

        let (changed, added, mut deleted) =
            classify_file_changes(&current, &HashMap::new(), &indexed, false);
        assert!(changed.is_empty());
        assert_eq!(added, vec!["case.txt"]);
        deleted.sort();
        assert_eq!(deleted, vec!["Case.txt", "reader-only.txt"]);
    }

    /// The reconcile waits for the server to go quiet, but not forever.
    #[test]
    fn a_scheduled_reconcile_defers_to_a_busy_server_but_not_indefinitely() {
        let long_quiet = RECONCILE_QUIET_PERIOD + Duration::from_secs(1);
        let just_queried = Duration::from_secs(1);

        // Before the interval, nothing runs however idle the server is.
        assert!(!reconcile_due(
            RECONCILE_INTERVAL - Duration::from_secs(1),
            long_quiet,
            false
        ));
        // After it, an idle server reconciles.
        assert!(reconcile_due(RECONCILE_INTERVAL, long_quiet, false));
        // A server mid-query waits for a gap...
        assert!(!reconcile_due(RECONCILE_INTERVAL, just_queried, false));
        // ...but a server that is *always* mid-query would otherwise never
        // reconcile at all, which is the failure this exists to prevent.
        assert!(reconcile_due(RECONCILE_DEADLINE, just_queried, false));
        // Indexing and flushing outrank even the deadline: they are rewriting
        // the index already, and the next tick is a minute away.
        assert!(!reconcile_due(RECONCILE_DEADLINE, long_quiet, true));
    }

    /// A file that cannot be read must not make every reconcile rebuild.
    ///
    /// Its stamp is deliberately withheld so it looks new and gets retried.
    /// Left at that, a file locked by another process would be "new" on every
    /// pass, and a reconcile on a timer would rewrite the whole index once an
    /// hour, forever, to re-attempt a read that fails the same way each time.
    #[test]
    fn a_file_that_stays_unreadable_is_not_retried_until_it_changes() {
        use tgrep_core::meta::FileStamp;
        use tgrep_core::walker::FileMeta;

        let memo = std::collections::HashMap::from([(
            "locked.bin".to_string(),
            FileStamp {
                mtime: 100,
                size: 5,
            },
        )]);

        let retried = |mtime: u64, size: u64| {
            let current = vec![FileMeta {
                relative_path: "locked.bin".to_string(),
                mtime,
                size,
            }];
            let (mut changed, mut added, _) = classify_file_changes(
                &current,
                &std::collections::HashMap::new(),
                &std::collections::HashSet::new(),
                false,
            );
            let skipped = drop_memoized_failures(&memo, &current, &mut changed, &mut added);
            (changed.len() + added.len(), skipped)
        };

        // Unchanged since the failed read: leave it alone.
        assert_eq!(retried(100, 5).0, 0);
        // Touched: worth another look.
        assert_eq!(retried(200, 5).0, 1);
        // Resized: likewise.
        assert_eq!(retried(100, 6).0, 1);
    }

    /// Skipping a file must not also mark it indexed.
    ///
    /// The two halves have to agree. `drop_memoized_failures` keeps a file out
    /// of the delta, so nothing reads it and nothing writes postings for it; if
    /// the stamps published alongside that delta still claimed it was indexed at
    /// its current mtime and size, the next reconcile would classify it as
    /// unchanged and skip it for a completely different reason — one that never
    /// clears, because the memo is not involved and the stamp outlives the
    /// process in `filestamps.json`. A file that failed to read once would be
    /// invisible to search forever, with no error and no way back short of
    /// deleting the index.
    #[test]
    fn a_skipped_file_is_left_unstamped_so_the_next_pass_still_sees_it() {
        use tgrep_core::meta::FileStamp;
        use tgrep_core::walker::FileMeta;

        let current = vec![
            FileMeta {
                relative_path: "locked.bin".to_string(),
                mtime: 100,
                size: 5,
            },
            FileMeta {
                relative_path: "src/main.rs".to_string(),
                mtime: 100,
                size: 9,
            },
        ];
        let memo = std::collections::HashMap::from([(
            "locked.bin".to_string(),
            FileStamp {
                mtime: 100,
                size: 5,
            },
        )]);

        let (mut changed, mut added, _) = classify_file_changes(
            &current,
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
            false,
        );
        let skipped = drop_memoized_failures(&memo, &current, &mut changed, &mut added);
        assert!(skipped.contains("locked.bin"));

        let stamps = stamps_for_indexed(&current, &skipped);
        assert!(
            !stamps.contains_key("locked.bin"),
            "published a stamp for a file that was never read: {stamps:?}"
        );
        assert!(stamps.contains_key("src/main.rs"), "{stamps:?}");

        // And with that stamp withheld, a later pass over an unchanged tree
        // still offers the file up rather than treating it as up-to-date.
        let (changed, added, _) =
            classify_file_changes(&current, &stamps, &std::collections::HashSet::new(), false);
        assert_eq!(changed.len() + added.len(), 1);
        assert!(added.contains(&"locked.bin".to_string()));
    }

    /// A walk error must not leave the watcher gated forever.
    ///
    /// `gitignore_pending` is what keeps the watcher off the index until a
    /// matcher exists, and the stale check owns clearing it. It also refuses to
    /// touch the index when the walk could not inspect every entry, because
    /// unseen files would be misclassified as deleted. Those two are separate
    /// decisions: taking the second one used to skip the first, so one
    /// unreadable directory — or one file whose `metadata()` lost a race with a
    /// delete — silently disabled the watcher for the life of the process, and
    /// the overflow-repair path would not reconcile either, since it defers to
    /// the pending matcher.
    ///
    /// Unix-only because it needs a directory the process genuinely cannot
    /// read, which has no portable equivalent on Windows.
    #[cfg(unix)]
    #[test]
    fn a_walk_error_still_publishes_the_watcher_matcher() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        // `.gitignore` is git-gated, matching the indexing walk.
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(root.join("src.rs"), "fn main() {}\n").unwrap();

        let unreadable = root.join("locked");
        std::fs::create_dir(&unreadable).unwrap();
        std::fs::write(unreadable.join("inner.rs"), "fn inner() {}\n").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&unreadable).is_ok() {
            // Running as root, so permissions prove nothing. Restore and skip.
            std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let index_dir = root.join(".tgrep");
        std::fs::create_dir(&index_dir).unwrap();
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(true, Ordering::SeqCst);

        let ok = background_refresh_stale(&state, &root, &index_dir, false);

        // Restore before any assertion so a failure still leaves a removable dir.
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            !ok,
            "a walk that could not inspect every entry must keep the old index"
        );
        assert!(
            !state.gitignore_pending.load(Ordering::SeqCst),
            "the watcher gate must be released even when the index is left alone"
        );
        assert!(
            state.gitignore.read().unwrap().is_some(),
            "the matcher the walk did find must still be published"
        );
    }

    /// The metadata the eligibility check uses and the bytes that get indexed
    /// have to describe the same object, which means one handle.
    #[test]
    fn open_within_root_reads_a_regular_file() {
        use std::io::Read;

        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        let path = tmp.path().join("src").join("plain.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();

        let file = open_within_root(tmp.path(), &path).expect("a regular file opens");
        let meta = file.metadata().expect("metadata off the handle");
        assert!(meta.is_file());
        assert_eq!(meta.len(), 13);

        let mut data = String::new();
        (&file).read_to_string(&mut data).unwrap();
        assert_eq!(data, "fn main() {}\n");
    }

    /// A symlink must not open as its target, or a link committed to a branch
    /// would pull a file from outside the served root into the index.
    #[cfg(unix)]
    #[test]
    fn open_within_root_refuses_a_symlinked_file() {
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "sensitive\n").unwrap();

        let root = TempDir::new().unwrap();
        let link = root.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(
            open_within_root(root.path(), &link).is_err(),
            "the link must not open as its target"
        );
    }

    /// And neither must a symlink anywhere *above* the file: `root/a/file`
    /// reads the same whether `a` is a directory or a link to one, so guarding
    /// only the last component still lets a whole tree in from outside.
    #[cfg(unix)]
    #[test]
    fn open_within_root_refuses_a_symlinked_ancestor() {
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "sensitive\n").unwrap();

        let root = TempDir::new().unwrap();
        let link = root.path().join("a");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let through_link = link.join("secret.txt");

        // The file at the end of that path is a perfectly ordinary file, and
        // opening it by name works — which is the point.
        assert!(std::fs::File::open(&through_link).is_ok());
        assert!(
            open_within_root(root.path(), &through_link).is_err(),
            "an intermediate symlink must not be traversed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn concrete_event_rejects_file_swapped_to_outside_symlink_between_reads() {
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("outside.rs");
        std::fs::write(&target, "fn outside_file_marker() {}\n").unwrap();
        let root_dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(root_dir.path()).unwrap();
        let path = root.join("raced.rs");
        std::fs::write(&path, "fn inside_file_marker() {}\n").unwrap();
        let state = test_server_state(&root, &root.join(".tgrep"));
        state.gitignore_pending.store(false, Ordering::SeqCst);
        {
            let _gate = state.snapshot_gate.read().unwrap();
            reindex_file(&state, &path, "raced.rs", false);
        }

        let swap_path = path.clone();
        let swap_target = target.clone();
        *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::AfterConcreteRead) {
                std::fs::remove_file(&swap_path).unwrap();
                std::os::unix::fs::symlink(&swap_target, &swap_path).unwrap();
            }
        }));
        handle_fs_event(
            &state,
            &root,
            &Event {
                kind: EventKind::Modify(notify::event::ModifyKind::Data(
                    notify::event::DataChange::Any,
                )),
                paths: vec![path],
                attrs: Default::default(),
            },
        );

        let result = handle_search(
            None,
            &serde_json::json!({"pattern": "outside_file_marker"}),
            &state,
        );
        assert!(
            !result.contains("outside_file_marker"),
            "verification must not follow a replacement symlink outside the root"
        );
        assert!(state.index.read().unwrap().live.is_deleted("raced.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn concrete_event_rejects_ancestor_swapped_to_outside_symlink_between_reads() {
        let outside = TempDir::new().unwrap();
        std::fs::write(
            outside.path().join("raced.rs"),
            "fn outside_ancestor_marker() {}\n",
        )
        .unwrap();
        let root_dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(root_dir.path()).unwrap();
        let dir = root.join("dir");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("raced.rs");
        std::fs::write(&path, "fn inside_ancestor_marker() {}\n").unwrap();
        let state = test_server_state(&root, &root.join(".tgrep"));
        state.gitignore_pending.store(false, Ordering::SeqCst);
        {
            let _gate = state.snapshot_gate.read().unwrap();
            reindex_file(&state, &path, "dir/raced.rs", false);
        }

        let swap_dir = dir.clone();
        let moved_dir = root.join("original-dir");
        let outside_dir = outside.path().to_path_buf();
        *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::AfterConcreteRead) {
                std::fs::rename(&swap_dir, &moved_dir).unwrap();
                std::os::unix::fs::symlink(&outside_dir, &swap_dir).unwrap();
            }
        }));
        handle_fs_event(
            &state,
            &root,
            &Event {
                kind: EventKind::Modify(notify::event::ModifyKind::Data(
                    notify::event::DataChange::Any,
                )),
                paths: vec![path],
                attrs: Default::default(),
            },
        );

        let result = handle_search(
            None,
            &serde_json::json!({"pattern": "outside_ancestor_marker"}),
            &state,
        );
        assert!(
            !result.contains("outside_ancestor_marker"),
            "verification must not traverse a replacement ancestor symlink"
        );
        assert!(state.index.read().unwrap().live.is_deleted("dir/raced.rs"));
    }

    #[test]
    fn concrete_event_caps_growth_between_verification_reads() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let path = root.join("grows.rs");
        std::fs::write(&path, "fn small() {}\n").unwrap();
        let mut state = test_server_state(&root, &root.join(".tgrep"));
        Arc::get_mut(&mut state).unwrap().max_file_size = Some(64);
        state.gitignore_pending.store(false, Ordering::SeqCst);
        {
            let _gate = state.snapshot_gate.read().unwrap();
            reindex_file(&state, &path, "grows.rs", false);
        }

        let grow_path = path.clone();
        *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
            if matches!(phase, StaleRefreshPhase::AfterConcreteRead) {
                std::fs::write(&grow_path, vec![b'x'; 4096]).unwrap();
            }
        }));
        handle_fs_event(
            &state,
            &root,
            &Event {
                kind: EventKind::Modify(notify::event::ModifyKind::Data(
                    notify::event::DataChange::Any,
                )),
                paths: vec![path],
                attrs: Default::default(),
            },
        );

        assert!(
            state.index.read().unwrap().live.is_deleted("grows.rs"),
            "growth past the configured limit must be rejected before a second unbounded read"
        );
    }

    /// Nothing may be resolved that could climb back out of the root.
    #[test]
    fn open_within_root_refuses_paths_that_escape_or_are_not_literal() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src").join("a.rs"), "x\n").unwrap();

        assert!(
            open_within_root(tmp.path(), tmp.path()).is_err(),
            "the root"
        );
        assert!(
            open_within_root(tmp.path(), &tmp.path().join("..").join("a.rs")).is_err(),
            "a parent component"
        );
        assert!(
            open_within_root(&tmp.path().join("src"), &tmp.path().join("src")).is_err(),
            "outside the given root"
        );
    }

    /// A burst big enough to be worth replaying individually is not worth
    /// replaying individually. The buffer gives up as a whole, because a
    /// truncated set looks exactly like a complete one at replay time.
    #[cfg(unix)]
    #[test]
    fn deferring_more_changes_than_the_cap_gives_up_on_the_whole_set() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        // The function only defers while a build is running; it now reports
        // back so the caller can handle an event that arrived after one ended.
        state.indexing.store(true, Ordering::SeqCst);

        let small = Event {
            kind: EventKind::Create(notify::event::CreateKind::Any),
            paths: vec![root.join("a.rs")],
            attrs: Default::default(),
        };
        assert!(defer_events_during_build(&state, &small));
        assert_eq!(
            state
                .deferred_events
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .len(),
            1
        );

        let flood = Event {
            kind: EventKind::Create(notify::event::CreateKind::Any),
            paths: (0..100_000)
                .map(|i| root.join(format!("f{i}.rs")))
                .collect(),
            attrs: Default::default(),
        };
        assert!(defer_events_during_build(&state, &flood));
        assert!(
            state.deferred_events.lock().unwrap().is_none(),
            "an overflowing burst must mark the buffer unusable, not truncate it"
        );

        // And stays given up on, rather than resuming a partial record.
        assert!(defer_events_during_build(&state, &small));
        assert!(state.deferred_events.lock().unwrap().is_none());
    }

    /// Events seen while a build ran are not applied then, but they are the
    /// only record that those paths moved: the build's walk misses anything
    /// written to a directory it already passed.
    #[cfg(unix)]
    #[test]
    fn changes_deferred_during_a_build_are_applied_once_it_publishes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        // As it is once a build publishes: no rules to wait for, nothing
        // indexing.
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let path = root.join("late.rs");
        std::fs::write(&path, "fn written_during_the_build() {}\n").unwrap();

        state.indexing.store(true, Ordering::SeqCst);
        handle_fs_event(
            &state,
            &root,
            &Event {
                kind: EventKind::Create(notify::event::CreateKind::Any),
                paths: vec![path.clone()],
                attrs: Default::default(),
            },
        );
        assert!(
            !state.index.read().unwrap().live.has_path("late.rs"),
            "an event during a build must not touch the index"
        );

        state.indexing.store(false, Ordering::SeqCst);
        replay_deferred_events(&state, &root);

        assert!(
            state.index.read().unwrap().live.has_path("late.rs"),
            "the deferred change must be applied once the build is done"
        );
        assert!(
            state
                .deferred_events
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|p| p.is_empty()),
            "the buffer must be drained, and left usable for the next build"
        );
    }

    /// A metadata-only change to a directory is not a claim that anything new
    /// is under it. Replaying every deferred path as a creation would turn a
    /// recursive `chmod` during a build into one subtree walk per directory.
    #[cfg(unix)]
    #[test]
    fn a_deferred_metadata_change_does_not_replay_as_a_subtree_arrival() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let dir = root.join("vendor");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("deep.rs"), "fn deep() {}\n").unwrap();

        state.indexing.store(true, Ordering::SeqCst);
        handle_fs_event(
            &state,
            &root,
            &Event {
                kind: EventKind::Modify(notify::event::ModifyKind::Metadata(
                    notify::event::MetadataKind::Permissions,
                )),
                paths: vec![dir.clone()],
                attrs: Default::default(),
            },
        );
        assert_eq!(
            state
                .deferred_events
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .get(&dir),
            Some(&false),
            "a metadata modify must not be recorded as introducing a directory"
        );

        state.indexing.store(false, Ordering::SeqCst);
        replay_deferred_events(&state, &root);

        // The replay reconstructs a modify, which stops at the directory gate.
        // Had it reconstructed a create, `watch_new_subtree` would have walked
        // in and indexed the file below.
        assert!(
            !state.index.read().unwrap().live.has_path("vendor/deep.rs"),
            "a metadata modify must not trigger a subtree walk on replay"
        );
    }

    #[test]
    fn a_deferred_directory_rename_reconciles_indexed_descendants() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let dir = root.join("removed");
        let path = dir.join("deep.rs");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(&path, "fn removed_descendant() {}\n").unwrap();
        let state = test_server_state(&root, &root.join(".tgrep"));
        state.gitignore_pending.store(false, Ordering::SeqCst);
        {
            let _gate = state.snapshot_gate.read().unwrap();
            reindex_file(&state, &path, "removed/deep.rs", false);
        }

        state.indexing.store(true, Ordering::SeqCst);
        std::fs::rename(&dir, root.join("moved")).unwrap();
        handle_fs_event(
            &state,
            &root,
            &Event {
                kind: EventKind::Modify(notify::event::ModifyKind::Name(
                    notify::event::RenameMode::From,
                )),
                paths: vec![dir],
                attrs: Default::default(),
            },
        );
        state.indexing.store(false, Ordering::SeqCst);
        replay_deferred_events(&state, &root);

        let deadline = Instant::now() + Duration::from_secs(10);
        while (state.ignore_refresh_scheduled.load(Ordering::SeqCst)
            || state.ignore_rules_dirty.load(Ordering::SeqCst))
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        let index = state.index.read().unwrap();
        assert!(
            !index.reader_has_path("removed/deep.rs") && !index.live.has_path("removed/deep.rs"),
            "the coalesced reconcile must retire descendants named by no removal event"
        );
    }

    /// A file that cannot be opened right now is not a file that stopped
    /// belonging in the index. Evicting on a transient error would drop live
    /// content because something else held the file open for a moment.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_keeps_its_indexed_content() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let path = root.join("locked.rs");
        std::fs::write(&path, "fn readable() {}\n").unwrap();
        reindex_file(&state, &path, "locked.rs", false);
        assert!(state.index.read().unwrap().live.has_path("locked.rs"));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&path).is_ok() {
            // Running as root, where the mode is advisory. Nothing to test.
            return;
        }
        reindex_file(&state, &path, "locked.rs", false);

        assert!(
            !state.index.read().unwrap().live.is_deleted("locked.rs"),
            "an unreadable file must keep what was already indexed for it"
        );
    }

    /// Deleting an ignore file is invisible to a scan that looks for *arrivals*
    /// by mtime, and leaves rules in force whose source is gone — so the
    /// published sources are checked directly.
    #[cfg(unix)]
    #[test]
    fn a_recovery_scan_notices_an_ignore_file_that_was_deleted() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        // Published as a source, then removed behind the matcher's back.
        let rules = root.join(".gitignore");
        std::fs::write(&rules, "build/\n").unwrap();
        *state.ignore_sources.write().unwrap() = vec![rules.clone()];
        std::fs::remove_file(&rules).unwrap();

        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        // Pretend a refresh worker is already running, so the scan's request
        // for one is coalesced into it instead of spawning a real rewalk that
        // would race the assertions below.
        state.ignore_refresh_scheduled.store(true, Ordering::SeqCst);
        reindex_files_in(
            &state,
            &root,
            std::slice::from_ref(&root),
            SystemTime::UNIX_EPOCH,
        );

        assert!(
            state.ignore_rules_dirty.load(Ordering::SeqCst),
            "a vanished ignore source must schedule a refresh"
        );
        assert!(
            !state.index.read().unwrap().live.has_path("kept.rs"),
            "the scan must abandon rather than index under rules it knows are stale"
        );
    }

    /// The invariant the incomplete-listing fix relies on: a directory that is
    /// not in `swept` has proved nothing about the names under it, so the
    /// sweep must leave them alone.
    ///
    /// The wiring above it — withholding a directory whose `read_dir` iterator
    /// yielded an error — has no test of its own, because a per-entry
    /// `readdir` failure cannot be induced portably. This pins the half that
    /// makes withholding sufficient.
    #[cfg(unix)]
    #[test]
    fn sweep_removed_files_only_deletes_from_directories_it_enumerated() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);

        let path = root.join("d").join("a.rs");
        std::fs::create_dir(root.join("d")).unwrap();
        std::fs::write(&path, "fn a() {}\n").unwrap();
        reindex_file(&state, &path, "d/a.rs", false);
        assert!(state.index.read().unwrap().live.has_path("d/a.rs"));

        // What `reindex_files_in` produces when an entry in `d` failed to
        // enumerate: the file is missing from `present`, and `d` is therefore
        // withheld from `swept`.
        let swept = std::collections::HashSet::new();
        let present = std::collections::HashSet::new();
        let no_vanished = std::collections::HashSet::new();
        sweep_removed_files(&state, &swept, &present, &no_vanished);
        assert!(
            !state.index.read().unwrap().live.is_deleted("d/a.rs"),
            "an unenumerated directory must not tombstone the files under it"
        );

        // And with the listing complete, the same absence is a deletion — once
        // the file is actually gone, which the sweep now confirms itself.
        std::fs::remove_file(&path).unwrap();
        let swept = std::collections::HashSet::from(["d".to_string()]);
        sweep_removed_files(&state, &swept, &present, &no_vanished);
        assert!(
            state.index.read().unwrap().live.is_deleted("d/a.rs"),
            "a fully enumerated directory must sweep what it no longer contains"
        );
    }

    /// The listing that decides what to sweep is from earlier in the scan. A
    /// file recreated since then has already had its event consumed, so
    /// deleting it on that stale evidence loses it until the next reconcile.
    #[cfg(unix)]
    #[test]
    fn the_sweep_does_not_delete_a_file_that_came_back() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);

        let path = root.join("d").join("a.rs");
        std::fs::create_dir(root.join("d")).unwrap();
        std::fs::write(&path, "fn a() {}\n").unwrap();
        reindex_file(&state, &path, "d/a.rs", false);
        assert!(state.index.read().unwrap().live.has_path("d/a.rs"));

        // `d` enumerated cleanly and did not contain `a.rs` at the time — but
        // it is back on disk by the time the sweep runs.
        let swept = std::collections::HashSet::from(["d".to_string()]);
        let present = std::collections::HashSet::new();
        let no_vanished = std::collections::HashSet::new();
        sweep_removed_files(&state, &swept, &present, &no_vanished);

        assert!(
            !state.index.read().unwrap().live.is_deleted("d/a.rs"),
            "a file that exists again must not be swept on a stale listing"
        );
    }

    /// A vanished directory that comes back as a link to somewhere else has
    /// not brought its files back: nothing under a link is walked, watched or
    /// indexed. The recheck that decides "it is here again" has to answer that
    /// under the same containment contract indexing does, or the stale in-root
    /// entry is kept and no later event ever corrects it.
    #[cfg(unix)]
    #[test]
    fn a_path_that_returns_through_a_symlinked_ancestor_is_still_swept() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);

        let dir = root.join("d");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn ours() {}\n").unwrap();
        reindex_file(&state, &dir.join("a.rs"), "d/a.rs", false);
        assert!(state.index.read().unwrap().live.has_path("d/a.rs"));

        // The scan listed the root, did not find `d`, and is about to sweep
        // what was under it. In between, `d` comes back — as a link to a tree
        // that is not ours, holding a file by the same name.
        let outside = tmp.path().join("elsewhere");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("a.rs"), "fn theirs() {}\n").unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&outside, &dir).unwrap();
        assert!(
            root.join("d").join("a.rs").is_file(),
            "the fixture must look like a returning file, or it proves nothing"
        );

        let swept = std::collections::HashSet::from([String::new()]);
        let present = std::collections::HashSet::new();
        let vanished = std::collections::HashSet::from(["d".to_string()]);
        sweep_removed_files(&state, &swept, &present, &vanished);

        assert!(
            state.index.read().unwrap().live.is_deleted("d/a.rs"),
            "a file reachable only through a symlink has not come back"
        );
    }

    /// An indexed file replaced in place by something that is not a regular
    /// file is not a removal — the path still exists — but its contents are no
    /// longer there to be found.
    #[cfg(unix)]
    #[test]
    fn a_file_replaced_by_a_fifo_loses_its_indexed_content() {
        use std::os::unix::ffi::OsStrExt;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let path = root.join("x.rs");
        std::fs::write(&path, "fn was_a_real_file() {}\n").unwrap();
        reindex_file(&state, &path, "x.rs", false);
        assert!(state.index.read().unwrap().live.has_path("x.rs"));

        std::fs::remove_file(&path).unwrap();
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `name` is a NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o644) }, 0);

        handle_fs_event(
            &state,
            &root,
            &Event {
                kind: EventKind::Modify(notify::event::ModifyKind::Name(
                    notify::event::RenameMode::To,
                )),
                paths: vec![path.clone()],
                attrs: Default::default(),
            },
        );

        assert!(
            state.index.read().unwrap().live.is_deleted("x.rs"),
            "content indexed before the path became a fifo must not stay searchable"
        );
    }

    /// A removal must not land while another thread is midway through indexing
    /// the same path, or the reindex commits bytes it read earlier and
    /// resurrects a file that is gone — with a fresh stamp, so nothing
    /// afterwards disagrees and no further event is coming to correct it.
    #[cfg(unix)]
    #[test]
    fn a_removal_waits_for_an_in_flight_reindex() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let path = root.join("x.rs");
        std::fs::write(&path, "fn indexed() {}\n").unwrap();
        reindex_file(&state, &path, "x.rs", false);
        assert!(state.index.read().unwrap().live.has_path("x.rs"));

        std::fs::remove_file(&path).unwrap();

        // Stands in for a `reindex_file` that has read the old bytes and not
        // yet committed them: it holds exactly this lock across that window.
        let held = state.reindex_lock.lock().unwrap();

        let worker = {
            let state = Arc::clone(&state);
            let root = root.clone();
            let path = path.clone();
            std::thread::spawn(move || {
                handle_fs_event(
                    &state,
                    &root,
                    &Event {
                        kind: EventKind::Remove(notify::event::RemoveKind::File),
                        paths: vec![path],
                        attrs: Default::default(),
                    },
                );
            })
        };

        // The delete has to wait its turn. Without the lock it lands
        // immediately, which is the ordering that loses the file.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !state.index.read().unwrap().live.is_deleted("x.rs"),
            "a removal must not mutate the index while an indexer holds the lock"
        );

        drop(held);
        worker.join().unwrap();
        assert!(
            state.index.read().unwrap().live.is_deleted("x.rs"),
            "and it must still apply once the lock is free"
        );
    }

    /// `git checkout`, `tar -x` and `rsync -a` all restore mtimes from what
    /// they unpack, so a nested ignore file can arrive carrying a timestamp
    /// from months ago. A recency test cannot see that; absence from the
    /// published sources can.
    #[cfg(unix)]
    #[test]
    fn an_arriving_ignore_file_with_a_preserved_mtime_still_refreshes_the_matcher() {
        use std::os::unix::ffi::OsStrExt;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        // The matcher in force was built without it.
        *state.ignore_sources.write().unwrap() = Vec::new();

        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let rules = sub.join(".gitignore");
        std::fs::write(&rules, "kept.rs\n").unwrap();
        std::fs::write(sub.join("kept.rs"), "fn kept() {}\n").unwrap();

        // Backdated well outside any plausible scan window.
        let name = std::ffi::CString::new(rules.as_os_str().as_bytes()).unwrap();
        let stamp = libc::timeval {
            tv_sec: 1_000_000,
            tv_usec: 0,
        };
        let times = [stamp, stamp];
        // SAFETY: `name` is NUL-terminated and `times` is a two-element array,
        // both outliving the call.
        assert_eq!(unsafe { libc::utimes(name.as_ptr(), times.as_ptr()) }, 0);

        state.ignore_refresh_scheduled.store(true, Ordering::SeqCst);
        let since = SystemTime::now() - std::time::Duration::from_secs(60);
        reindex_files_in(&state, &root, std::slice::from_ref(&sub), since);

        assert!(
            state.ignore_rules_dirty.load(Ordering::SeqCst),
            "an ignore file the matcher never read must schedule a refresh, \
             however old its mtime is"
        );
        assert!(
            !state.index.read().unwrap().live.has_path("sub/kept.rs"),
            "the scan must abandon rather than index under rules it knows are stale"
        );
    }

    /// The walker collects rule files with `Path::is_file`, which follows
    /// links, so a symlinked `.gitignore` contributes rules like any other —
    /// but `DirEntry::file_type` does not follow links, and the scan used to
    /// drop those entries before ever asking what they were.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_ignore_file_is_still_seen_by_a_recovery_scan() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let target = root.join("shared-rules");
        std::fs::write(&target, "kept.rs\n").unwrap();
        std::os::unix::fs::symlink(&target, sub.join(".gitignore")).unwrap();
        std::fs::write(sub.join("kept.rs"), "fn kept() {}\n").unwrap();

        // Recent enough that the mtime window alone would catch a *regular*
        // file here: what this pins is that a symlink gets that far at all.
        state.ignore_refresh_scheduled.store(true, Ordering::SeqCst);
        let since = SystemTime::now() - std::time::Duration::from_secs(60);
        reindex_files_in(&state, &root, std::slice::from_ref(&sub), since);

        assert!(
            state.ignore_rules_dirty.load(Ordering::SeqCst),
            "a symlinked ignore file carries rules and must schedule a refresh"
        );
        assert!(
            !state.index.read().unwrap().live.has_path("sub/kept.rs"),
            "the scan must abandon rather than index under rules it knows are stale"
        );
    }

    /// `read_dir` promises no ordering, and the rules a scan must respect can
    /// live in a directory it has not reached yet — so every directory in the
    /// scan is asked before any file in it is indexed.
    #[cfg(unix)]
    #[test]
    fn a_scan_checks_every_directory_for_rules_before_indexing_any_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);
        *state.ignore_sources.write().unwrap() = Vec::new();

        // The rules are in `b`, the file is in `a`, and `a` is scanned first.
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(a.join("keep.rs"), "fn keep() {}\n").unwrap();
        std::fs::write(b.join(".gitignore"), "keep.rs\n").unwrap();

        state.ignore_refresh_scheduled.store(true, Ordering::SeqCst);
        let since = SystemTime::now() - std::time::Duration::from_secs(60);
        reindex_files_in(&state, &root, &[a, b], since);

        assert!(
            state.ignore_rules_dirty.load(Ordering::SeqCst),
            "the scan must notice rules that live later in its own list"
        );
        assert!(
            !state.index.read().unwrap().live.has_path("a/keep.rs"),
            "nothing may be indexed before every directory has been asked for rules"
        );
    }

    /// A path is not evidence about contents. An archive restore can put a
    /// different `.gitignore` at a path the matcher already read, carrying an
    /// mtime older than the scan window — known name, untouched by the clock,
    /// and yet not the file whose rules are being enforced.
    #[test]
    fn a_rule_file_swapped_for_an_older_one_is_not_taken_on_faith() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let rules = root.join(".gitignore");
        std::fs::write(&rules, "nothing-at-all.rs\n").unwrap();
        std::fs::write(root.join("keep.rs"), "fn keep() {}\n").unwrap();
        *state.ignore_source_stamps.write().unwrap() =
            ignore_stamps_of(&root, std::slice::from_ref(&rules));
        *state.ignore_sources.write().unwrap() = vec![rules.clone()];

        // Far enough ahead that the mtime window cannot fire for anything on
        // disk: what is left is the question of whether the file is the one
        // that was read.
        let since = SystemTime::now() + std::time::Duration::from_secs(3600);
        state.ignore_refresh_scheduled.store(true, Ordering::SeqCst);
        reindex_files_in(&state, &root, std::slice::from_ref(&root), since);
        assert!(
            !state.ignore_rules_dirty.load(Ordering::SeqCst),
            "the file the matcher read must not be reported as changed"
        );
        assert!(
            state.index.read().unwrap().live.has_path("keep.rs"),
            "and the scan must get on with its work"
        );

        // Same path, same age as far as the window is concerned, different
        // rules.
        std::fs::write(&rules, "keep.rs\n").unwrap();
        reindex_files_in(&state, &root, std::slice::from_ref(&root), since);
        assert!(
            state.ignore_rules_dirty.load(Ordering::SeqCst),
            "a source that is no longer the file that was read must schedule a refresh"
        );
    }

    /// A `.gitignore` symlinked to `shared-rules` contributes the target's
    /// contents, because the walker follows links. Editing the target is
    /// therefore a rules change — but it touches nothing named like one.
    #[cfg(unix)]
    #[test]
    fn an_edit_to_a_symlinked_rule_files_target_schedules_a_refresh() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let target = root.join("shared-rules");
        std::fs::write(&target, "keep.rs\n").unwrap();
        let link = root.join(".gitignore");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let stamps = ignore_stamps_of(&root, std::slice::from_ref(&link));
        assert!(
            stamps.contains_key("shared-rules"),
            "the file the rules were actually read from has to be recorded too"
        );
        *state.ignore_source_stamps.write().unwrap() = stamps;

        // Stop at the flag: what is being pinned is that the event is
        // recognised, not what the refresh then does.
        state.indexing.store(true, Ordering::SeqCst);
        handle_fs_event(
            &state,
            &root,
            &Event {
                kind: EventKind::Modify(notify::event::ModifyKind::Data(
                    notify::event::DataChange::Any,
                )),
                paths: vec![target],
                attrs: Default::default(),
            },
        );

        assert!(
            state.ignore_rules_dirty.load(Ordering::SeqCst),
            "an edit to the file a rules symlink resolves to is a rules change, \
             whatever the path is called"
        );
    }

    /// An mtime is not a wall-clock instant. Whole-second (HFS+, ext3) or
    /// two-second (FAT) granularity can date a write before the moment it
    /// actually followed, and a source edited between the walk and the
    /// publication carries a stamp that matches it — so the window is the only
    /// test left, and it has to allow for the rounding.
    #[test]
    fn a_rule_file_stamped_a_second_early_is_still_inside_the_window() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let rules = root.join(".gitignore");
        std::fs::write(&rules, "nothing-at-all.rs\n").unwrap();
        *state.ignore_source_stamps.write().unwrap() =
            ignore_stamps_of(&root, std::slice::from_ref(&rules));
        *state.ignore_sources.write().unwrap() = vec![rules.clone()];
        let stamped = std::fs::metadata(&rules).unwrap().modified().unwrap();
        state.ignore_refresh_scheduled.store(true, Ordering::SeqCst);

        // Far outside any rounding: this one really is history.
        let old = stamped + std::time::Duration::from_secs(3600);
        reindex_files_in(&state, &root, std::slice::from_ref(&root), old);
        assert!(
            !state.ignore_rules_dirty.load(Ordering::SeqCst),
            "a source last written an hour before the walk is not a change"
        );

        // Within the granularity of a coarse filesystem's clock: the file may
        // well have been written after the walk began and been rounded down.
        let rounded = stamped + std::time::Duration::from_secs(1);
        reindex_files_in(&state, &root, std::slice::from_ref(&root), rounded);
        assert!(
            state.ignore_rules_dirty.load(Ordering::SeqCst),
            "an mtime that sits just under the window has to be treated as recent"
        );
    }

    /// A stamp is not what makes a file searchable — the index is.
    /// `filestamps.json` is optional, and a partial or absent map used to mean
    /// the sweep had no candidates and deleted files kept answering searches.
    #[test]
    fn the_sweep_drops_a_deleted_file_that_never_had_a_stamp() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let path = root.join("seeded.rs");
        std::fs::write(&path, "fn seeded() {}\n").unwrap();
        let gate = state.snapshot_gate.read().unwrap();
        reindex_file(&state, &path, "seeded.rs", false);
        assert!(state.index.read().unwrap().live.has_path("seeded.rs"));

        // Indexed, and searchable, but with nothing in the stamp map to say so
        // — as after a seed whose stamps could not be read.
        state.file_stamps.write().unwrap().remove("seeded.rs");
        std::fs::remove_file(&path).unwrap();

        let swept: std::collections::HashSet<String> = [String::new()].into_iter().collect();
        let no_vanished = std::collections::HashSet::new();
        sweep_removed_files(
            &state,
            &swept,
            &std::collections::HashSet::new(),
            &no_vanished,
        );
        drop(gate);

        assert!(
            state.index.read().unwrap().live.is_deleted("seeded.rs"),
            "a file that is gone from disk must stop answering searches whether or \
             not it had a stamp"
        );
    }

    /// Metadata is not content either. `rsync -a` and `tar -x` preserve mtime,
    /// and two different sets of rules are easily the same length — so a
    /// size-and-mtime pair is identical across the replacement and only the
    /// bytes tell them apart.
    #[cfg(unix)]
    #[test]
    fn a_rule_file_swapped_for_one_of_the_same_size_and_age_is_still_caught() {
        use std::os::unix::ffi::OsStrExt;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let rules = root.join(".gitignore");
        let name = std::ffi::CString::new(rules.as_os_str().as_bytes()).unwrap();
        // Whole seconds, so it survives a round trip through `utimes`, which
        // takes microseconds while ext4 stores nanoseconds.
        let stamp = libc::timeval {
            tv_sec: 1_000_000,
            tv_usec: 0,
        };
        let times = [stamp, stamp];
        // SAFETY: `name` is NUL-terminated and `times` is a two-element array,
        // both outliving each call.
        let backdate = || assert_eq!(unsafe { libc::utimes(name.as_ptr(), times.as_ptr()) }, 0);

        std::fs::write(&rules, "aaa.rs\n").unwrap();
        backdate();
        let before = std::fs::metadata(&rules).unwrap();
        *state.ignore_source_stamps.write().unwrap() =
            ignore_stamps_of(&root, std::slice::from_ref(&rules));
        *state.ignore_sources.write().unwrap() = vec![rules.clone()];

        // Different rules, same seven bytes, and the restore puts the old
        // timestamp back.
        std::fs::write(&rules, "bbb.rs\n").unwrap();
        backdate();
        let after = std::fs::metadata(&rules).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());

        state.ignore_refresh_scheduled.store(true, Ordering::SeqCst);
        let since = SystemTime::now() + std::time::Duration::from_secs(3600);
        reindex_files_in(&state, &root, std::slice::from_ref(&root), since);
        assert!(
            state.ignore_rules_dirty.load(Ordering::SeqCst),
            "rules that were swapped for different ones of the same size and age \
             must still be noticed"
        );
    }

    /// A rule file that has become a directory, a FIFO or a socket is as gone
    /// as a deleted one: the walker would no longer collect it and a rebuild
    /// would no longer read it, so the rules it contributed are being enforced
    /// by nothing. `exists` says otherwise, and nothing downstream corrects it
    /// — the digest check skips candidates that are not files.
    #[test]
    fn a_rule_file_replaced_by_a_directory_counts_as_gone() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let rules = root.join(".gitignore");
        std::fs::write(&rules, "aaa.rs\n").unwrap();
        *state.ignore_source_stamps.write().unwrap() =
            ignore_stamps_of(&root, std::slice::from_ref(&rules));
        *state.ignore_sources.write().unwrap() = vec![rules.clone()];

        std::fs::remove_file(&rules).unwrap();
        std::fs::create_dir(&rules).unwrap();

        // Keeps the refresh this schedules from clearing the flag underneath
        // the assertion.
        state.ignore_refresh_scheduled.store(true, Ordering::SeqCst);
        let since = SystemTime::now() + Duration::from_secs(3600);
        reindex_files_in(&state, &root, std::slice::from_ref(&root), since);
        assert!(
            state.ignore_rules_dirty.load(Ordering::SeqCst),
            "a source that is no longer a file must be treated as gone"
        );
    }

    /// Containment is a property of the whole path, not of its last component.
    ///
    /// `root/link/inner` is a perfectly real directory while `link` is a
    /// symlink to somewhere else entirely. The walker never descends through
    /// the link, so nothing under it is part of the served tree — subscribing
    /// to it spends a watch descriptor per directory of a tree that is not
    /// ours, which on a large linked-in tree is the inotify exhaustion this
    /// registration exists to avoid.
    #[cfg(unix)]
    #[test]
    fn a_directory_below_a_symlinked_one_is_not_subscribed_to() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir_all(outside.join("inner")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let escaped = root.join("link").join("inner");
        assert!(
            is_real_dir(&escaped),
            "the fixture must be a real directory, or it proves nothing"
        );
        assert!(!is_contained_dir(&root, &escaped));

        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);
        let watcher = notify::recommended_watcher(|_: notify::Result<Event>| {}).unwrap();
        *state.watch_registry.lock().unwrap() = Some(WatchRegistry {
            watcher,
            root: root.clone(),
            watched: std::iter::once(root.clone()).collect(),
        });

        let _gate = state.snapshot_gate.read().unwrap();
        watch_new_subtree(&state, &root, &escaped);

        let registry = state.watch_registry.lock().unwrap();
        assert!(
            !registry.as_ref().unwrap().watched.contains(&escaped),
            "a directory reached through a symlink must not be subscribed to"
        );
    }

    /// A stamp is a claim about the index, not about the filesystem.
    ///
    /// The build stamps from a second traversal that runs after the one that
    /// fed the index, so a file created between them is on disk and in no
    /// index. Stamping it makes every later check agree it is up to date:
    /// `reindex_file` returns early on a matching stamp, and the periodic
    /// reconcile compares stamps alone. The file would never be searchable.
    #[test]
    fn stamps_are_published_only_for_what_the_build_indexed() {
        use tgrep_core::walker::FileMeta;

        let files = vec![
            FileMeta {
                relative_path: "indexed.rs".to_string(),
                mtime: 1,
                size: 10,
            },
            FileMeta {
                relative_path: "arrived_between_the_walks.rs".to_string(),
                mtime: 2,
                size: 20,
            },
        ];
        let indexed = std::iter::once("indexed.rs".to_string()).collect();

        let stamps = stamps_for_index_members(files, &indexed);
        assert!(stamps.contains_key("indexed.rs"));
        assert!(
            !stamps.contains_key("arrived_between_the_walks.rs"),
            "a file the build never indexed must not be stamped as if it had been"
        );
    }

    /// The matcher reads its sources itself, inside the build. A replace that
    /// lands while it is reading leaves it enforcing the old rules, and stamps
    /// taken afterwards describe the new file — so pathname, timestamp and
    /// digest all agree there is nothing to reread, and the stale rules stay in
    /// force until something unrelated rebuilds them.
    #[test]
    fn a_rule_file_rewritten_during_the_build_marks_the_matcher_stale() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        let rules = root.join(".gitignore");
        std::fs::write(&rules, "aaa.rs\n").unwrap();
        state.ignore_refresh_scheduled.store(true, Ordering::SeqCst);

        // A build nothing raced publishes without complaint. Asserted first, or
        // the test below passes for a matcher that is always stale.
        let quiet = publish_ignore_matcher(&state, &root, vec![rules.clone()], || None);
        assert!(quiet.is_empty());
        assert!(!state.ignore_rules_dirty.load(Ordering::SeqCst));

        let newly = publish_ignore_matcher(&state, &root, vec![rules.clone()], || {
            // The checkout that lands while the builder is reading.
            std::fs::write(&rules, "bbb.rs\n").unwrap();
            None
        });
        assert!(newly.is_empty());
        assert!(
            state.ignore_rules_dirty.load(Ordering::SeqCst),
            "a matcher built over a moving source must be marked stale"
        );
    }

    /// A directory that vanished whole leaves indexed files whose own parent
    /// was never enumerated. `swept`/`present` only speak for a file's
    /// immediate parent, so without the vanished-directory evidence every one
    /// of those files stays searchable — and no event names them either: a
    /// move away delivers nothing at all for what was inside.
    #[test]
    fn a_directory_that_went_away_whole_takes_its_files_with_it() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);

        let dir = root.join("d");
        let nested = dir.join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(nested.join("b.rs"), "fn b() {}\n").unwrap();
        reindex_file(&state, &dir.join("a.rs"), "d/a.rs", false);
        reindex_file(&state, &nested.join("b.rs"), "d/deep/b.rs", false);
        assert!(state.index.read().unwrap().live.has_path("d/a.rs"));

        // Still there, merely unlistable from the scan's point of view: the
        // directory it was named in enumerated it, so silence proves nothing.
        {
            let _gate = state.snapshot_gate.read().unwrap();
            let since = SystemTime::now() + Duration::from_secs(3600);
            reindex_files_in(&state, &root, std::slice::from_ref(&root), since);
        }
        assert!(
            !state.index.read().unwrap().live.is_deleted("d/a.rs"),
            "a directory that is still on disk must not sweep its files"
        );

        // Gone, and the scan is told to recheck it — which is what a recovery
        // scan over a newly watched directory does. Its parent lists cleanly
        // and does not contain it.
        std::fs::remove_dir_all(&dir).unwrap();
        {
            let _gate = state.snapshot_gate.read().unwrap();
            let since = SystemTime::now() + Duration::from_secs(3600);
            reindex_files_in(&state, &root, &[root.clone(), dir.clone()], since);
        }
        let index = state.index.read().unwrap();
        assert!(
            index.live.is_deleted("d/a.rs"),
            "a file directly inside a vanished directory must be swept"
        );
        assert!(
            index.live.is_deleted("d/deep/b.rs"),
            "and so must one further down, whose parent vanished with it"
        );
    }

    /// `desired` is a set, so the order it iterates in is not the order the
    /// tree is in. Subscribing a child before its parent makes the containment
    /// check walk every level from the root for each one, which is the cost
    /// the parent-first fast path exists to avoid on a tree with 40k
    /// directories in it.
    #[test]
    fn subscriptions_are_established_from_the_root_down() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let deep = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();

        let watcher = notify::recommended_watcher(|_: notify::Result<Event>| {}).unwrap();
        let mut registry = WatchRegistry {
            watcher,
            root: root.clone(),
            watched: std::collections::HashSet::new(),
        };

        let desired: std::collections::HashSet<PathBuf> = [
            deep.clone(),
            root.join("a").join("b"),
            root.join("a"),
            root.clone(),
        ]
        .into_iter()
        .collect();
        let (added, _removed) = registry.sync(&desired, TraversalCompleteness::Complete, false);

        assert_eq!(added.len(), 4);
        let depths: Vec<usize> = added.iter().map(|d| d.components().count()).collect();
        assert!(
            depths.windows(2).all(|w| w[0] <= w[1]),
            "a parent must be subscribed before anything under it, got {added:?}"
        );
        assert!(registry.watched.contains(&deep));
    }

    /// After an overflow the registry's records are not evidence. A directory
    /// removal that was dropped leaves the kernel's descriptor released and the
    /// entry here intact, and an ordinary sync skips anything it already
    /// believes it watches — so the entry never gets corrected and a path
    /// recreated there reports nothing for the life of the server.
    #[test]
    fn a_forced_sync_retires_a_subscription_the_kernel_already_dropped() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let gone = root.join("gone");
        std::fs::create_dir(&gone).unwrap();

        let watcher = notify::recommended_watcher(|_: notify::Result<Event>| {}).unwrap();
        let mut registry = WatchRegistry {
            watcher,
            root: root.clone(),
            watched: std::collections::HashSet::new(),
        };
        assert_eq!(registry.add_all(std::slice::from_ref(&gone)).len(), 1);

        // The removal event that would have called `forget` was one of the
        // ones the overflow ate.
        std::fs::remove_dir(&gone).unwrap();
        let desired: std::collections::HashSet<PathBuf> = std::iter::once(gone.clone()).collect();

        registry.sync(&desired, TraversalCompleteness::Complete, false);
        assert!(
            registry.is_watched(&gone),
            "the fixture must reproduce the poisoned entry, or it proves nothing"
        );

        registry.sync(&desired, TraversalCompleteness::Complete, true);
        assert!(
            !registry.is_watched(&gone),
            "a forced sync must re-issue the subscription and drop the entry \
             when it fails, rather than trusting a descriptor that is gone"
        );
    }

    /// A directory can arrive already full — a `mv` from outside the root, a
    /// checkout, an unpacked archive — and nothing reports what came with it.
    /// Linux needs the descent to subscribe; Windows and macOS get the whole
    /// move as a single event for the directory and no per-file events at all.
    /// The indexing half therefore has to run everywhere, not only where the
    /// subscribing half does.
    #[test]
    fn a_populated_directory_that_arrives_whole_is_indexed_on_every_platform() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);
        state.gitignore_pending.store(false, Ordering::SeqCst);

        let watcher = notify::recommended_watcher(|_: notify::Result<Event>| {}).unwrap();
        *state.watch_registry.lock().unwrap() = Some(WatchRegistry {
            watcher,
            root: root.clone(),
            watched: std::iter::once(root.clone()).collect(),
        });

        // Built somewhere else and moved in, so no event ever described its
        // contents.
        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(staging.join("deep")).unwrap();
        std::fs::write(staging.join("top.rs"), "fn top() {}\n").unwrap();
        std::fs::write(staging.join("deep").join("low.rs"), "fn low() {}\n").unwrap();
        let moved = root.join("moved");
        std::fs::rename(&staging, &moved).unwrap();

        handle_fs_event(
            &state,
            &root,
            &Event {
                kind: EventKind::Create(notify::event::CreateKind::Any),
                paths: vec![moved.clone()],
                attrs: Default::default(),
            },
        );

        let index = state.index.read().unwrap();
        assert!(
            index.live.has_path("moved/top.rs"),
            "a file that moved in with its directory must be indexed"
        );
        assert!(
            index.live.has_path("moved/deep/low.rs"),
            "and so must one further down"
        );
    }

    /// Parent-directory rule files and the repository's `info/exclude` are
    /// enforced by the published matcher but sit outside the walk that finds
    /// everything else, so nothing would notice one being deleted — and rules
    /// with no source left keep hiding a subtree from the index.
    #[test]
    fn ignore_sources_include_the_rules_that_live_outside_the_tree() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let root = repo.join("sub");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::create_dir_all(repo.join(".git").join("info")).unwrap();

        std::fs::write(repo.join(".git").join("info").join("exclude"), "*.tmp\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();

        let sources = ignore_sources_of(&root, &[root.join(".gitignore")], &[], false);
        assert!(
            sources.contains(&repo.join(".gitignore")),
            "a parent .gitignore the matcher enforces must be tracked: {sources:?}"
        );
        assert!(
            sources.contains(&repo.join(".git").join("info").join("exclude")),
            "so must the repository's own exclude file: {sources:?}"
        );
        // Every listed source must exist, or the vanished-source check treats a
        // path that was never there as one that just disappeared and schedules
        // a refresh on every single scan.
        for source in &sources {
            assert!(
                source.is_file(),
                "listed a source that is not there: {source:?}"
            );
        }

        // And they are digested, rather than silently dropped for having no
        // path relative to the served root.
        let stamps = ignore_stamps_of(&root, &sources);
        assert_eq!(
            stamps.len(),
            sources.len(),
            "every source must be stamped: {sources:?} -> {stamps:?}"
        );
    }

    /// `background_index_build` publishes its matcher while it is still
    /// part-way through Phase 2 and holds `snapshot_gate` for none of that. A
    /// refresh scheduled from that publish would take the gate uncontended and
    /// replace `file_stamps` from its own walk, only for the build to overwrite
    /// them from a walk that predates the new rules — an index and a stamp map
    /// describing two different trees, with no scan left to notice.
    #[test]
    fn an_ignore_refresh_waits_for_a_running_build() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let state = test_server_state(&root, &index_dir);

        state.indexing.store(true, Ordering::SeqCst);
        state.gitignore_pending.store(true, Ordering::SeqCst);
        state.ignore_rules_dirty.store(true, Ordering::SeqCst);
        schedule_ignore_rules_refresh(Arc::clone(&state), root.clone());

        // Publishing a matcher is the first thing the refresh does that anyone
        // outside it can see, so `gitignore_pending` still being set is proof
        // that it has not started.
        thread::sleep(Duration::from_millis(400));
        assert!(
            state.gitignore_pending.load(Ordering::SeqCst),
            "a refresh must not run against an index that is still being built"
        );

        state.indexing.store(false, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(30);
        while state.gitignore_pending.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !state.gitignore_pending.load(Ordering::SeqCst),
            "and must run once the build is done, rather than waiting forever"
        );
    }

    /// Stamps describe what the index holds. A file written while the build
    /// ran is read by the metadata walk that produces them but not by the
    /// content walk that fed the index, so its stamp says "current" about
    /// bytes that are already stale — and `reindex_file` returns early on a
    /// matching stamp, so the replay of the very event that reported the write
    /// reads nothing.
    #[test]
    fn a_file_written_during_the_build_is_not_stamped_by_it() {
        use tgrep_core::meta::FileStamp;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let state = test_server_state(&root, &index_dir);

        let stamps: std::collections::HashMap<String, FileStamp> = [
            ("quiet.rs".to_string(), FileStamp { mtime: 1, size: 10 }),
            ("racy.rs".to_string(), FileStamp { mtime: 2, size: 20 }),
            (
                "moved/deep.rs".to_string(),
                FileStamp { mtime: 3, size: 30 },
            ),
            (
                "moved-aside.rs".to_string(),
                FileStamp { mtime: 4, size: 40 },
            ),
        ]
        .into_iter()
        .collect();

        state
            .deferred_events
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .insert(root.join("racy.rs"), false);
        state
            .deferred_events
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .insert(root.join("moved"), true);

        let published = withhold_stamps_for_deferred(&state, &root, stamps.clone());
        assert!(
            published.contains_key("quiet.rs"),
            "a file nothing touched keeps its stamp, or the build re-reads the repository"
        );
        assert!(
            !published.contains_key("racy.rs"),
            "a file whose event is waiting to be replayed must not be stamped as indexed"
        );
        assert!(
            !published.contains_key("moved/deep.rs"),
            "a directory event must withhold stamps for every descendant the subtree replay covers"
        );
        assert!(
            published.contains_key("moved-aside.rs"),
            "directory-prefix matching must not withhold similarly named siblings"
        );

        // Overflowed: the buffer names nothing, so nothing in the map can be
        // told apart from what changed underneath it.
        *state.deferred_events.lock().unwrap() = None;
        assert!(
            withhold_stamps_for_deferred(&state, &root, stamps).is_empty(),
            "with the buffer overflowed no stamp from this build can be trusted"
        );
    }

    /// A file that grows in between must not be read into memory without
    /// bound, nor indexed past the cap.
    #[test]
    fn a_read_stops_one_byte_past_the_cap() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("grew.txt");
        std::fs::write(&path, "x".repeat(4096)).unwrap();

        // Stat said 16 bytes; the file is 4096 by the time it is read.
        let mut file = std::fs::File::open(&path).unwrap();
        assert!(matches!(
            read_within_limit(&mut file, Some(64), 16),
            CappedRead::TooLarge
        ));

        let mut file = std::fs::File::open(&path).unwrap();
        assert!(
            matches!(read_within_limit(&mut file, None, 16), CappedRead::Data(d) if d.len() == 4096),
            "no cap means no bound"
        );

        std::fs::write(&path, "small").unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        assert!(
            matches!(read_within_limit(&mut file, Some(64), 5), CappedRead::Data(d) if d == b"small"),
            "a file within the cap reads whole"
        );

        // Exactly at the cap is still within it.
        std::fs::write(&path, "x".repeat(64)).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        assert!(
            matches!(read_within_limit(&mut file, Some(64), 64), CappedRead::Data(d) if d.len() == 64)
        );
    }

    /// The size gate is checked against a fresh stat on every visit, so a file
    /// that outgrew the cap since it was indexed loses what the index holds
    /// rather than keeping the smaller version until the reconcile. (The
    /// growth this pins is between visits — growth *during* a read is what
    /// `read_within_limit` above covers.)
    #[cfg(unix)]
    #[test]
    fn a_file_that_outgrew_the_cap_between_visits_loses_its_indexed_content() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let index_dir = root.join(".tgrep");
        let mut state = test_server_state(&root, &index_dir);
        Arc::get_mut(&mut state).unwrap().max_file_size = Some(64);

        let path = root.join("grows.rs");
        std::fs::write(&path, "fn small_enough() {}\n").unwrap();
        reindex_file(&state, &path, "grows.rs", false);
        assert!(
            state.index.read().unwrap().live.has_path("grows.rs"),
            "a file under the cap should index normally"
        );

        std::fs::write(&path, "x".repeat(4096)).unwrap();
        reindex_file(&state, &path, "grows.rs", false);
        assert!(
            state.index.read().unwrap().live.is_deleted("grows.rs"),
            "content past the cap must not stay searchable"
        );
    }

    #[test]
    fn glob_filter_unix_patterns() {
        use crate::glob_filter::GlobFilter;
        let f = GlobFilter::new(&["**/*.cs".to_string()], &[], false).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(f.matches("bar.cs"));
        assert!(!f.matches("src/foo/bar.rs"));
        let f2 = GlobFilter::new(&["src/**".to_string()], &[], false).unwrap();
        assert!(f2.matches("src/foo/bar.cs"));
        assert!(!f2.matches("lib/foo/bar.cs"));
    }

    #[test]
    fn glob_filter_backslash_normalization() {
        use crate::glob_filter::GlobFilter;
        let f = GlobFilter::new(&[r"**\*.cs".to_string()], &[], false).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        let f2 = GlobFilter::new(&[r"src\**\*.cs".to_string()], &[], false).unwrap();
        assert!(f2.matches("src/foo/bar.cs"));
        let f3 = GlobFilter::new(&[r"src\**".to_string()], &[], false).unwrap();
        assert!(f3.matches("src/foo/bar.cs"));
        assert!(
            !GlobFilter::new(&[r"lib\**".to_string()], &[], false)
                .unwrap()
                .matches("src/foo/bar.cs")
        );
    }

    #[test]
    fn glob_filter_case_insensitive() {
        use crate::glob_filter::GlobFilter;
        // Globs are case-sensitive by default, as in ripgrep.
        let sensitive = GlobFilter::new(&["**/*.CS".to_string()], &[], false).unwrap();
        assert!(!sensitive.matches("src/foo/bar.cs"));
        assert!(sensitive.matches("src/foo/BAR.CS"));

        // --iglob opts a pattern into case-insensitive matching.
        let insensitive = GlobFilter::new(&[], &["**/*.CS".to_string()], false).unwrap();
        assert!(insensitive.matches("src/foo/bar.cs"));
        assert!(insensitive.matches("src/foo/BAR.CS"));
    }

    #[test]
    fn glob_filter_negation_only() {
        use crate::glob_filter::GlobFilter;
        let f = GlobFilter::new(&["!.git".to_string()], &[], false).unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(f.matches("README.md"));
        assert!(!f.matches(".git"));
        assert!(!f.matches("foo/.git"));
    }

    #[test]
    fn glob_filter_inclusion_and_exclusion() {
        use crate::glob_filter::GlobFilter;
        let f = GlobFilter::new(
            &["**/*.cs".to_string(), "!**/test/**".to_string()],
            &[],
            false,
        )
        .unwrap();
        assert!(f.matches("src/foo/bar.cs"));
        assert!(!f.matches("src/test/bar.cs"));
        assert!(!f.matches("src/foo/bar.rs"));
    }

    #[test]
    fn glob_filter_empty_passes_all() {
        use crate::glob_filter::GlobFilter;
        let f = GlobFilter::new(&[], &[], false).unwrap();
        assert!(f.matches("anything"));
    }

    fn write_file(path: &Path, content: &[u8]) {
        std::fs::write(path, content).expect("write_file");
    }

    #[test]
    fn publish_file_renames_when_target_missing() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.bin");
        let dst = tmp.path().join("dst.bin");
        write_file(&src, b"hello");
        publish_file(&src, &dst).unwrap();
        assert!(!src.exists(), "src should be moved");
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");
    }

    #[test]
    fn publish_file_replaces_existing_target() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.bin");
        let dst = tmp.path().join("dst.bin");
        write_file(&src, b"new");
        write_file(&dst, b"old");
        publish_file(&src, &dst).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"new");
    }

    #[test]
    fn publish_file_fails_fast_on_missing_source() {
        // NotFound is a structural error; should fail on the first attempt
        // without any retries (regardless of platform).
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("does_not_exist.bin");
        let dst = tmp.path().join("dst.bin");
        let err = publish_file(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        let msg = err.to_string();
        assert!(
            msg.contains("after 1 attempt"),
            "expected fast-fail (1 attempt), got: {msg}"
        );
    }

    #[test]
    fn publish_file_preserves_original_error_via_source_chain() {
        // The wrapped error should keep the original io::Error reachable
        // through std::error::Error::source() so callers can recover
        // raw_os_error() for diagnostics.
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("missing.bin");
        let dst = tmp.path().join("dst.bin");
        let err = publish_file(&src, &dst).unwrap_err();

        // Walk source chain: outer io::Error -> PublishError -> inner io::Error
        let inner_dyn =
            std::error::Error::source(&err).expect("outer error should expose its inner cause");
        let inner_io = inner_dyn
            .downcast_ref::<std::io::Error>()
            .or_else(|| {
                std::error::Error::source(inner_dyn)
                    .and_then(|s| s.downcast_ref::<std::io::Error>())
            })
            .expect("inner io::Error should be reachable via source chain");
        assert_eq!(inner_io.kind(), std::io::ErrorKind::NotFound);
        // raw_os_error is platform-specific but should be Some on the
        // platforms we target (Windows: 2, Unix: 2). Just check it's set.
        assert!(
            inner_io.raw_os_error().is_some(),
            "raw_os_error should be preserved on the inner error"
        );
    }

    #[test]
    fn move_staged_files_publishes_known_files_only() {
        let tmp = TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&staging).unwrap();
        for name in [
            "index.bin",
            "lookup.bin",
            "files.bin",
            tgrep_core::path_index::EXTRA_PATHS_FILENAME,
            "filestamps.json",
            "meta.json",
        ] {
            write_file(&staging.join(name), name.as_bytes());
        }
        write_file(&staging.join("ignored.txt"), b"nope");
        let mut moved = move_staged_files(&staging, &target).unwrap();
        moved.commit();
        for name in [
            "index.bin",
            "lookup.bin",
            "files.bin",
            tgrep_core::path_index::EXTRA_PATHS_FILENAME,
            "filestamps.json",
            "meta.json",
        ] {
            assert_eq!(std::fs::read(target.join(name)).unwrap(), name.as_bytes());
            assert!(
                !staging.join(name).exists(),
                "{name} should be moved out of staging"
            );
        }
        assert!(
            staging.join("ignored.txt").exists(),
            "unknown files should be left alone"
        );
    }

    #[test]
    fn staged_file_move_rolls_back_replaced_files() {
        let tmp = TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        write_file(&staging.join("index.bin"), b"new");
        write_file(&target.join("index.bin"), b"old");

        let mut moved = move_staged_files(&staging, &target).unwrap();
        assert_eq!(std::fs::read(target.join("index.bin")).unwrap(), b"new");
        moved.rollback().unwrap();

        assert_eq!(std::fs::read(target.join("index.bin")).unwrap(), b"old");
    }

    #[test]
    fn rollback_restores_backup_when_published_target_is_already_missing() {
        let tmp = TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        write_file(&staging.join("index.bin"), b"new");
        write_file(&target.join("index.bin"), b"old");

        let mut moved = move_staged_files(&staging, &target).unwrap();
        std::fs::remove_file(target.join("index.bin")).unwrap();
        moved.rollback().unwrap();

        assert_eq!(std::fs::read(target.join("index.bin")).unwrap(), b"old");
    }

    #[test]
    fn rollback_restores_backup_when_replacement_was_never_published() {
        let tmp = TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        write_file(&staging.join(".previous-index.bin"), b"old");

        let mut moved = StagedFileMove {
            staging,
            target: target.clone(),
            backed_up: vec!["index.bin"],
            published: Vec::new(),
            finished: false,
        };
        moved.rollback().unwrap();

        assert_eq!(std::fs::read(target.join("index.bin")).unwrap(), b"old");
    }

    #[test]
    fn move_failure_reports_rollback_failure_and_preserves_backups() {
        let tmp = TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        write_file(&staging.join(".previous-index.bin"), b"old-index");
        std::fs::create_dir(target.join("index.bin")).unwrap();

        let moved = StagedFileMove {
            staging: staging.clone(),
            target,
            backed_up: vec!["index.bin"],
            published: vec!["index.bin"],
            finished: false,
        };
        let error = moved.fail(std::io::Error::other("publish failed"));

        assert!(error.rollback_failed());
        assert_eq!(
            std::fs::read(staging.join(".previous-index.bin")).unwrap(),
            b"old-index"
        );
    }

    #[test]
    fn rollback_retry_does_not_delete_an_already_restored_backup() {
        let tmp = TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        write_file(&staging.join(".previous-index.bin"), b"old-index");
        write_file(&staging.join(".previous-lookup.bin"), b"old-lookup");
        write_file(&target.join("index.bin"), b"new-index");
        std::fs::create_dir(target.join("lookup.bin")).unwrap();

        let mut moved = StagedFileMove {
            staging,
            target: target.clone(),
            backed_up: vec!["index.bin", "lookup.bin"],
            published: vec!["index.bin", "lookup.bin"],
            finished: false,
        };
        assert!(moved.rollback().is_err());
        assert_eq!(
            std::fs::read(target.join("index.bin")).unwrap(),
            b"old-index"
        );

        std::fs::remove_dir(target.join("lookup.bin")).unwrap();
        moved.rollback().unwrap();

        assert_eq!(
            std::fs::read(target.join("index.bin")).unwrap(),
            b"old-index"
        );
        assert_eq!(
            std::fs::read(target.join("lookup.bin")).unwrap(),
            b"old-lookup"
        );
    }
}
