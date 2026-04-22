/// `tgrep serve` — TCP JSON-RPC server with file watcher.
///
/// Keeps the trigram index in memory (HybridIndex), watches for filesystem
/// changes, and serves search/status queries over TCP.
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

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
const AUTO_SAVE_MUTATIONS: u32 = 5000;
const AUTO_SAVE_INTERVAL: Duration = Duration::from_secs(600); // 10 minutes

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

struct ServerState {
    index: RwLock<HybridIndex>,
    cache: RwLock<LruCache<String, Arc<String>>>,
    root: PathBuf,
    watcher_active: std::sync::atomic::AtomicBool,
    /// True while the initial index build is in progress.
    indexing: std::sync::atomic::AtomicBool,
    /// True while a bulk flush to disk is running. Internal-only; not
    /// surfaced through `status`. Used to suppress the auto-save loop
    /// from kicking off a redundant parallel snapshot while the bulk
    /// flush (or stale-refresh flush) is still executing.
    flushing: std::sync::atomic::AtomicBool,
    /// Progress: number of files indexed so far.
    index_progress: std::sync::atomic::AtomicU64,
    /// Total files discovered for indexing.
    index_total: std::sync::atomic::AtomicU64,
    /// Directories to exclude from indexing.
    exclude_dirs: Vec<String>,
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
    /// Gitignore matcher used by the file watcher to drop events for
    /// paths the initial walk would have skipped via the `ignore` crate
    /// (`.gitignore`, `.git/info/exclude`, global gitignore, etc.).
    /// Built once at startup; `None` if the matcher could not be built
    /// (in which case the watcher just falls back to its hidden / exclude
    /// filtering and accepts that gitignored files may be reindexed).
    gitignore: Option<tgrep_core::gitignore::Gitignore>,
}

struct SearchOpts {
    files_only: bool,
    invert_match: bool,
    only_matching: bool,
    max_count: Option<usize>,
    before_context: usize,
    after_context: usize,
}

pub fn run(
    root: &Path,
    index_path: Option<&Path>,
    no_watch: bool,
    exclude_dirs: &[String],
) -> Result<()> {
    let serve_start = Instant::now();
    let root = std::fs::canonicalize(root)?;
    let index_dir = index_path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| builder::default_index_dir(&root));

    // Ensure only one server runs per index directory.
    // The lock file is held for the lifetime of the server and released on exit.
    let _lock_file = try_acquire_server_lock(&index_dir)?;

    let has_index = index_dir.join("lookup.bin").exists();

    if !has_index {
        // Create an empty on-disk index so HybridIndex can open it.
        std::fs::create_dir_all(&index_dir)?;
        create_empty_index(&index_dir)?;
        eprintln!("[trace] no existing index found, will build in background");
    }

    // Open the hybrid index (may be empty or partial from a previous checkpoint)
    let index_start = Instant::now();
    let hybrid = HybridIndex::open(&index_dir, &root)?;
    let existing_files = hybrid.num_files();

    // Check meta.json complete flag to decide whether to rebuild
    let index_complete = tgrep_core::meta::IndexMeta::load(&index_dir)
        .map(|m| m.complete)
        .unwrap_or(false);

    let needs_build = !has_index || !index_complete;

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
        cache: RwLock::new(LruCache::new(NonZeroUsize::new(CACHE_CAPACITY).unwrap())),
        root: root.clone(),
        watcher_active: std::sync::atomic::AtomicBool::new(false),
        indexing: std::sync::atomic::AtomicBool::new(needs_build),
        flushing: std::sync::atomic::AtomicBool::new(false),
        index_progress: std::sync::atomic::AtomicU64::new(0),
        index_total: std::sync::atomic::AtomicU64::new(0),
        exclude_dirs: exclude_dirs.to_vec(),
        publish_lock: Mutex::new(()),
        file_stamps: RwLock::new(tgrep_core::meta::read_filestamps(&index_dir).unwrap_or_default()),
        snapshot_gate: RwLock::new(()),
        // Only pay the cost of walking the tree to gather .gitignore
        // rules when we'll actually use them — i.e. when the file watcher
        // is enabled. With --no-watch the matcher would just sit unused
        // but we'd still have eaten a full-tree walk at startup.
        gitignore: if no_watch {
            None
        } else {
            tgrep_core::gitignore::build_matcher(&root)
        },
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
        "[trace] serve ready in {:.1}ms. TCP on port {}. Cache: max {} entries.",
        serve_start.elapsed().as_secs_f64() * 1000.0,
        port,
        CACHE_CAPACITY
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
            background_refresh_stale(&stale_state, &stale_root, &stale_index_dir);
        });
    }

    // Start file watcher (unless --no-watch)
    let _watcher = if no_watch {
        eprintln!("[trace] file watcher disabled (--no-watch)");
        None
    } else {
        let watcher_state = Arc::clone(&state);
        let watcher_root = root.clone();
        start_file_watcher(watcher_state, &watcher_root)
    };

    // Set up graceful shutdown
    let shutdown_index_dir = index_dir.clone();
    ctrlc_handler(move || {
        eprintln!("\n[trace] shutting down...");
        ServerInfo::cleanup(&shutdown_index_dir);
        std::process::exit(0);
    });

    // Start auto-save thread
    let save_state = Arc::clone(&state);
    let save_index_dir = index_dir.clone();
    thread::spawn(move || auto_save_loop(save_state, &save_index_dir));

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

