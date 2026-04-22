//! Tests for reader snapshot consistency in HybridIndex.
//!
//! Verifies that query execution and path resolution use the same reader
//! snapshot, preventing a race condition where a concurrent `swap_reader`
//! could cause all file_path lookups to fail.

use std::collections::HashMap;
use std::path::Path;
use tgrep_core::PostingEntry;
use tgrep_core::hybrid::HybridIndex;
use tgrep_core::{builder, query, trigram};

/// Build a test index on disk from a set of file contents.
/// Returns the temp directory path (caller owns cleanup).
fn build_test_index(root: &Path, files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    let index_dir = tmp.path();

    let mut paths: Vec<String> = Vec::new();
    let mut inverted: HashMap<u32, Vec<PostingEntry>> = HashMap::new();

    for (i, (rel_path, content)) in files.iter().enumerate() {
        paths.push(rel_path.to_string());

        // Extract trigrams with masks
        let tri_masks = trigram::extract_with_masks(content);
        let mut per_tri: HashMap<u32, trigram::TrigramMasks> = HashMap::new();
        for &(tri, m) in &tri_masks {
            let entry = per_tri.entry(tri).or_default();
            entry.loc_mask |= m.loc_mask;
            entry.next_mask |= m.next_mask;
        }

        for (tri, m) in per_tri {
            inverted.entry(tri).or_default().push(PostingEntry {
                file_id: i as u32,
                loc_mask: m.loc_mask,
                next_mask: m.next_mask,
            });
        }
    }

    builder::write_index_from_snapshot(root, index_dir, &paths, &inverted, true).unwrap();
    tmp
}

/// Every file ID returned by execute_query_with_masks must resolve via the
/// returned reader snapshot — even without any concurrent swap.
#[test]
fn all_query_ids_resolve_via_snapshot() {
    let root = tempfile::TempDir::new().unwrap();
    let files: Vec<(&str, &[u8])> = vec![
        ("src/main.rs", b"fn main() { println!(\"hello\"); }"),
        (
            "src/lib.rs",
            b"pub fn hello() -> &'static str { \"hello\" }",
        ),
        (
            "src/util.rs",
            b"pub fn format_hello(name: &str) -> String { format!(\"hello {}\", name) }",
        ),
    ];

    // Create the actual files on disk (for root)
    for (path, content) in &files {
        let full = root.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, content).unwrap();
    }

    let index_dir = build_test_index(root.path(), &files);
    let hybrid = HybridIndex::open(index_dir.path(), root.path()).unwrap();

    // Query for "hello" which appears in all files
    let plan = query::build_query_plan("hello", false).unwrap();
    let (file_ids, reader_snapshot) = hybrid.execute_query_with_masks(&plan);

    assert!(!file_ids.is_empty(), "expected candidates for 'hello'");

    // Every returned ID must resolve to a valid path
    for &fid in &file_ids {
        let path = hybrid.resolve_path(fid, &reader_snapshot);
        assert!(
            path.is_some(),
            "file_id {fid} should resolve to a path via the reader snapshot"
        );
        let full = hybrid.resolve_full_path(fid, &reader_snapshot);
        assert!(
            full.is_some(),
            "file_id {fid} should resolve to a full path via the reader snapshot"
        );
    }
}

