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
///   5. `cache`         — guards the file content LRU cache
///   6. `file_stamps`   — guards per-file mtime/size stamps
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
    cache: RwLock<ContentCache>,
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
}

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
        cache: RwLock::new(ContentCache::new(
            CACHE_CAPACITY,
            CACHE_MAX_BYTES,
            CACHE_MAX_ENTRY_BYTES,
        )),
        root: root.clone(),
        watcher_active: std::sync::atomic::AtomicBool::new(false),
        indexing: std::sync::atomic::AtomicBool::new(needs_build),
        flushing: std::sync::atomic::AtomicBool::new(false),
        // Only meaningful when the watcher runs with gitignore filtering
        // enabled; otherwise there is no matcher to wait for.
        gitignore_pending: std::sync::atomic::AtomicBool::new(!no_watch && !no_ignore),
        ignore_rules_dirty: std::sync::atomic::AtomicBool::new(false),
        ignore_refresh_scheduled: std::sync::atomic::AtomicBool::new(false),
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
) -> Option<tgrep_core::gitignore::IgnoreMatcher> {
    if state.no_ignore {
        return None;
    }

    let start = Instant::now();
    let matcher = tgrep_core::walker::build_gitignore_matcher_from_files(
        root,
        &walk.gitignore_files,
        &walk.ignore_files,
        state.no_require_git,
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

/// Publish a new ignore matcher and bring everything that depends on it up to
/// date. `None` is a legitimate matcher when no rules exist.
///
/// Callers on the stale path hold `snapshot_gate` for write, which is what
/// makes the matcher swap and the index decisions around it atomic from the
/// watcher's point of view.
///
/// Returns the directories that were newly subscribed to as a result, and the
/// moment the walk behind that decision began. Those were unwatched while the
/// caller's walk ran, so anything written to them in that window produced no
/// event and appears in no walk result. Callers pass both to
/// [`reindex_files_in`] once `state.file_stamps` describes the index they just
/// published.
#[must_use = "newly watched directories need a recovery scan or writes race the subscription"]
fn publish_ignore_matcher(
    state: &ServerState,
    root: &Path,
    matcher: Option<tgrep_core::gitignore::IgnoreMatcher>,
) -> (Vec<PathBuf>, SystemTime) {
    *state.gitignore.write().unwrap() = matcher;
    state.gitignore_pending.store(false, Ordering::SeqCst);
    // New rules mean a different set of directories worth hearing about:
    // a tightened rule releases the subscriptions under it, and a relaxed
    // one takes subscriptions for the tree it used to hide.
    sync_watch_registrations(state, root)
}

fn handle_connection(stream: TcpStream, state: &ServerState) -> Result<()> {
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

fn process_request(request: &str, state: &ServerState) -> String {
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
        "status" => handle_status(id, state),
        "reload" => handle_reload(id, state),
        _ => json_rpc_error(id, -32601, &format!("Method not found: {method}")),
    }
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
            let mut cache = state.cache.write().unwrap();
            // Promote hit entries so LRU recency stays accurate
            for key in &hit_keys {
                cache.touch(key);
            }
            // Insert disk results, re-checking for races with other threads
            for (rel_path, content) in &disk_results {
                if cache.peek(rel_path).is_none() {
                    cache.put(rel_path.clone(), Arc::clone(content));
                }
            }
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

fn handle_reload(id: Option<serde_json::Value>, state: &ServerState) -> String {
    let index_dir = state.index_dir.clone();

    // Rebuild from disk. Uses the options form rather than `build_index` so the
    // rebuild keeps the ignore semantics the server started with — a reload that
    // silently changed them would rewrite the index against different rules.
    if let Err(e) = builder::build_index_with_options(
        &state.root,
        Some(&index_dir),
        &builder::BuildOptions {
            no_ignore: state.no_ignore,
            no_require_git: state.no_require_git,
            max_file_size: state.max_file_size,
            exclude_dirs: state.exclude_dirs.clone(),
            ..Default::default()
        },
    ) {
        return json_rpc_error(id, -32000, &format!("rebuild failed: {e}"));
    }

    // Reopen index
    match HybridIndex::open(&index_dir, &state.root) {
        Ok(new_index) => {
            let mut index = state.index.write().unwrap();
            *index = new_index;
            let mut cache = state.cache.write().unwrap();
            cache.clear();
            json_rpc_result(id, serde_json::json!({"status": "reloaded"}))
        }
        Err(e) => json_rpc_error(id, -32000, &format!("reopen failed: {e}")),
    }
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
                }
                Err(TrySendError::Disconnected(_)) => {}
            },
            // Surface these. A dropped ReadDirectoryChangesW buffer looks
            // exactly like "the watcher stopped working" from the outside,
            // and silence makes it impossible to tell apart from a bug in
            // our own filtering.
            Err(e) => eprintln!("[trace] warning: file watcher error: {e}"),
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
            loop {
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
/// ReadDirectoryChangesW (Windows) and FSEvents (macOS) subscribe once for the
/// whole subtree, so there is no per-directory registration to withhold. On
/// those platforms filtering on delivery is the only lever available, and
/// [`should_skip_watcher_path`] remains it.
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

/// The watcher plus the set of directories it is currently subscribed to.
///
/// Only meaningful when [`PER_DIRECTORY_WATCHES`] is true; elsewhere `watched`
/// holds just the root, which is subscribed recursively.
struct WatchRegistry {
    watcher: RecommendedWatcher,
    watched: std::collections::HashSet<PathBuf>,
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
    /// Returns `(added, removed)`. Only for a set that describes the whole
    /// tree — anything absent from `desired` is unsubscribed. To subscribe to
    /// a subtree without disturbing the rest, use [`Self::add_all`].
    fn sync(&mut self, desired: &std::collections::HashSet<PathBuf>) -> (Vec<PathBuf>, usize) {
        let stale: Vec<PathBuf> = self.watched.difference(desired).cloned().collect();
        let mut removed = 0;
        for dir in stale {
            // Best effort. inotify drops a descriptor by itself when the
            // directory is deleted, so "not found" is an expected outcome
            // here, not an error worth reporting.
            let _ = self.watcher.unwatch(&dir);
            self.watched.remove(&dir);
            removed += 1;
        }

        (self.add_all(desired), removed)
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
fn watchable_dirs(
    root: &Path,
    start: &Path,
    exclude_dirs: &[String],
    gitignore: Option<&tgrep_core::gitignore::IgnoreMatcher>,
) -> std::collections::HashSet<PathBuf> {
    let mut found = std::collections::HashSet::new();
    found.insert(start.to_path_buf());

    let mut stack = vec![start.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            // An unreadable directory is not a reason to abandon the rest of
            // the tree; the periodic reconcile is what catches what we miss.
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(root) else {
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
    found
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
    let desired = {
        let gitignore = state.gitignore.read().unwrap();
        watchable_dirs(root, root, &state.exclude_dirs, gitignore.as_ref())
    };
    let total = desired.len();
    let (added, removed) = registry.sync(&desired);
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

/// Re-check the files directly inside `dirs`, indexing the ones that changed,
/// dropping the ones that are gone, and subscribing to subdirectories that
/// appeared while the subscriptions were being established.
///
/// Used to close the gap between a walk and the subscriptions that follow it:
/// [`reindex_file`] compares stamps first, so for a tree that did not change
/// under us this costs one `metadata` call per file and indexes nothing.
///
/// `since` is when the walk behind `dirs` began — the start of the window this
/// is closing. It is only consulted for ignore-rules files, where "did this
/// arrive after the matcher was decided" cannot be answered from the stamps:
/// the dot-prefixed ones are hidden, so they are never indexed and never have
/// one.
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
    // Directories whose listing succeeded, and the files those listings
    // contained, for the removal sweep at the end. Only files are recorded:
    // stamps describe files, so directory names would just be dead weight on a
    // set that at startup spans the whole repository.
    let mut swept: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut present: std::collections::HashSet<String> = std::collections::HashSet::new();

    for dir in dirs {
        let Ok(rel_dir) = dir.strip_prefix(root) else {
            continue;
        };
        let rel_dir = rel_dir.to_string_lossy().replace('\\', "/");
        let Ok(entries) = std::fs::read_dir(dir) else {
            // No listing means no evidence, and the sweep below must not treat
            // silence as absence.
            continue;
        };
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
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
                subdirs.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            present.insert(rel.clone());

            // An ignore-rules file that landed in this window was not seen by
            // the walk that built the matcher in force, so every other file in
            // this scan is being judged by rules that do not know about it.
            // Indexing them now would apply the wrong rules and leave whatever
            // was wrongly indexed until something touched it again.
            //
            // The mtime test is what keeps this quiet: a repository has an
            // ignore file in every other directory and the startup scan walks
            // past all of them, but they predate the walk and are already
            // accounted for. Only one written inside the window can have been
            // missed — and a spurious match (a `touch` in the same
            // millisecond) costs an idempotent refresh, not correctness.
            //
            // Bounded at both ends, not just the near one. On a network mount
            // whose server clock runs ahead of ours, every recently touched
            // file carries a future mtime and would pass a one-sided test — on
            // every scan, including the one at the end of the refresh this
            // schedules, which walks the whole repository and then arms the
            // next. Treating a future mtime as skew rather than as an arrival
            // gives up the fix on such a mount and keeps the loop closed.
            if !state.no_ignore
                && is_ignore_rules_file(root, &path)
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|m| m >= since && m <= SystemTime::now())
            {
                // Abandon the scan: the refresh rewalks and republishes, which
                // covers these directories properly, and anything indexed
                // between here and there would be judged by the stale rules.
                state.ignore_rules_dirty.store(true, Ordering::SeqCst);
                schedule_ignore_rules_refresh(Arc::clone(state), root.to_path_buf());
                eprintln!(
                    "[trace] watcher: ignore rules changed during recovery ({rel}); \
                     deferring to a refresh"
                );
                return;
            }

            let skip = {
                let gitignore = state.gitignore.read().unwrap();
                should_skip_watcher_path(&rel, &state.exclude_dirs, gitignore.as_ref())
            };
            if !skip {
                reindex_file(state, &path, &rel);
            }
        }
        swept.insert(rel_dir);

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

    sweep_removed_files(state, &swept, &present);

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
/// The caller must already hold `snapshot_gate`.
fn sweep_removed_files(
    state: &ServerState,
    swept: &std::collections::HashSet<String>,
    present: &std::collections::HashSet<String>,
) {
    if swept.is_empty() {
        return;
    }
    // One pass over the stamps rather than a lookup per swept directory: at
    // startup both sides of this span the whole repository, and anything
    // proportional to their product would not finish.
    let gone: Vec<String> = {
        let stamps = state.file_stamps.read().unwrap();
        stamps
            .keys()
            .filter(|rel| {
                let parent = rel.rsplit_once('/').map_or("", |(dir, _)| dir);
                swept.contains(parent) && !present.contains(rel.as_str())
            })
            .cloned()
            .collect()
    };
    if gone.is_empty() {
        return;
    }
    eprintln!(
        "[trace] watcher: dropped {} file(s) removed while subscriptions were \
         being established",
        gone.len()
    );
    {
        let mut index = state.index.write().unwrap();
        for rel in &gone {
            index.live.delete_file(rel);
        }
    }
    {
        let mut stamps = state.file_stamps.write().unwrap();
        for rel in &gone {
            stamps.remove(rel);
        }
    }
    if let Ok(mut cache) = state.cache.write() {
        for rel in &gone {
            cache.pop(rel);
        }
    }
}

/// Subscribe to a directory that has just appeared, and to anything already
/// inside it.
///
/// Non-recursive subscriptions are not extended by notify — it only auto-adds
/// watches beneath a watch that was registered as recursive — so a new
/// directory has to be picked up here or its contents are invisible.
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
    // would not have walked into.
    if !is_real_dir(dir) {
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
        {
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
                    // A subtree that arrives whole — a clone, a `mv`, a branch
                    // switch — can carry its own ignore rules. Those files are
                    // dot-prefixed, so the scan below would silently drop them
                    // and index the rest of the subtree against rules that do
                    // not know about them.
                    //
                    // Abandon the descent immediately rather than finishing it.
                    // Everything gathered from here on is discarded by the
                    // refresh anyway, and the rules that are about to be
                    // published are the ones that decide whether these
                    // directories should be watched at all — continuing would
                    // subscribe to every level of, say, a `node_modules/` that
                    // was just moved into place, which on Linux is a watch
                    // descriptor apiece and the exhaustion this pass exists to
                    // avoid. The refresh's `sync` would prune them, but only
                    // after they had already been taken.
                    if !state.no_ignore && is_ignore_rules_file(root, &path) {
                        found_ignore_rules = true;
                        break 'descend;
                    }
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
        reindex_file(state, path, rel);
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

/// Recheck newly watched directories on a background thread.
///
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
/// whole repository alongside the build that is already doing it.
fn spawn_recovery_scan(
    state: &Arc<ServerState>,
    root: &Path,
    dirs: Vec<PathBuf>,
    since: SystemTime,
) {
    if dirs.is_empty() || !PER_DIRECTORY_WATCHES {
        return;
    }
    let state = Arc::clone(state);
    let root = root.to_path_buf();
    let spawned = thread::Builder::new()
        .name("tgrep-watch-recovery".into())
        .spawn(move || {
            while state.indexing.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(200));
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

fn handle_fs_event(state: &Arc<ServerState>, root: &Path, event: &Event) {
    let dominated_kinds = matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    );
    if !dominated_kinds {
        return;
    }

    let ignore_rules_changed = !state.no_ignore
        && event
            .paths
            .iter()
            .any(|path| is_ignore_rules_file(root, path));
    if ignore_rules_changed {
        state.ignore_rules_dirty.store(true, Ordering::SeqCst);
        if state.indexing.load(Ordering::SeqCst) {
            return;
        }
        schedule_ignore_rules_refresh(Arc::clone(state), root.to_path_buf());
        return;
    }

    // Skip ordinary file events while the initial background index build is in
    // progress. The indexer will pick up those files itself.
    if state.indexing.load(Ordering::SeqCst) {
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

        let is_remove = matches!(event.kind, EventKind::Remove(_)) || !path.exists();

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
            let known_path = state.file_stamps.read().unwrap().contains_key(&rel_path);
            if known_path {
                eprintln!("[trace] reindex: removed {rel_path}");
            }
            // gate acquired at the function level — the entire event
            // is processed atomically with respect to flush/auto-save.
            state.index.write().unwrap().live.delete_file(&rel_path);
            state.file_stamps.write().unwrap().remove(&rel_path);
            if let Ok(mut cache) = state.cache.write() {
                cache.pop(&rel_path);
            }
            continue;
        }

        // `is_file` follows symlinks, so a link to a file lands in
        // `reindex_file` below rather than here — deliberately: that is where
        // it is recognised as ineligible and any content indexed under that
        // path before it became a link is dropped.
        if !path.is_file() {
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
            // `is_real_dir` rather than `is_dir`: the latter follows symlinks,
            // and a link to a directory is not something the walker descends
            // into, so subscribing to and indexing its target would pull in a
            // tree the index never contained — possibly outside `root`.
            if PER_DIRECTORY_WATCHES && introduces_dir && is_real_dir(path) {
                // With non-recursive subscriptions notify will not extend the
                // watch set for us, so a directory that just appeared — and
                // anything already inside it — has to be picked up here.
                watch_new_subtree(state, root, path);
            }
            continue;
        }

        reindex_file(state, path, &rel_path);
    }
}

/// Read a file and merge it into the live index, unless its stamp says the
/// content we already indexed is current.
///
/// The caller must hold `snapshot_gate`: the read, the commit, and the stamp
/// update have to be atomic with respect to a flush or auto-save.
fn reindex_file(state: &ServerState, path: &Path, rel_path: &str) {
    use tgrep_core::meta::FileStamp;

    // Compute the file's current stamp and skip if it matches what we
    // last indexed. notify on Windows in particular fires Modify events
    // for atime/attribute updates, opens, etc. — re-indexing on those
    // would re-read large files, churn the live overlay, and produce a
    // misleading "modified" trace for files that didn't actually change.
    //
    // `symlink_metadata` describes the link itself; `metadata` would describe
    // its target. See the eligibility check below for why that distinction
    // matters here.
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    let current = FileStamp {
        mtime: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0),
        size: meta.len(),
    };

    // The rules `walk_file_metadata` applies, and for the same reason: the walk
    // is authoritative about what belongs in the index, so anything it rejects
    // must not be added here. Without this a file that grew past the cap — or
    // an ineligible extension in a directory a relaxed ignore rule just exposed
    // — would be read whole and indexed, and the next reconcile would silently
    // delete it again.
    //
    // `is_file` on the link's own metadata is the third rule, and the one with
    // teeth: the walker runs with `follow_links(false)`, where a symlink is
    // neither file nor dir and is skipped outright. Following one here would
    // read the target and index its bytes under the link's path — and the
    // target need not be under `root` at all, so a link committed to a branch
    // (or dropped in by a build) is enough to pull `~/.ssh/id_rsa` into an
    // index whose whole contract is that it covers the served tree.
    let eligible = !tgrep_core::walker::is_binary_extension(path)
        && !state
            .max_file_size
            .is_some_and(|limit| current.size > limit);
    if !eligible {
        // It may have been eligible when it was last indexed — a file can grow
        // past the cap, and a real file can be replaced by a link to one. Drop
        // what we hold so the index matches the walk rather than keeping a
        // stale copy of the smaller version until the reconcile.
        //
        // The stamp is not the only evidence that something is indexed. A file
        // the watcher added since the last flush lives in the live overlay,
        // and a stamp map that was replaced wholesale — by a stale merge, or
        // by a load that failed and left it empty — no longer mentions it, so
        // testing the stamp alone would skip the drop and keep serving content
        // the walk rejects. `has_path` covers the overlay; an entry that is
        // only in the persisted reader with no stamp to match is beyond what
        // can be checked cheaply here (the reader has no path index) and is
        // left to the reconcile's membership diff.
        //
        // Both are checked before deleting rather than deleting unconditionally
        // the way the removal branch does: removals are rare, but this runs for
        // every ineligible file a recovery scan walks past, and `delete_file`
        // records a tombstone and dirties the overlay even when the path was
        // never indexed.
        let had_stamp = state
            .file_stamps
            .write()
            .unwrap()
            .remove(rel_path)
            .is_some();
        let in_overlay = state.index.read().unwrap().live.has_path(rel_path);
        if had_stamp || in_overlay {
            eprintln!("[trace] reindex: dropped {rel_path} (no longer eligible)");
            state.index.write().unwrap().live.delete_file(rel_path);
            if let Ok(mut cache) = state.cache.write() {
                cache.pop(rel_path);
            }
        }
        return;
    }

    if state.file_stamps.read().unwrap().get(rel_path) == Some(&current) {
        return;
    }

    // Read contents and extract trigrams OUTSIDE the index write lock
    // so a concurrent search (which needs a read lock) is not blocked
    // on our file I/O and trigram parsing. Windows' SRWLock is
    // writer-preferring: a single waiting writer here would otherwise
    // stall every subsequent search request.
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return,
    };
    let text = tgrep_core::encoding::decode_for_index(&data);
    let is_binary = tgrep_core::trigram::is_binary(&text);
    let per_tri = if is_binary {
        None
    } else {
        Some(tgrep_core::live::LiveIndex::compute_trigram_masks(&text))
    };

    eprintln!("[trace] reindex: modified {rel_path}");
    // Gate held by the caller — the commit + stamp update is processed
    // atomically with respect to flush/auto-save.
    {
        let mut index = state.index.write().unwrap();
        match per_tri {
            Some(per_tri) => index.live.commit_upsert(rel_path, per_tri),
            None => index.live.delete_file(rel_path),
        }
    }
    state
        .file_stamps
        .write()
        .unwrap()
        .insert(rel_path.to_string(), current);
    if let Ok(mut cache) = state.cache.write() {
        cache.pop(rel_path);
    }
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
            if stream_merge_stale_changes(&state, &[], &[], &[], &stamps, "auto-save", false) {
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
    let mut meta = IndexMeta::new("", 0, 0);
    meta.complete = false; // empty skeleton — not a complete index
    meta.save(index_dir)?;
    Ok(())
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

/// Apply a stale diff without materializing the existing index in heap.
///
/// The ordinary incremental flush uses `HybridIndex::full_snapshot`, whose
/// memory is proportional to every posting already on disk. That is especially
/// harmful when a newer tgrep first opens an index built with an older file-size
/// cap: every formerly-oversized file appears as new at once. Build new and
/// replacement files into a bounded external-sort delta, then stream it together
/// with the old index while filtering replaced and deleted reader entries.
///
/// The caller holds `snapshot_gate` across the metadata walk and this merge, so
/// the walk's exact path set is newer than every live entry captured here.
fn stream_merge_stale_changes(
    state: &Arc<ServerState>,
    changed: &[String],
    added: &[String],
    deleted: &[String],
    stamps: &std::collections::HashMap<String, tgrep_core::meta::FileStamp>,
    operation: &str,
    authoritative_membership: bool,
) -> bool {
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
        .filter(|path| seen.insert((*path).clone()))
        .cloned()
        .collect();
    if authoritative_membership {
        candidates.extend(
            reader
                .all_paths()
                .iter()
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
    let removed: std::collections::HashSet<String> = candidates.iter().cloned().collect();

    let mut published_stamps = stamps.clone();

    let result = (|| -> Result<bool> {
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
        {
            let mut memo = state.unreadable.write().unwrap();
            // Anything this delta was asked to build is settled: either it was
            // read, or it is in `outcome.unreadable` and re-recorded below.
            for path in changed.iter().chain(added).chain(deleted) {
                memo.remove(path);
            }
            for path in &outcome.unreadable {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if let Some(stamp) = published_stamps.remove(&rel) {
                    memo.insert(rel, stamp);
                }
            }
        }
        if !outcome.unreadable.is_empty() {
            eprintln!(
                "[trace] {} file(s) were unreadable during the delta build; \
                 their stamps are withheld so a later reconcile retries them",
                outcome.unreadable.len()
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
        let published = publish_staged_index(state, index_dir, &staging_dir, expected_files);
        if published {
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
    let _ = std::fs::remove_dir_all(&staging_dir);
    state.flushing.store(false, Ordering::SeqCst);
    if matches!(&result, Ok(true)) {
        *state.file_stamps.write().unwrap() = published_stamps;
    }
    match result {
        Ok(true) => {
            eprintln!(
                "[trace] {operation}: streamed {} changes into the index in {:.1}s",
                candidates.len(),
                start.elapsed().as_secs_f64()
            );
            true
        }
        Ok(false) => {
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
    let _refresh = state.stale_refresh_lock.lock().unwrap();
    // Keep watcher/auto-save mutations out for the complete walk → matcher →
    // merge → recovery cycle. Search queries do not take this gate and remain
    // available. Held here rather than inside so the recovery scan below is
    // still covered by it.
    let _gate = state.snapshot_gate.write().unwrap();

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
    ok
}

fn refresh_stale_locked(
    state: &Arc<ServerState>,
    root: &Path,
    index_dir: &Path,
    compare_index_membership: bool,
    newly_watched: &mut Vec<PathBuf>,
) -> bool {
    use tgrep_core::meta;
    use tgrep_core::walker;

    let start = Instant::now();
    eprintln!("[trace] stale check: comparing index against filesystem...");

    // Walk first. This single traversal feeds both the stale diff and the
    // watcher's ignore matcher, and it must run before the early returns below
    // so the matcher can be published on every path out of this function.
    let walk = walker::walk_file_metadata(
        root,
        &walker::MetaWalkOptions {
            exclude_dirs: state.exclude_dirs.clone(),
            no_ignore: state.no_ignore,
            no_require_git: state.no_require_git,
            max_file_size: state.max_file_size,
        },
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
    // The caller anchored the recovery window at its own walk, which starts
    // earlier than the subscription sync inside this call, so the timestamp
    // that comes back here is the looser of the two and is dropped.
    *newly_watched = publish_ignore_matcher(state, root, build_stale_matcher(state, root, &walk)).0;

    if walk.skipped_error > 0 {
        eprintln!(
            "[trace] warning: stale check could not inspect {} filesystem entries \
             (walk: {walk_ms}ms); keeping the old index",
            walk.skipped_error
        );
        return false;
    }
    let current_meta = &walk.files;

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
        "stale check",
        true,
    ) {
        return false;
    }

    if let Ok(mut cache) = state.cache.write() {
        for path in changed.iter().chain(added.iter()).chain(deleted.iter()) {
            cache.pop(path);
        }
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
            *state.index.write().unwrap() = empty;
            state.cache.write().unwrap().clear();
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
    eprintln!("[trace] bootstrapping index with the external merge sort (memory-bounded)...");

    // Dropped once the build is done so the sampled peak (on platforms without
    // a kernel high-water mark) covers the whole of it. Unlike the incremental
    // path below, nothing here polls memory on its own.
    let sampler = crate::mem::PrivatePeakSampler::start();
    let outcome = match builder::build_index_with_options(
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
    *state.index.write().unwrap() = opened;
    state.cache.write().unwrap().clear();
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
    match tgrep_core::meta::read_filestamps(index_dir) {
        Ok(stamps) => *state.file_stamps.write().unwrap() = stamps,
        Err(e) => eprintln!(
            "[trace] warning: could not load file stamps ({e}); \
             the watcher may reindex on spurious events"
        ),
    }
    if state.watch_enabled && !state.no_ignore {
        let t_gi = Instant::now();
        let matcher = tgrep_core::walker::build_gitignore_matcher_from_files(
            root,
            &outcome.gitignore_files,
            &outcome.ignore_files,
            state.no_require_git,
        );
        let found = matcher.is_some();
        // "Newly watched" here is every directory in the repository, and the
        // build's walk ran before any of them were subscribed. Deferred rather
        // than skipped: the scan waits out `indexing` and then costs one
        // `metadata` call per file, since the stamps this build just wrote
        // describe the index exactly.
        let (newly_watched, since) = publish_ignore_matcher(state, root, matcher);
        spawn_recovery_scan(state, root, newly_watched, since);
        eprintln!(
            "[trace] gitignore matcher built from {} file(s) in {:.1}ms{}",
            outcome.gitignore_files.len(),
            t_gi.elapsed().as_secs_f64() * 1000.0,
            if found { "" } else { " (no rules found)" }
        );
    }

    state.indexing.store(false, Ordering::SeqCst);
    drop(gate);

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
    let walk = walker::walk_dir(
        root,
        &WalkOptions {
            include_hidden: false,
            no_ignore: state.no_ignore,
            no_require_git: state.no_require_git,
            max_file_size: state.max_file_size,
            collect_gitignore_files: state.watch_enabled && !state.no_ignore,
            exclude_dirs: state.exclude_dirs.clone(),
            ..Default::default()
        },
    );

    if state.watch_enabled && !state.no_ignore {
        let start = Instant::now();
        let matcher = walker::build_gitignore_matcher_from_files(
            root,
            &walk.gitignore_files,
            &walk.ignore_files,
            state.no_require_git,
        );
        let has_matcher = matcher.is_some();
        // Subscriptions are taken here, partway through the build, so files
        // written to a directory the walk has already passed are in neither
        // the build's results nor any event. The scan waits for the build to
        // finish before looking, because until then the stamps describe
        // nothing and every file would read as changed.
        let (newly_watched, since) = publish_ignore_matcher(state, root, matcher);
        spawn_recovery_scan(state, root, newly_watched, since);
        eprintln!(
            "[trace] gitignore matcher built from index walk in {:.1}ms \
             ({} .gitignore + {} .ignore files{})",
            start.elapsed().as_secs_f64() * 1000.0,
            walk.gitignore_files.len(),
            walk.ignore_files.len(),
            if has_matcher { "" } else { ", no rules found" }
        );
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
    let stamps: std::collections::HashMap<String, tgrep_core::meta::FileStamp> = walk_meta
        .files
        .into_iter()
        .map(|fm| {
            (
                fm.relative_path,
                tgrep_core::meta::FileStamp {
                    mtime: fm.mtime,
                    size: fm.size,
                },
            )
        })
        .collect();

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
    *state.file_stamps.write().unwrap() = stamps;
    state.indexing.store(false, Ordering::SeqCst);

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

    // Reclaim memory held by the indexing-time live overlay — but only when
    // the flush actually completed and `prune_persisted_entries` ran. If the
    // flush failed, the overlay is still the source of truth and shrinking
    // the indexing-sized maps would just waste the write lock with no benefit.
    if pruned {
        let mut index = state.index.write().unwrap();
        index.live.shrink_to_fit();
    }

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

    let pruned = publish_staged_index(state, index_dir, &staging_dir, num_files);
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
/// Returns `true` when the swap + prune succeeded, `false` on any failure (the
/// previous reader and the live overlay are retained as the fallback).
fn publish_staged_index(
    state: &ServerState,
    index_dir: &Path,
    staging_dir: &Path,
    num_files: usize,
) -> bool {
    // Held across move + open + swap so concurrent publishers (auto-save /
    // background-build / watcher reindex flush) cannot interleave renames
    // or swap readers out of order. Searches do not take this lock.
    let _publish = state.publish_lock.lock().unwrap();
    if let Err(e) = move_staged_files(staging_dir, index_dir) {
        eprintln!("[trace] warning: flush move failed: {e}");
        let _ = std::fs::remove_dir_all(staging_dir);
        return false;
    }

    // Open the new reader. The publish mutex is intentionally still held
    // here so that move + open + swap form an atomic publish unit (no other
    // publisher can interleave a rename or swap a competing reader between
    // these steps). The server-wide `state.index` RwLock is NOT taken, so
    // search queries continue to be served by the previous reader (whose
    // `Arc<IndexReader>` they hold) throughout this call.
    //
    // On Windows, NTFS metadata for a recently-renamed file can transiently
    // appear stale (zero-length), causing IndexReader::open to create a
    // degenerate reader with files but no trigrams. We retry a few times
    // with a short backoff to ride out the transient.
    let pruned = 'open: {
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
                        if attempt + 1 < READER_OPEN_RETRIES {
                            thread::sleep(READER_OPEN_BACKOFF * (attempt + 1));
                            continue;
                        }
                        eprintln!(
                            "[trace] warning: degenerate reader persists after \
                             {READER_OPEN_RETRIES} attempts, keeping live overlay as fallback"
                        );
                        break 'open false;
                    }

                    // Validate + warm the lookup mmap before swapping the
                    // reader in. This catches corruption (unsorted lookup
                    // table, out-of-bounds posting offsets) and, as a
                    // side-effect, pages in every byte of lookup.bin so that
                    // subsequent binary searches never hit cold mmap pages
                    // — preventing the zero-candidate failure observed on
                    // Windows after flush.
                    if let Err(msg) = new_reader.validate_lookup() {
                        eprintln!(
                            "[trace] warning: reader validation failed \
                             (attempt {}/{READER_OPEN_RETRIES}): {msg}",
                            attempt + 1
                        );
                        if attempt + 1 < READER_OPEN_RETRIES {
                            thread::sleep(READER_OPEN_BACKOFF * (attempt + 1));
                            continue;
                        }
                        eprintln!(
                            "[trace] warning: reader validation failed after \
                             {READER_OPEN_RETRIES} attempts, keeping live overlay"
                        );
                        break 'open false;
                    }

                    if reader_files >= num_files {
                        // Atomic swap — no outer write lock required.
                        state.index.read().unwrap().swap_reader(new_reader);
                        // Brief write lock for in-memory overlay maintenance only.
                        {
                            let mut index = state.index.write().unwrap();
                            index.prune_persisted_entries();
                            index.live.reset_dirty_count();
                        }
                        eprintln!(
                            "[trace] flush: reader reopened ({reader_files} files, \
                             {reader_trigrams} trigrams), overlay pruned"
                        );
                        break 'open true;
                    } else {
                        eprintln!(
                            "[trace] warning: reader has {reader_files} files \
                             (expected {num_files}), keeping live overlay as fallback"
                        );
                        break 'open false;
                    }
                }
                Err(e) => {
                    if attempt + 1 < READER_OPEN_RETRIES {
                        eprintln!(
                            "[trace] warning: reader open failed (attempt {}/{READER_OPEN_RETRIES}): {e}",
                            attempt + 1
                        );
                        thread::sleep(READER_OPEN_BACKOFF * (attempt + 1));
                        continue;
                    }
                    eprintln!(
                        "[trace] warning: failed to reopen reader after flush: {e}, \
                         live overlay retained"
                    );
                    break 'open false;
                }
            }
        }
        false
    };
    let _ = std::fs::remove_dir_all(staging_dir);
    pruned
}

/// Move index files from staging to the target directory.
///
/// Files are published in a fixed order, with `meta.json` last. This is only a
/// convention for publication layout; it does not provide atomic publish
/// semantics or reader-side validation by itself.
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
fn move_staged_files(staging: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    // Data files first, meta last.
    for name in &[
        "index.bin",
        "lookup.bin",
        "files.bin",
        "filestamps.json",
        "meta.json",
    ] {
        let src = staging.join(name);
        let dst = target.join(name);
        if !src.exists() {
            continue;
        }
        publish_file(&src, &dst)?;
    }
    Ok(())
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
    #[cfg(unix)]
    fn test_server_state(root: &Path, index_dir: &Path) -> Arc<ServerState> {
        create_empty_index(index_dir).expect("create empty index");
        let hybrid = HybridIndex::open(index_dir, root).expect("open empty index");
        Arc::new(ServerState {
            index: RwLock::new(hybrid),
            cache: RwLock::new(ContentCache::new(
                CACHE_CAPACITY,
                CACHE_MAX_BYTES,
                CACHE_MAX_ENTRY_BYTES,
            )),
            root: root.to_path_buf(),
            watcher_active: std::sync::atomic::AtomicBool::new(false),
            indexing: std::sync::atomic::AtomicBool::new(false),
            flushing: std::sync::atomic::AtomicBool::new(false),
            gitignore_pending: std::sync::atomic::AtomicBool::new(true),
            ignore_rules_dirty: std::sync::atomic::AtomicBool::new(false),
            ignore_refresh_scheduled: std::sync::atomic::AtomicBool::new(false),
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
        })
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
    fn watch_registry_add_all_is_additive_but_sync_prunes() {
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

        // `sync`, by contrast, is authoritative over the whole tree.
        let (added, removed) = registry.sync(&[c.clone()].into_iter().collect());
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
        for name in ["index.bin", "lookup.bin", "files.bin", "meta.json"] {
            write_file(&staging.join(name), name.as_bytes());
        }
        write_file(&staging.join("ignored.txt"), b"nope");
        move_staged_files(&staging, &target).unwrap();
        for name in ["index.bin", "lookup.bin", "files.bin", "meta.json"] {
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
}