fn handle_search(
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
    state: &ServerState,
) -> String {
    let start = Instant::now();

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
    let glob_filters: Vec<String> = params
        .get("glob")
        .and_then(|g| g.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let file_type = params
        .get("file_type")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string());
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
    let files_only = params
        .get("files_only")
        .and_then(|f| f.as_bool())
        .unwrap_or(false);

    // Build combined regex from all patterns
    let mut all_patterns = vec![pattern.to_string()];
    all_patterns.extend(extra_patterns);

    let re = match crate::search::build_combined_regex(
        &all_patterns,
        case_insensitive,
        fixed_string,
        word_boundary,
        multiline,
    ) {
        Ok(r) => r,
        Err(e) => return json_rpc_error(id, -32602, &format!("{e}")),
    };

    // Build query plan from primary pattern for index filtering
    let plan = if fixed_string {
        query::build_literal_plan(pattern, case_insensitive)
    } else {
        match query::build_query_plan(pattern, case_insensitive) {
            Ok(p) => p,
            Err(e) => return json_rpc_error(id, -32602, &e),
        }
    };

    // Collect candidates and their paths/full_paths while holding the index lock briefly
    let t_index = Instant::now();
    let (candidate_info, raw_candidate_count): (Vec<(String, PathBuf)>, usize) = {
        let index = state.index.read().unwrap();

        let candidates = index.execute_query_with_masks(&plan);
        let raw_count = candidates.len();

        // Diagnostic counters for filter stages
        let mut no_path_count: usize = 0;
        let mut type_filtered_count: usize = 0;
        let mut glob_filtered_count: usize = 0;

        let filtered: Vec<(String, PathBuf)> = candidates
            .iter()
            .filter_map(|&fid| {
                let rel_path = match index.file_path(fid) {
                    Some(p) => p.to_string(),
                    None => {
                        no_path_count += 1;
                        return None;
                    }
                };
                if let Some(ref type_name) = file_type
                    && !tgrep_core::filetypes::matches_type(&rel_path, type_name)
                {
                    type_filtered_count += 1;
                    return None;
                }
                if !glob_filters.is_empty()
                    && !glob_filters.iter().any(|g| simple_glob_match(g, &rel_path))
                {
                    glob_filtered_count += 1;
                    return None;
                }
                let full_path = index.full_path(fid)?;
                Some((rel_path, full_path))
            })
            .collect();

        // Log filter breakdown when raw candidates are dropped to zero
        if raw_count > 0 && filtered.is_empty() {
            eprintln!(
                "[trace] filter: raw={raw_count} no_path={no_path_count} \
                 type_filtered={type_filtered_count} glob_filtered={glob_filtered_count} \
                 file_type={file_type:?} globs={glob_count}",
                glob_count = glob_filters.len(),
            );
        }

        (filtered, raw_count)
    }; // index lock released here
    let index_ms = t_index.elapsed().as_secs_f64() * 1000.0;

    // Resolve file contents from cache (LRU) or disk
    let t_resolve = Instant::now();
    let candidate_contents: Vec<(String, Arc<String>)> = candidate_info
        .iter()
        .filter_map(|(rel_path, full_path)| {
            let mut cache = state.cache.write().unwrap();
            let content = if let Some(cached) = cache.get(rel_path) {
                Arc::clone(cached)
            } else {
                let c = std::fs::read_to_string(full_path).ok()?;
                let arc = Arc::new(c);
                cache.put(rel_path.clone(), Arc::clone(&arc));
                arc
            };
            Some((rel_path.clone(), content))
        })
        .collect();
    let resolve_ms = t_resolve.elapsed().as_secs_f64() * 1000.0;

    let has_context = before_context > 0 || after_context > 0;
    let opts = SearchOpts {
        files_only,
        invert_match,
        only_matching,
        max_count,
        before_context,
        after_context,
    };

    // Parallel regex matching across candidate files
    let t_search = Instant::now();
    let matches: Vec<serde_json::Value> = if has_context {
        // Context mode: sequential (needs ordered output)
        candidate_contents
            .iter()
            .flat_map(|(rel_path, content)| search_file_matches(rel_path, content, &re, &opts))
            .collect()
    } else {
        // No context: parallel search
        candidate_contents
            .par_iter()
            .flat_map(|(rel_path, content)| search_file_matches(rel_path, content, &re, &opts))
            .collect()
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
        "num_matches": matches.len(),
        "elapsed_ms": elapsed_ms,
    });

    json_rpc_result(id, result)
}