/// After swapping the reader to a completely different index, the OLD file IDs
/// should fail to resolve via the NEW reader — but should still resolve via
/// the snapshot returned from execute_query_with_masks.
#[test]
fn snapshot_survives_reader_swap() {
    let root = tempfile::TempDir::new().unwrap();
    let files_v1: Vec<(&str, &[u8])> = vec![
        ("alpha.rs", b"fn alpha() { println!(\"function_alpha\"); }"),
        ("beta.rs", b"fn beta() { println!(\"function_beta\"); }"),
        ("gamma.rs", b"fn gamma() { println!(\"function_gamma\"); }"),
    ];

    for (path, content) in &files_v1 {
        let full = root.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, content).unwrap();
    }

    let index_v1 = build_test_index(root.path(), &files_v1);
    let hybrid = HybridIndex::open(index_v1.path(), root.path()).unwrap();

    // Query to get file IDs from reader v1
    let plan = query::build_query_plan("function", false).unwrap();
    let (file_ids_v1, snapshot_v1) = hybrid.execute_query_with_masks(&plan);
    assert!(
        !file_ids_v1.is_empty(),
        "expected candidates for 'function' in v1"
    );

    // Verify all IDs resolve with the v1 snapshot
    for &fid in &file_ids_v1 {
        assert!(
            hybrid.resolve_path(fid, &snapshot_v1).is_some(),
            "fid {fid} should resolve with v1 snapshot"
        );
    }

    // Build a DIFFERENT index (v2) with completely different files
    let files_v2: Vec<(&str, &[u8])> = vec![
        ("one.py", b"def function_one(): pass"),
        ("two.py", b"def function_two(): pass"),
    ];
    for (path, content) in &files_v2 {
        let full = root.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, content).unwrap();
    }

    let index_v2 = build_test_index(root.path(), &files_v2);
    let reader_v2 = tgrep_core::reader::IndexReader::open(index_v2.path()).unwrap();

    // Swap the reader to v2 — simulates a concurrent flush
    hybrid.swap_reader(reader_v2);

    // The v1 snapshot should STILL resolve the old file IDs
    for &fid in &file_ids_v1 {
        let path = hybrid.resolve_path(fid, &snapshot_v1);
        assert!(
            path.is_some(),
            "fid {fid} should STILL resolve with v1 snapshot after swap"
        );
    }

    // But the OLD file_path method (which gets a fresh reader) may fail
    // because the v2 reader has fewer files (2 vs 3)
    let mut failures = 0;
    for &fid in &file_ids_v1 {
        if hybrid.file_path(fid).is_none() {
            failures += 1;
        }
    }
    // At least some IDs should fail (v1 had 3 files, v2 has 2)
    // fid=2 is out of range for v2's file_paths
    assert!(
        failures > 0 || file_ids_v1.iter().all(|&fid| fid < 2),
        "expected some file_path failures after swap to smaller reader, \
         unless all IDs happen to be < 2 (v2 file count)"
    );
}

/// After swap, a new query against the new reader should produce IDs
/// that resolve correctly — no stale state.
#[test]
fn new_query_after_swap_resolves_correctly() {
    let root = tempfile::TempDir::new().unwrap();
    let files_v1: Vec<(&str, &[u8])> = vec![("a.rs", b"fn search_target() {}")];
    for (path, content) in &files_v1 {
        let full = root.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, content).unwrap();
    }

    let index_v1 = build_test_index(root.path(), &files_v1);
    let hybrid = HybridIndex::open(index_v1.path(), root.path()).unwrap();

    // Build v2 with different content
    let files_v2: Vec<(&str, &[u8])> = vec![
        ("x.rs", b"fn search_target() { /* v2 */ }"),
        ("y.rs", b"fn search_target() { /* also v2 */ }"),
    ];
    for (path, content) in &files_v2 {
        let full = root.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, content).unwrap();
    }

    let index_v2 = build_test_index(root.path(), &files_v2);
    let reader_v2 = tgrep_core::reader::IndexReader::open(index_v2.path()).unwrap();
    hybrid.swap_reader(reader_v2);

    // Query after swap should use the new reader
    let plan = query::build_query_plan("search_target", false).unwrap();
    let (file_ids, snapshot) = hybrid.execute_query_with_masks(&plan);

    assert_eq!(file_ids.len(), 2, "v2 has 2 files with search_target");
    for &fid in &file_ids {
        let path = hybrid.resolve_path(fid, &snapshot);
        assert!(path.is_some(), "v2 IDs should resolve with v2 snapshot");
    }
}

