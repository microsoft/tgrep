/// `tgrep serve` — TCP JSON-RPC server with file watcher.
///
/// Keeps the trigram index in memory (HybridIndex), watches for filesystem
/// changes, and serves search/status queries over TCP.
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use lru::LruCache;

use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use regex::RegexBuilder;
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

struct ServerState {
    index: RwLock<HybridIndex>,
    cache: RwLock<LruCache<String, Arc<String>>>,
    root: PathBuf,
    watcher_active: std::sync::atomic::AtomicBool,
    /// True while the initial index build is in progress.
    indexing: std::sync::atomic::AtomicBool,
    /// Progress: number of files indexed so far.
    index_progress: std::sync::atomic::AtomicU64,
    /// Total files discovered for indexing.
    index_total: std::sync::atomic::AtomicU64,
}

struct SearchOpts {
    files_only: bool,
    invert_match: bool,
    only_matching: bool,
    max_count: Option<usize>,
    before_context: usize,
    after_context: usize,
}

pub fn run(root: &Path, index_path: Option<&Path>, no_watch: bool) -> Result<()> {
    let serve_start = Instant::now();
    let root = std::fs::canonicalize(root)?;
    let index_dir = index_path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| builder::default_index_dir(&root));

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
        index_progress: std::sync::atomic::AtomicU64::new(0),
        index_total: std::sync::atomic::AtomicU64::new(0),
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

    let combined = if all_patterns.len() == 1 {
        let mut p = if fixed_string {
            regex::escape(&all_patterns[0])
        } else {
            all_patterns[0].clone()
        };
        if word_boundary {
            p = format!(r"\b(?:{p})\b");
        }
        p
    } else {
        let parts: Vec<String> = all_patterns
            .iter()
            .map(|p| {
                let mut p = if fixed_string {
                    regex::escape(p)
                } else {
                    p.clone()
                };
                if word_boundary {
                    p = format!(r"\b(?:{p})\b");
                }
                p
            })
            .collect();
        format!("(?:{})", parts.join("|"))
    };

    let re = match RegexBuilder::new(&combined)
        .case_insensitive(case_insensitive)
        .multi_line(multiline)
        .dot_matches_new_line(multiline)
        .build()
    {
        Ok(r) => r,
        Err(e) => return json_rpc_error(id, -32602, &format!("regex error: {e}")),
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
    let candidate_info: Vec<(String, PathBuf)> = {
        let index = state.index.read().unwrap();
        let candidates = index.execute_query(&plan);

        candidates
            .iter()
            .filter_map(|&fid| {
                let rel_path = index.file_path(fid)?.to_string();
                if let Some(ref type_name) = file_type
                    && !tgrep_core::filetypes::matches_type(&rel_path, type_name)
                {
                    return None;
                }
                if !glob_filters.is_empty()
                    && !glob_filters.iter().any(|g| simple_glob_match(g, &rel_path))
                {
                    return None;
                }
                let full_path = index.full_path(fid)?;
                Some((rel_path, full_path))
            })
            .collect()
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
        "[trace] search: pattern={:?} case_insensitive={} candidates={} matches={} elapsed={:.1}ms (index={:.1}ms resolve={:.1}ms search={:.1}ms)",
        pattern,
        case_insensitive,
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
    if let Err(e) = builder::build_index(&state.root, Some(&index_dir), false) {
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

fn handle_fs_event(state: &ServerState, root: &Path, event: &Event) {
    let dominated_kinds = matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    );
    if !dominated_kinds {
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

        let mut index = state.index.write().unwrap();

        if matches!(event.kind, EventKind::Remove(_)) || !path.exists() {
            eprintln!("[trace] reindex: removed {rel_path}");
            index.live.delete_file(&rel_path);
        } else if path.is_file() {
            eprintln!("[trace] reindex: modified {rel_path}");
            index.live.update_from_disk(root, &rel_path);
        }

        // Invalidate cache for this path
        if let Ok(mut cache) = state.cache.write() {
            cache.pop(&rel_path);
        }
    }
}

fn auto_save_loop(state: Arc<ServerState>, index_dir: &Path) {
    let mut last_save = Instant::now();

    loop {
        thread::sleep(Duration::from_secs(60));

        // Don't auto-save while background indexing is active — it handles its own flushes
        if state.indexing.load(std::sync::atomic::Ordering::Relaxed) {
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

            // Snapshot reader + overlay under brief read lock
            let (paths, inverted) = {
                let index = state.index.read().unwrap();
                index.full_snapshot()
            };
            let staging_dir = index_dir.with_file_name(".tgrep_save_staging");
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

            // Brief write lock: swap files and reopen
            {
                let mut index = state.index.write().unwrap();
                index.drop_reader();
                if let Err(e) = move_staged_files(&staging_dir, index_dir) {
                    eprintln!("[trace] auto-save move failed: {e}");
                    let _ = std::fs::remove_dir_all(&staging_dir);
                    continue;
                }
                match HybridIndex::open(index_dir, &state.root) {
                    Ok(new_index) => {
                        *index = new_index;
                        last_save = Instant::now();
                        eprintln!(
                            "[trace] auto-save complete in {:.1}s",
                            save_start.elapsed().as_secs_f64()
                        );
                    }
                    Err(e) => {
                        eprintln!("[trace] auto-save reopen failed: {e}");
                    }
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
    let meta = IndexMeta::new("", 0, 0);
    meta.save(index_dir)?;
    Ok(())
}

/// Walk the repo and populate the LiveIndex in batches in a background thread.
/// Uses rayon for parallel trigram extraction. Flushes to disk every
/// FLUSH_INTERVAL or FLUSH_FILE_THRESHOLD, whichever comes first.
fn background_index_build(state: &Arc<ServerState>, root: &Path, index_dir: &Path) {
    use rayon::prelude::*;
    use tgrep_core::walker::{self, WalkOptions};

    const BATCH_SIZE: usize = 500;
    const FLUSH_FILE_THRESHOLD: usize = 100_000;
    const FLUSH_INTERVAL: Duration = Duration::from_secs(300); // 5 minutes

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

    let mut files_since_flush: usize = 0;
    let mut last_flush = Instant::now();
    let checkpoint_active = Arc::new(std::sync::atomic::AtomicBool::new(false));

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
        files_since_flush += batch.len();
        state
            .index_progress
            .store(progress as u64, std::sync::atomic::Ordering::Relaxed);

        if progress % 5000 < BATCH_SIZE {
            eprintln!(
                "[trace] indexing progress: ~{progress}/{total} files ({:.1}s elapsed)",
                start.elapsed().as_secs_f64()
            );
        }

        // Periodic flush to disk (checkpoint only — don't reopen, keep accumulating in LiveIndex)
        if files_since_flush >= FLUSH_FILE_THRESHOLD || last_flush.elapsed() >= FLUSH_INTERVAL {
            eprintln!(
                "[trace] flushing index to disk ({files_since_flush} files since last flush)..."
            );
            checkpoint_index_to_disk(state, index_dir, &checkpoint_active);
            files_since_flush = 0;
            last_flush = Instant::now();
        }
    }

    eprintln!(
        "[trace] background indexing complete: {} total files ({} new, {} seeded) in {:.1}s",
        total,
        new_count,
        seeded_count,
        start.elapsed().as_secs_f64()
    );

    // Wait for any active checkpoint thread to finish before final flush
    while checkpoint_active.load(std::sync::atomic::Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }

    // Final flush to disk
    eprintln!("[trace] persisting final index to disk...");
    flush_index_to_disk(state, root, index_dir);

    // Clear indexing flag AFTER final flush so auto-save doesn't race
    state
        .indexing
        .store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Checkpoint: write LiveIndex to disk for crash recovery, but do NOT reopen.
/// Clones raw data under a brief read lock, then spawns a background thread
/// for the expensive ID remapping and disk I/O — indexing continues unblocked.
fn checkpoint_index_to_disk(
    state: &Arc<ServerState>,
    index_dir: &Path,
    checkpoint_active: &Arc<std::sync::atomic::AtomicBool>,
) {
    // Skip if a previous checkpoint is still running
    if checkpoint_active.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("[trace] checkpoint skipped: previous checkpoint still running");
        return;
    }

    let clone_start = Instant::now();

    // Snapshot reader + overlay under read lock for a complete checkpoint
    let (paths, inverted) = {
        let index = state.index.read().unwrap();
        let (paths, inverted) = index.full_snapshot();
        eprintln!(
            "[trace] checkpoint: snapshotted {} files in {:.1}ms (indexing continues)",
            paths.len(),
            clone_start.elapsed().as_secs_f64() * 1000.0
        );
        (paths, inverted)
    };

    // Spawn background thread for the expensive disk write
    checkpoint_active.store(true, std::sync::atomic::Ordering::Relaxed);
    let state = Arc::clone(state);
    let index_dir = index_dir.to_path_buf();
    let active_flag = Arc::clone(checkpoint_active);

    thread::spawn(move || {
        let flush_start = Instant::now();

        let num_files = paths.len();

        // Write to staging directory
        let staging_dir = index_dir.with_file_name(".tgrep_staging");
        if let Err(e) =
            builder::write_index_from_snapshot(&state.root, &staging_dir, &paths, &inverted, false)
        {
            eprintln!("[trace] warning: checkpoint flush failed: {e}");
            let _ = std::fs::remove_dir_all(&staging_dir);
            active_flag.store(false, std::sync::atomic::Ordering::Relaxed);
            return;
        }

        // Brief write lock: drop mmap handles, move staged files in
        {
            let mut index = state.index.write().unwrap();
            index.drop_reader();
            if let Err(e) = move_staged_files(&staging_dir, &index_dir) {
                eprintln!("[trace] warning: checkpoint move failed: {e}");
            }
        }
        let _ = std::fs::remove_dir_all(&staging_dir);

        eprintln!(
            "[trace] index checkpoint: {num_files} files written in {:.1}s",
            flush_start.elapsed().as_secs_f64()
        );
        active_flag.store(false, std::sync::atomic::Ordering::Relaxed);
    });
}

/// Flush the current LiveIndex to disk and reopen the reader.
/// Uses fast clone + background remap so queries stay responsive.
fn flush_index_to_disk(state: &ServerState, _root: &Path, index_dir: &Path) {
    let flush_start = Instant::now();

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

    // Expensive write — no lock held, queries served normally
    let staging_dir = index_dir.with_file_name(".tgrep_flush_staging");
    if let Err(e) =
        builder::write_index_from_snapshot(&state.root, &staging_dir, &paths, &inverted, true)
    {
        eprintln!("[trace] warning: flush to disk failed: {e}");
        let _ = std::fs::remove_dir_all(&staging_dir);
        return;
    }

    // Brief write lock: drop reader, move staged files, reopen
    {
        let mut index = state.index.write().unwrap();
        index.drop_reader(); // release mmap handles (Windows)
        if let Err(e) = move_staged_files(&staging_dir, index_dir) {
            eprintln!("[trace] warning: flush move failed: {e}");
            let _ = std::fs::remove_dir_all(&staging_dir);
            return;
        }

        // Reopen the reader from the newly written files
        match HybridIndex::open(index_dir, &state.root) {
            Ok(new_index) => {
                *index = new_index;
            }
            Err(e) => {
                eprintln!("[trace] warning: failed to reopen index after flush: {e}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&staging_dir);

    eprintln!(
        "[trace] index flushed: {num_files} files on disk in {:.1}s",
        flush_start.elapsed().as_secs_f64()
    );
}

/// Move index files from staging to the target directory.
fn move_staged_files(staging: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for name in &["index.bin", "lookup.bin", "files.bin", "meta.json"] {
        let src = staging.join(name);
        let dst = target.join(name);
        if src.exists() {
            // Use copy+remove instead of rename — rename may fail across drives/volumes
            std::fs::copy(&src, &dst)?;
            let _ = std::fs::remove_file(&src);
        }
    }
    Ok(())
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