fn search_file_matches(
    rel_path: &str,
    content: &str,
    re: &regex::Regex,
    opts: &SearchOpts,
) -> Vec<serde_json::Value> {
    let effective_max = if opts.files_only {
        Some(1)
    } else {
        opts.max_count
    };

    let lines: Vec<&str> = content.lines().collect();

    // Find matching line indices
    let mut match_indices: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let is_match = re.is_match(line);
        let include = if opts.invert_match {
            !is_match
        } else {
            is_match
        };
        if include {
            match_indices.push(i);
            if let Some(max) = effective_max
                && match_indices.len() >= max
            {
                break;
            }
        }
    }

    if match_indices.is_empty() {
        return Vec::new();
    }

    let has_context = opts.before_context > 0 || opts.after_context > 0;

    if has_context {
        let mut results = Vec::new();
        let mut printed = std::collections::BTreeSet::new();
        let mut is_match_line = std::collections::HashSet::new();
        for &mi in &match_indices {
            is_match_line.insert(mi);
            let ctx_start = mi.saturating_sub(opts.before_context);
            let ctx_end = (mi + opts.after_context + 1).min(lines.len());
            for j in ctx_start..ctx_end {
                printed.insert(j);
            }
        }
        for &li in &printed {
            if is_match_line.contains(&li) {
                let mc = if opts.only_matching {
                    re.find_iter(lines[li])
                        .map(|m| m.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    lines[li].to_string()
                };
                let col = re.find(lines[li]).map(|m| m.start() + 1);
                let mut entry = serde_json::json!({
                    "type": "match",
                    "file": rel_path,
                    "line": li + 1,
                    "content": mc,
                });
                if let Some(c) = col {
                    entry["column"] = serde_json::json!(c);
                }
                results.push(entry);
            } else {
                results.push(serde_json::json!({
                    "type": "context",
                    "file": rel_path,
                    "line": li + 1,
                    "content": lines[li],
                }));
            }
        }
        results
    } else {
        match_indices
            .iter()
            .map(|&mi| {
                let mc = if opts.only_matching {
                    re.find_iter(lines[mi])
                        .map(|m| m.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    lines[mi].to_string()
                };
                let col = re.find(lines[mi]).map(|m| m.start() + 1);
                let mut entry = serde_json::json!({
                    "type": "match",
                    "file": rel_path,
                    "line": mi + 1,
                    "content": mc,
                });
                if let Some(c) = col {
                    entry["column"] = serde_json::json!(c);
                }
                entry
            })
            .collect()
    }
}

fn handle_status(id: Option<serde_json::Value>, state: &ServerState) -> String {
    let index = state.index.read().unwrap();
    let cache = state.cache.read().unwrap();
    let indexing = state.indexing.load(std::sync::atomic::Ordering::Relaxed);

    let result = serde_json::json!({
        "num_files": index.num_files(),
        "num_trigrams": index.num_trigrams(),
        "cache_size": cache.len(),
        "cache_capacity": CACHE_CAPACITY,
        "watcher_active": state.watcher_active.load(std::sync::atomic::Ordering::Relaxed),
        "indexing": indexing,
        "index_progress": state.index_progress.load(std::sync::atomic::Ordering::Relaxed),
        "index_total": state.index_total.load(std::sync::atomic::Ordering::Relaxed),
    });

    json_rpc_result(id, result)
}

fn handle_reload(id: Option<serde_json::Value>, state: &ServerState) -> String {
    let index_dir = builder::default_index_dir(&state.root);

    // Rebuild from disk
    if let Err(e) = builder::build_index(&state.root, Some(&index_dir), false, &state.exclude_dirs)
    {
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

fn start_file_watcher(state: Arc<ServerState>, root: &Path) -> Option<RecommendedWatcher> {
    let root_path = root.to_path_buf();
    let state_clone = Arc::clone(&state);

    let mut watcher = match notify::recommended_watcher(
        move |result: std::result::Result<Event, notify::Error>| {
            if let Ok(event) = result {
                handle_fs_event(&state_clone, &root_path, &event);
            }
        },
    ) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[trace] warning: failed to start file watcher: {e}");
            return None;
        }
    };

    if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
        eprintln!("[trace] warning: failed to watch directory: {e}");
        return None;
    }

    state
        .watcher_active
        .store(true, std::sync::atomic::Ordering::Relaxed);
    eprintln!("[trace] file watcher started");

    Some(watcher)
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
    gitignore: Option<&tgrep_core::gitignore::Gitignore>,
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
        // Ancestor (i.e. not the last segment) — apply exclude_dirs.
        if segments.peek().is_some()
            && !exclude_dirs.is_empty()
            && exclude_dirs.iter().any(|d| d == seg)
        {
            return true;
        }
    }

    // Gitignore check (if a matcher is available).
    if let Some(gi) = gitignore {
        // We don't know whether the path is a dir or a file here — for
        // the watcher's purposes we treat all events as "file" matches.
        // Notify usually fires per-file events anyway, and gitignore
        // rules that target dirs would have already skipped the dir's
        // contents via `matched_path_or_any_parents`.
        let m = gi.matched_path_or_any_parents(rel_path, /* is_dir = */ false);
        if m.is_ignore() {
            return true;
        }
    }

    false
}