/// MatchAll plan should also return a consistent reader snapshot.
#[test]
fn match_all_uses_consistent_snapshot() {
    let root = tempfile::TempDir::new().unwrap();
    let files: Vec<(&str, &[u8])> = vec![("a.txt", b"hello world"), ("b.txt", b"goodbye world")];
    for (path, content) in &files {
        let full = root.path().join(path);
        std::fs::write(&full, content).unwrap();
    }

    let index_dir = build_test_index(root.path(), &files);
    let hybrid = HybridIndex::open(index_dir.path(), root.path()).unwrap();

    // A very short pattern that produces MatchAll
    let plan = query::build_query_plan(".", false).unwrap();
    assert!(plan.is_match_all(), "single-char regex should be MatchAll");

    let (file_ids, snapshot) = hybrid.execute_query_with_masks(&plan);
    assert_eq!(file_ids.len(), 2);
    for &fid in &file_ids {
        assert!(
            hybrid.resolve_path(fid, &snapshot).is_some(),
            "MatchAll IDs should resolve with snapshot"
        );
    }
}

/// Concurrent reader swap during query execution: demonstrates that the
/// snapshot-based API is safe while the old API would fail.
#[test]
fn concurrent_swap_during_query() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let root = tempfile::TempDir::new().unwrap();

    // v1: many files to slow down query
    let mut files_v1: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..50 {
        let path = format!("file_{i}.rs");
        let content = format!("fn target_function_{i}() {{ /* content */ }}");
        let full = root.path().join(&path);
        std::fs::write(&full, content.as_bytes()).unwrap();
        files_v1.push((path, content.into_bytes()));
    }

    let files_refs: Vec<(&str, &[u8])> = files_v1
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_slice()))
        .collect();
    let index_v1 = build_test_index(root.path(), &files_refs);
    let hybrid = Arc::new(HybridIndex::open(index_v1.path(), root.path()).unwrap());

    // v2: completely different files
    let mut files_v2: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..10 {
        let path = format!("other_{i}.py");
        let content = format!("def other_function_{i}(): pass");
        let full = root.path().join(&path);
        std::fs::write(&full, content.as_bytes()).unwrap();
        files_v2.push((path, content.into_bytes()));
    }
    let files_refs_v2: Vec<(&str, &[u8])> = files_v2
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_slice()))
        .collect();
    let index_v2 = build_test_index(root.path(), &files_refs_v2);

    let barrier = Arc::new(Barrier::new(2));
    let hybrid_clone = Arc::clone(&hybrid);
    let barrier_clone = Arc::clone(&barrier);

    // Thread 1: query and resolve paths using snapshot
    let query_thread = thread::spawn(move || {
        let plan = query::build_query_plan("target_function", false).unwrap();
        let (file_ids, snapshot) = hybrid_clone.execute_query_with_masks(&plan);

        // Signal the swap thread to proceed
        barrier_clone.wait();
        // Brief delay to increase chance of swap happening during resolution
        thread::sleep(std::time::Duration::from_millis(10));

        // Resolve using the snapshot — should always work
        let mut resolved = 0;
        for &fid in &file_ids {
            if hybrid_clone.resolve_path(fid, &snapshot).is_some() {
                resolved += 1;
            }
        }
        (file_ids.len(), resolved)
    });

    // Thread 2: swap the reader after query completes
    let swap_thread = thread::spawn(move || {
        barrier.wait();
        let reader_v2 = tgrep_core::reader::IndexReader::open(index_v2.path()).unwrap();
        hybrid.swap_reader(reader_v2);
    });

    let (total, resolved) = query_thread.join().unwrap();
    swap_thread.join().unwrap();

    // With the snapshot-based API, ALL IDs should resolve
    assert_eq!(
        resolved, total,
        "snapshot-based resolution should succeed for all {total} IDs, got {resolved}"
    );
}