fn handle_fs_event(state: &ServerState, root: &Path, event: &Event) {
    use tgrep_core::meta::FileStamp;

    // Skip file events while the initial background index build is in progress —
    // the indexer will pick up all files itself, and the watcher would just
    // cause duplicate reindex work.
    if state.indexing.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }

    let dominated_kinds = matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    );
    if !dominated_kinds {
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
        if should_skip_watcher_path(&rel_path, &state.exclude_dirs, state.gitignore.as_ref()) {
            continue;
        }

        let is_remove = matches!(event.kind, EventKind::Remove(_)) || !path.exists();

        if is_remove {
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

        if !path.is_file() {
            continue;
        }

        // Compute the file's current stamp and skip if it matches what we
        // last indexed. notify on Windows in particular fires Modify events
        // for atime/attribute updates, opens, etc. — re-indexing on those
        // would re-read large files, churn the live overlay, and produce a
        // misleading "modified" trace for files that didn't actually change.
        let current = match std::fs::metadata(path) {
            Ok(m) => FileStamp {
                mtime: m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                size: m.len(),
            },
            Err(_) => continue,
        };
        if state.file_stamps.read().unwrap().get(&rel_path) == Some(&current) {
            continue;
        }

        // Read contents and extract trigrams OUTSIDE the index write lock
        // so a concurrent search (which needs a read lock) is not blocked
        // on our file I/O and trigram parsing. Windows' SRWLock is
        // writer-preferring: a single waiting writer here would otherwise
        // stall every subsequent search request.
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let is_binary = tgrep_core::trigram::is_binary(&data);
        let per_tri = if is_binary {
            None
        } else {
            Some(tgrep_core::live::LiveIndex::compute_trigram_masks(&data))
        };

        eprintln!("[trace] reindex: modified {rel_path}");
        // gate acquired at the function level — the commit + stamp
        // update is processed atomically with respect to flush/auto-save.
        {
            let mut index = state.index.write().unwrap();
            match per_tri {
                Some(per_tri) => index.live.commit_upsert(&rel_path, per_tri),
                None => index.live.delete_file(&rel_path),
            }
        }
        state
            .file_stamps
            .write()
            .unwrap()
            .insert(rel_path.clone(), current);
        if let Ok(mut cache) = state.cache.write() {
            cache.pop(&rel_path);
        }
    }
}

fn auto_save_loop(state: Arc<ServerState>, index_dir: &Path) {
    let mut last_save = Instant::now();

    loop {
        thread::sleep(Duration::from_secs(60));

        // Don't auto-save while background indexing or a bulk flush is
        // active — those paths handle their own publication and an
        // auto-save fired in parallel would just snapshot the same
        // overlay redundantly.
        if state.indexing.load(std::sync::atomic::Ordering::Relaxed)
            || state.flushing.load(std::sync::atomic::Ordering::Relaxed)
        {
            continue;
        }

        let dirty = {
            let index = state.index.read().unwrap();
            index.live.dirty_count()
        };

        let elapsed = last_save.elapsed();
        if dirty >= AUTO_SAVE_MUTATIONS || (dirty > 0 && elapsed >= AUTO_SAVE_INTERVAL) {
            let save_start = Instant::now();
            eprintln!("[trace] auto-save: {dirty} mutations, saving...");

            // Hold the snapshot gate in write mode for the entire
            // snapshot → publish → prune cycle so watcher mutations
            // can't race the prune (see flush_index_to_disk for the
            // same pattern + rationale).
            let _gate = state.snapshot_gate.write().unwrap();

            // Snapshot reader + overlay under brief read lock
            let (paths, inverted) = {
                let index = state.index.read().unwrap();
                index.full_snapshot()
            };
            let staging_dir = index_dir.with_file_name(".tgrep_save_staging");
            // Clear any stale files from a previously crashed auto-save —
            // otherwise leftover artifacts (e.g. an old filestamps.json)
            // would be picked up by move_staged_files and published.
            let _ = std::fs::remove_dir_all(&staging_dir);
            if let Err(e) = builder::write_index_from_snapshot(
                &state.root,
                &staging_dir,
                &paths,
                &inverted,
                true,
            ) {
                eprintln!("[trace] auto-save failed: {e}");
                let _ = std::fs::remove_dir_all(&staging_dir);
                continue;
            }

            // Lock-free publish: rename staging files into place, build a
            // new IndexReader, then swap it in via a brief read lock. Search
            // queries are NOT blocked: they continue to be served by the
            // previous reader (whose mmap stays valid until the last
            // in-flight Arc<IndexReader> is dropped) and by the live overlay
            // throughout the entire publish.
            //
            // Held across move + open + swap so concurrent publishers
            // (checkpoint / flush) cannot interleave renames or swap
            // readers out of order. Searches do not take this lock.
            let _publish = state.publish_lock.lock().unwrap();
            let num_files = paths.len();
            if let Err(e) = move_staged_files(&staging_dir, index_dir) {
                eprintln!("[trace] auto-save move failed: {e}");
                let _ = std::fs::remove_dir_all(&staging_dir);
                continue;
            }
            match tgrep_core::reader::IndexReader::open(index_dir) {
                Ok(new_reader) => {
                    let reader_files = new_reader.num_files();
                    let reader_trigrams = new_reader.num_trigrams();

                    if new_reader.is_degenerate() {
                        eprintln!(
                            "[trace] auto-save: degenerate reader ({reader_files} files, \
                             0 trigrams), keeping live overlay"
                        );
                    } else if let Err(msg) = new_reader.validate_lookup() {
                        eprintln!(
                            "[trace] auto-save: validation failed: {msg}, keeping live overlay"
                        );
                    } else if reader_files >= num_files {
                        // Swap the reader without blocking concurrent searches.
                        state.index.read().unwrap().swap_reader(new_reader);
                        // Brief write lock for in-memory overlay prune + dirty reset.
                        {
                            let mut index = state.index.write().unwrap();
                            index.prune_persisted_entries();
                            index.live.reset_dirty_count();
                        }
                        last_save = Instant::now();
                        eprintln!(
                            "[trace] auto-save complete in {:.1}s ({reader_files} files, \
                             {reader_trigrams} trigrams on disk)",
                            save_start.elapsed().as_secs_f64(),
                        );
                    } else {
                        eprintln!(
                            "[trace] auto-save reopen incomplete: expected {num_files} files, found {reader_files}; live overlay retained"
                        );
                    }
                }
                Err(e) => {
                    eprintln!("[trace] auto-save reopen failed: {e}, live overlay retained");
                }
            }
            let _ = std::fs::remove_dir_all(&staging_dir);
        }
    }
}

fn simple_glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace('.', r"\.");
    let pattern = pattern.replace("**", "§§");
    let pattern = pattern.replace('*', "[^/]*");
    let pattern = pattern.replace("§§", ".*");
    regex::Regex::new(&format!("(?i){pattern}$"))
        .map(|re| re.is_match(path))
        .unwrap_or(false)
}

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
fn background_refresh_stale(state: &Arc<ServerState>, root: &Path, index_dir: &Path) {
    use tgrep_core::meta::{self, FileStamp};
    use tgrep_core::walker;

    let start = Instant::now();
    eprintln!("[trace] stale check: comparing index against filesystem...");

    // Load stored per-file stamps from last index write
    let old_stamps = match meta::read_filestamps(index_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[trace] stale check: no filestamps found ({e}), skipping");
            return;
        }
    };
    if old_stamps.is_empty() {
        eprintln!("[trace] stale check: no filestamps found, skipping");
        return;
    }

    // Walk filesystem metadata (no content reads)
    let current_meta = walker::walk_file_metadata(root, &state.exclude_dirs);
    let walk_ms = start.elapsed().as_millis();

    // Build lookup of current filesystem state
    let mut current_set: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(current_meta.len());
    let mut changed: Vec<String> = Vec::new();
    let mut added: Vec<String> = Vec::new();

    for fm in &current_meta {
        current_set.insert(fm.relative_path.clone());
        let stamp = FileStamp {
            mtime: fm.mtime,
            size: fm.size,
        };
        match old_stamps.get(&fm.relative_path) {
            Some(old) if *old == stamp => {
                // Unchanged — skip
            }
            Some(_) => {
                // mtime or size differs — file changed
                changed.push(fm.relative_path.clone());
            }
            None => {
                // New file (not in previous index)
                added.push(fm.relative_path.clone());
            }
        }
    }

    // Detect deleted files (in old stamps but not on filesystem)
    let deleted: Vec<String> = old_stamps
        .keys()
        .filter(|p| !current_set.contains(p.as_str()))
        .cloned()
        .collect();

    let total_changes = changed.len() + added.len() + deleted.len();
    if total_changes == 0 {
        eprintln!(
            "[trace] stale check: index is up-to-date ({} files checked in {}ms)",
            current_meta.len(),
            walk_ms
        );
        return;
    }

    eprintln!(
        "[trace] stale check: {} changed, {} new, {} deleted (walk: {}ms)",
        changed.len(),
        added.len(),
        deleted.len(),
        walk_ms
    );

    // Apply changes to the LiveIndex. Hold snapshot_gate.read() across the
    // mutation block so a concurrent flush/auto-save cannot snapshot the
    // overlay and then prune away these updates after publishing. The
    // subsequent flush_index_to_disk call below takes the gate exclusively
    // itself.
    let update_start = Instant::now();
    let files_to_update: Vec<String> = changed.into_iter().chain(added).collect();

    {
        let _gate = state.snapshot_gate.read().unwrap();
        let mut index = state.index.write().unwrap();

        // Remove deleted files
        for rel_path in &deleted {
            index.live.delete_file(rel_path);
        }

        // Upsert changed/new files (reads content, extracts trigrams)
        for rel_path in &files_to_update {
            index.live.update_from_disk(root, rel_path);
        }
    }

    // Invalidate cache entries for all affected files
    {
        if let Ok(mut cache) = state.cache.write() {
            for rel_path in deleted.iter().chain(files_to_update.iter()) {
                cache.pop(rel_path);
            }
        }
    }

    eprintln!(
        "[trace] stale check: updated {} files in {:.1}ms (total: {:.1}ms)",
        total_changes,
        update_start.elapsed().as_secs_f64() * 1000.0,
        start.elapsed().as_secs_f64() * 1000.0
    );

    // Persist the updated index immediately so changes survive a crash.
    // Pass the freshly-walked stamps so they publish atomically with the
    // index files.
    eprintln!("[trace] stale check: flushing updated index to disk...");
    let new_stamps: std::collections::HashMap<String, FileStamp> = current_meta
        .iter()
        .map(|fm| {
            (
                fm.relative_path.clone(),
                FileStamp {
                    mtime: fm.mtime,
                    size: fm.size,
                },
            )
        })
        .collect();
    state
        .flushing
        .store(true, std::sync::atomic::Ordering::Relaxed);
    flush_index_to_disk(state, root, index_dir, Some(&new_stamps));
    state
        .flushing
        .store(false, std::sync::atomic::Ordering::Relaxed);

    // Refresh in-memory stamps so the watcher can dedupe spurious notify
    // events for files that already match what we just published.
    *state.file_stamps.write().unwrap() = new_stamps;
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

    // Phase 1: Walk file paths (no content reads)
    let t_walk = Instant::now();
    let walk = walker::walk_dir(
        root,
        &WalkOptions {
            include_hidden: false,
            exclude_dirs: state.exclude_dirs.clone(),
            ..Default::default()
        },
    );

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
        "[trace] walk complete: {} new files to index ({} already indexed, {} binary skipped, {} errors) in {:.1}ms",
        new_count,
        seeded_count,
        walk.skipped_binary,
        walk.skipped_error,
        t_walk.elapsed().as_secs_f64() * 1000.0
    );

    // Phase 2: Process new files in parallel batches
    for (batch_idx, batch) in new_files.chunks(BATCH_SIZE).enumerate() {
        // Parallel: read files + extract trigrams (no locks held)
        let batch_results: Vec<(String, Vec<u32>)> = batch
            .par_iter()
            .filter_map(|path| {
                let data = std::fs::read(path).ok()?;
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
                if lower != data {
                    trigrams.extend(tgrep_core::trigram::extract(&lower));
                }
                Some((rel_path, trigrams))
            })
            .collect();

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
    }

    eprintln!(
        "[trace] background indexing complete: {} total files ({} new, {} seeded) in {:.1}s",
        total,
        new_count,
        seeded_count,
        start.elapsed().as_secs_f64()
    );

    // Walk filesystem metadata BEFORE the flush so we can publish the
    // resulting per-file stamps atomically with the index files. Writing
    // them after a successful flush would leave a multi-minute window where
    // the index looks fully published but `filestamps.json` is missing — a
    // server kill in that window disables incremental stale detection on
    // the next start.
    let walk_meta = tgrep_core::walker::walk_file_metadata(root, &state.exclude_dirs);
    let stamps: std::collections::HashMap<String, tgrep_core::meta::FileStamp> = walk_meta
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
    state
        .flushing
        .store(true, std::sync::atomic::Ordering::Relaxed);
    state
        .indexing
        .store(false, std::sync::atomic::Ordering::Relaxed);

    // Final (and only) flush to disk for the bulk build.
    eprintln!("[trace] persisting final index to disk...");
    let pruned = flush_index_to_disk(state, root, index_dir, Some(&stamps));
    state
        .flushing
        .store(false, std::sync::atomic::Ordering::Relaxed);

    // Refresh the in-memory file_stamps so the file watcher can recognize
    // unchanged files and skip spurious notify events (e.g. atime/attribute
    // updates on Windows). Done even if the flush failed — the live overlay
    // already reflects what we just indexed, and the stamps describe that.
    *state.file_stamps.write().unwrap() = stamps;

    // Reclaim memory held by the indexing-time live overlay — but only when
    // the flush actually completed and `prune_persisted_entries` ran. If the
    // flush failed, the overlay is still the source of truth and shrinking
    // the indexing-sized maps would just waste the write lock with no benefit.
    if pruned {
        let mut index = state.index.write().unwrap();
        index.live.shrink_to_fit();
    }
}

/// Flush the current LiveIndex to disk and reopen the reader.
/// Uses fast clone + background remap so queries stay responsive.
///
/// If `stamps` is `Some`, the function attempts to write per-file stamps
/// into the staging directory so they are renamed into the index dir
/// alongside the index files. Stamp persistence is best-effort: if writing
/// `filestamps.json` fails we log and still publish the index, since
/// losing the incremental stale-check on next start is preferable to
/// dropping the freshly-built index. Callers must therefore not rely on
/// `filestamps.json` always being published alongside a newly-flushed
/// index.
///
/// Returns `true` if the new reader was opened and `prune_persisted_entries`
/// ran on the live overlay; `false` on any earlier failure (write/move/open
/// error or partial reader). Callers can use this to decide whether
/// follow-up work that depends on the prune (e.g. shrinking the overlay)
/// is worth doing.
fn flush_index_to_disk(
    state: &ServerState,
    _root: &Path,
    index_dir: &Path,
    stamps: Option<&std::collections::HashMap<String, tgrep_core::meta::FileStamp>>,
) -> bool {
    let flush_start = Instant::now();

    // Hold the snapshot gate in write mode for the entire snapshot →
    // publish → prune cycle. Watcher mutations block on this for the
    // duration; without it, an event that fires after the snapshot but
    // before the prune would be silently dropped (snapshot doesn't see
    // it, reader is reopened with the old version, then the prune
    // removes the overlay entry by path because it now matches a reader
    // entry). Searches do not take this lock and remain unaffected.
    let _gate = state.snapshot_gate.write().unwrap();

    // Snapshot under brief read lock — use full_snapshot to merge reader + overlay
    let (paths, inverted, num_files) = {
        let index = state.index.read().unwrap();
        let (paths, inverted) = index.full_snapshot();
        let num = paths.len();
        eprintln!(
            "[trace] flush: snapshotted {num} files in {:.1}ms",
            flush_start.elapsed().as_secs_f64() * 1000.0
        );
        (paths, inverted, num)
    };

    // Always start from a clean staging dir. A previous crash (or a partial
    // earlier flush whose error path missed the cleanup) could have left
    // stale files behind — including a stale `filestamps.json`. If the
    // current flush is invoked with `stamps == None` and we left an old
    // filestamps.json there, `move_staged_files` would publish it next to
    // a freshly-built index, silently corrupting the stale-check baseline.
    let staging_dir = index_dir.with_file_name(".tgrep_flush_staging");
    let _ = std::fs::remove_dir_all(&staging_dir);

    // Expensive write — no lock held, queries served normally
    if let Err(e) =
        builder::write_index_from_snapshot(&state.root, &staging_dir, &paths, &inverted, true)
    {
        eprintln!("[trace] warning: flush to disk failed: {e}");
        let _ = std::fs::remove_dir_all(&staging_dir);
        return false;
    }

    // Stage filestamps alongside the index files so the subsequent move
    // publishes them atomically. If this fails we still try to publish the
    // index — losing only the incremental stale-check benefit on next start,
    // not the index itself.
    if let Some(stamps) = stamps
        && let Err(e) = tgrep_core::meta::write_filestamps(stamps, &staging_dir)
    {
        eprintln!("[trace] warning: failed to write staging filestamps: {e}");
    }

    // Lock-free publish: rename staging files into place, build new reader,
    // then swap. Search queries continue to be served throughout.
    //
    // Held across move + open + swap so concurrent publishers (auto-save /
    // background-build / watcher reindex flush) cannot interleave renames
    // or swap readers out of order. Searches do not take this lock.
    let _publish = state.publish_lock.lock().unwrap();
    if let Err(e) = move_staged_files(&staging_dir, index_dir) {
        eprintln!("[trace] warning: flush move failed: {e}");
        let _ = std::fs::remove_dir_all(&staging_dir);
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
    let _ = std::fs::remove_dir_all(&staging_dir);

    eprintln!(
        "[trace] index flushed: {num_files} files on disk in {:.1}s",
        flush_start.elapsed().as_secs_f64()
    );
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
