use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use tempfile::TempDir;
use tgrep_core::hybrid::HybridIndex;
use tgrep_core::meta::IndexMeta;
use tgrep_core::reader::IndexReader;
use tgrep_core::shared::SharedBase;
use tgrep_core::{PostingEntry, builder, query, trigram};

fn build_base(root: &Path, files: &[(&str, &[u8])]) -> TempDir {
    let directory = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    let mut postings: HashMap<u32, Vec<PostingEntry>> = HashMap::new();
    for (id, &(path, contents)) in files.iter().enumerate() {
        paths.push(path.to_string());
        for (tri, masks) in trigram::extract_merged_masks(contents) {
            postings.entry(tri).or_default().push(PostingEntry {
                file_id: id as u32,
                loc_mask: masks.loc_mask,
                next_mask: masks.next_mask,
            });
        }
    }
    builder::write_index_from_snapshot(root, directory.path(), &paths, &postings, true).unwrap();
    let mut meta = IndexMeta::load(directory.path()).unwrap();
    meta.hidden_complete = true;
    meta.save(directory.path()).unwrap();
    directory
}

fn write(root: &Path, path: &str, contents: &[u8]) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn candidates(index: &HybridIndex, pattern: &str, insensitive: bool) -> Vec<String> {
    let plan = query::build_query_plan(pattern, insensitive).unwrap();
    let (ids, reader) = index.execute_query_with_masks(&plan);
    let mut paths: Vec<_> = ids
        .into_iter()
        .map(|id| index.resolve_path(id, &reader).unwrap())
        .collect();
    paths.sort_unstable();
    paths
}

fn matches(index: &HybridIndex, pattern: &str) -> Vec<String> {
    let matcher = regex::bytes::Regex::new(pattern).unwrap();
    let plan = query::build_query_plan(pattern, false).unwrap();
    let (ids, reader) = index.execute_query_with_masks(&plan);
    let mut paths: Vec<_> = ids
        .into_iter()
        .filter_map(|id| {
            let full_path = index.resolve_full_path(id, &reader).unwrap();
            assert!(full_path.starts_with(&index.root));
            let contents = fs::read(full_path).unwrap();
            matcher
                .is_match(&contents)
                .then(|| index.resolve_path(id, &reader).unwrap())
        })
        .collect();
    paths.sort_unstable();
    paths
}

fn read_checkpoint(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn worktrees_share_reader_but_not_changes_or_roots() {
    let original = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let directory = build_base(
        original.path(),
        &[
            ("src/shared.rs", b"shared unchanged"),
            ("changed.rs", b"original feature"),
            ("deleted.rs", b"original deletion"),
        ],
    );
    let base = SharedBase::open(directory.path()).unwrap();
    let mut a = base.create_worktree(first.path()).unwrap();
    let mut b = base.clone().create_worktree(second.path()).unwrap();
    assert!(Arc::ptr_eq(&a.reader_arc(), &b.reader_arc()));

    for root in [first.path(), second.path()] {
        write(root, "src/shared.rs", b"shared unchanged");
    }
    write(first.path(), "changed.rs", b"alpha_feature");
    write(first.path(), "new.rs", b"alpha_feature new");
    a.live.upsert_file("changed.rs", b"alpha_feature");
    a.live.upsert_file("new.rs", b"alpha_feature new");
    a.live.delete_file("deleted.rs");
    write(second.path(), "changed.rs", b"beta_feature");
    write(second.path(), "deleted.rs", b"original deletion");
    b.live.upsert_file("changed.rs", b"beta_feature");

    assert_eq!(matches(&a, "alpha_feature"), ["changed.rs", "new.rs"]);
    assert!(matches(&b, "alpha_feature").is_empty());
    assert_eq!(matches(&b, "beta_feature"), ["changed.rs"]);
    assert!(matches(&a, "beta_feature").is_empty());
    assert!(matches(&a, "original").is_empty());
    assert_eq!(matches(&b, "original"), ["deleted.rs"]);
    assert_eq!(matches(&a, "shared"), ["src/shared.rs"]);
    assert_eq!(matches(&b, "shared"), ["src/shared.rs"]);
    assert_eq!(a.num_files(), 3);
    assert_eq!(b.num_files(), 3);

    let untouched = base.create_worktree(original.path()).unwrap();
    assert_eq!(
        candidates(&untouched, "original", false),
        ["changed.rs", "deleted.rs"]
    );
}

#[test]
fn checkpoint_preserves_masks_tombstones_empty_files_and_unmasked_postings() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(
        root.path(),
        &[("changed.rs", b"original"), ("deleted.rs", b"original")],
    );
    let base = SharedBase::open(directory.path()).unwrap();
    let mut worktree = base.create_worktree(root.path()).unwrap();
    worktree.live.upsert_file("changed.rs", b"AlphaBeta");
    worktree.live.upsert_file("short.rs", b"x");
    worktree.live.upsert_file("empty.rs", b"");
    worktree
        .live
        .upsert_file_with_trigrams("unmasked.rs", trigram::extract(b"abcdef"));
    worktree.live.delete_file("deleted.rs");
    worktree.live.delete_file("already-absent.rs");
    let checkpoint = root.path().join("overlay.json");
    base.save_overlay(&worktree, &checkpoint).unwrap();

    let restored = base.restore_worktree(root.path(), &checkpoint).unwrap();
    assert!(Arc::ptr_eq(&worktree.reader_arc(), &restored.reader_arc()));
    assert_eq!(restored.live.dirty_count(), 0);
    assert!(restored.live.has_pending_changes());
    assert!(restored.live.is_deleted("deleted.rs"));
    assert!(restored.live.is_deleted("already-absent.rs"));
    for pattern in ["original", "AlphaBeta", "abcdef", ".", "x"] {
        for insensitive in [false, true] {
            assert_eq!(
                candidates(&worktree, pattern, insensitive),
                candidates(&restored, pattern, insensitive)
            );
        }
    }
    for (&tri, ids) in worktree.live.inverted_index() {
        for &id in ids {
            let path = worktree.live.file_path(id).unwrap();
            let restored_id = restored.live.file_id_for_path(path).unwrap();
            assert_eq!(
                worktree.live.get_masks(tri, id),
                restored.live.get_masks(tri, restored_id)
            );
        }
    }
}

#[test]
fn checkpoint_contains_only_overlay_and_never_changes_base_files() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(
        root.path(),
        &[
            ("unchanged-never-saved.rs", b"original unchanged"),
            ("changed.rs", b"original"),
        ],
    );
    let before: Vec<_> = ["lookup.bin", "index.bin", "files.bin", "meta.json"]
        .map(|name| (name, fs::read(directory.path().join(name)).unwrap()))
        .into();
    let base = SharedBase::open(directory.path()).unwrap();
    let mut worktree = base.create_worktree(root.path()).unwrap();
    worktree.live.upsert_file("changed.rs", b"new content");
    let checkpoint = root.path().join("overlay.json");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let first_bytes = fs::read(&checkpoint).unwrap();
    let value = read_checkpoint(&checkpoint);
    let files = value["overlay"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "changed.rs");
    assert!(
        !String::from_utf8(first_bytes.clone())
            .unwrap()
            .contains("unchanged-never-saved.rs")
    );
    base.save_overlay(&worktree, &checkpoint).unwrap();
    assert_eq!(fs::read(&checkpoint).unwrap(), first_bytes);
    for (name, bytes) in before {
        assert_eq!(fs::read(directory.path().join(name)).unwrap(), bytes);
    }
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn repeated_saves_replace_existing_checkpoint_with_latest_overlay() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(
        root.path(),
        &[("changed.rs", b"original"), ("deleted.rs", b"original")],
    );
    let base = SharedBase::open(directory.path()).unwrap();
    let mut worktree = base.create_worktree(root.path()).unwrap();
    let checkpoint = root.path().join("overlay.json");
    worktree.live.upsert_file("changed.rs", b"first_revision");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let first_bytes = fs::read(&checkpoint).unwrap();

    worktree.live.upsert_file("changed.rs", b"second_revision");
    worktree.live.delete_file("deleted.rs");
    worktree.live.upsert_file("new.rs", b"second_revision");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    assert_ne!(fs::read(&checkpoint).unwrap(), first_bytes);
    let restored = base.restore_worktree(root.path(), &checkpoint).unwrap();
    assert!(candidates(&restored, "first_revision", false).is_empty());
    assert_eq!(
        candidates(&restored, "second_revision", false),
        ["changed.rs", "new.rs"]
    );
    assert!(restored.live.is_deleted("deleted.rs"));

    worktree.live.delete_file("changed.rs");
    worktree.live.delete_file("new.rs");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let restored = base.restore_worktree(root.path(), &checkpoint).unwrap();
    assert_eq!(restored.num_files(), 0);
    assert!(restored.live.is_deleted("changed.rs"));
    assert!(restored.live.is_deleted("deleted.rs"));
    assert!(restored.live.is_deleted("new.rs"));
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn locked_checkpoint_preserves_previous_save_and_allows_retry() {
    use std::os::windows::fs::OpenOptionsExt;

    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[("changed.rs", b"original")]);
    let base = SharedBase::open(directory.path()).unwrap();
    let mut worktree = base.create_worktree(root.path()).unwrap();
    let checkpoint = root.path().join("overlay.json");
    worktree.live.upsert_file("changed.rs", b"first_revision");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let first_bytes = fs::read(&checkpoint).unwrap();
    let locked = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&checkpoint)
        .unwrap();

    worktree.live.upsert_file("changed.rs", b"second_revision");
    assert!(base.save_overlay(&worktree, &checkpoint).is_err());
    drop(locked);
    assert_eq!(fs::read(&checkpoint).unwrap(), first_bytes);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    let restored = base.restore_worktree(root.path(), &checkpoint).unwrap();
    assert_eq!(
        candidates(&restored, "first_revision", false),
        ["changed.rs"]
    );

    base.save_overlay(&worktree, &checkpoint).unwrap();
    let restored = base.restore_worktree(root.path(), &checkpoint).unwrap();
    assert_eq!(
        candidates(&restored, "second_revision", false),
        ["changed.rs"]
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn restored_changes_can_be_reverted_to_base_without_affecting_other_worktrees() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(
        root.path(),
        &[("changed.rs", b"original"), ("deleted.rs", b"original")],
    );
    let base = SharedBase::open(directory.path()).unwrap();
    let mut worktree = base.create_worktree(root.path()).unwrap();
    worktree.live.upsert_file("changed.rs", b"replacement");
    worktree.live.delete_file("deleted.rs");
    let checkpoint = root.path().join("overlay.json");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let mut restored = base.restore_worktree(root.path(), &checkpoint).unwrap();
    restored
        .live
        .clear_reconciled_paths(&["changed.rs".into(), "deleted.rs".into()]);
    assert_eq!(
        candidates(&restored, "original", false),
        ["changed.rs", "deleted.rs"]
    );
    assert!(candidates(&worktree, "original", false).is_empty());
    base.save_overlay(&restored, &checkpoint).unwrap();
    let restored = base.restore_worktree(root.path(), &checkpoint).unwrap();
    assert!(!restored.live.has_pending_changes());
    assert_eq!(restored.num_files(), 2);
}

#[test]
fn identical_base_at_another_location_can_restore_checkpoint() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[("base.rs", b"original")]);
    let copied = tempfile::tempdir().unwrap();
    for name in ["lookup.bin", "index.bin", "files.bin", "meta.json"] {
        fs::copy(directory.path().join(name), copied.path().join(name)).unwrap();
    }
    let checkpoint = root.path().join("overlay.json");
    {
        let base = SharedBase::open(directory.path()).unwrap();
        let mut worktree = base.create_worktree(root.path()).unwrap();
        worktree.live.delete_file("base.rs");
        base.save_overlay(&worktree, &checkpoint).unwrap();
    }
    let reopened = SharedBase::open(copied.path()).unwrap();
    let restored = reopened.restore_worktree(root.path(), &checkpoint).unwrap();
    assert_eq!(restored.num_files(), 0);
}

#[test]
fn checkpoint_rejects_changed_postings_even_with_identical_path_table() {
    let root = tempfile::tempdir().unwrap();
    let original = build_base(root.path(), &[("base.rs", b"abcdef")]);
    let changed = build_base(root.path(), &[("base.rs", b"uvwxyz")]);
    assert_eq!(
        fs::read(original.path().join("files.bin")).unwrap(),
        fs::read(changed.path().join("files.bin")).unwrap()
    );
    let base = SharedBase::open(original.path()).unwrap();
    let worktree = base.create_worktree(root.path()).unwrap();
    let checkpoint = root.path().join("overlay.json");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let changed = SharedBase::open(changed.path()).unwrap();
    let error = changed
        .restore_worktree(root.path(), &checkpoint)
        .err()
        .unwrap();
    assert!(error.to_string().contains("different base snapshot"));
}

#[test]
fn checkpoint_identity_covers_masks_not_just_trigram_membership() {
    let root = tempfile::tempdir().unwrap();
    let original = build_base(root.path(), &[("base.rs", b"abcabc")]);
    let changed = tempfile::tempdir().unwrap();
    for name in ["lookup.bin", "index.bin", "files.bin", "meta.json"] {
        fs::copy(original.path().join(name), changed.path().join(name)).unwrap();
    }
    let mut postings = fs::read(changed.path().join("index.bin")).unwrap();
    postings[4] = u8::MAX;
    fs::write(changed.path().join("index.bin"), postings).unwrap();
    let base = SharedBase::open(original.path()).unwrap();
    let worktree = base.create_worktree(root.path()).unwrap();
    let checkpoint = root.path().join("overlay.json");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let changed = SharedBase::open(changed.path()).unwrap();
    assert!(
        changed
            .restore_worktree(root.path(), &checkpoint)
            .err()
            .unwrap()
            .to_string()
            .contains("different base snapshot")
    );
}

#[test]
fn wrong_root_and_reader_swap_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[("base.rs", b"original")]);
    let base = SharedBase::open(directory.path()).unwrap();
    let worktree = base.create_worktree(root.path()).unwrap();
    let checkpoint = root.path().join("overlay.json");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    assert!(
        base.restore_worktree(other.path(), &checkpoint)
            .err()
            .unwrap()
            .to_string()
            .contains("different root")
    );
    assert!(
        base.restore_worktree(&root.path().join("."), &checkpoint)
            .is_ok()
    );
    let before = fs::read(&checkpoint).unwrap();
    let sibling = base.create_worktree(other.path()).unwrap();
    worktree.swap_reader(IndexReader::empty());
    assert!(base.save_overlay(&worktree, &checkpoint).is_err());
    assert_eq!(fs::read(&checkpoint).unwrap(), before);
    assert_eq!(candidates(&sibling, "original", false), ["base.rs"]);
}

#[test]
fn malformed_checkpoints_fail_instead_of_revealing_base_entries() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[("base.rs", b"original")]);
    let base = SharedBase::open(directory.path()).unwrap();
    let mut worktree = base.create_worktree(root.path()).unwrap();
    worktree.live.upsert_file("changed.rs", b"abcdef");
    worktree.live.delete_file("base.rs");
    let checkpoint = root.path().join("overlay.json");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let good = read_checkpoint(&checkpoint);
    let mut bad_values = Vec::new();
    let mut bad = good.clone();
    bad["version"] = json!(2);
    bad_values.push(bad);
    for path in ["../outside.rs", "/absolute.rs", "a//b", "a/./b", "a\\b", ""] {
        let mut bad = good.clone();
        bad["overlay"]["files"][0]["path"] = json!(path);
        bad_values.push(bad);
    }
    let mut bad = good.clone();
    bad["overlay"]["deleted"] = json!(["../outside.rs"]);
    bad_values.push(bad);
    let mut bad = good.clone();
    bad["overlay"]["deleted"] = json!(["changed.rs"]);
    bad_values.push(bad);
    let mut bad = good.clone();
    bad["overlay"]["deleted"] = json!(["base.rs", "base.rs"]);
    bad_values.push(bad);
    let mut bad = good.clone();
    bad["overlay"]["files"] = json!([good["overlay"]["files"][0], good["overlay"]["files"][0]]);
    bad_values.push(bad);
    let mut bad = good.clone();
    bad["overlay"]["files"][0]["trigrams"] = json!([[0x1000000, 1, 0]]);
    bad_values.push(bad);
    let mut bad = good.clone();
    bad["overlay"]["files"][0]["trigrams"] = json!([[1, 0, 0]]);
    bad_values.push(bad);
    let mut bad = good.clone();
    bad["overlay"]["files"][0]["trigrams"] = json!([[1, 1, 0], [1, 2, 0]]);
    bad_values.push(bad);
    let mut bad = good.clone();
    bad["overlay"].as_object_mut().unwrap().remove("deleted");
    bad_values.push(bad);
    for bad in bad_values {
        fs::write(&checkpoint, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(
            base.restore_worktree(root.path(), &checkpoint).is_err(),
            "{bad}"
        );
    }
    fs::write(&checkpoint, b"{\"version\":").unwrap();
    assert!(base.restore_worktree(root.path(), &checkpoint).is_err());
    fs::remove_file(&checkpoint).unwrap();
    assert!(base.restore_worktree(root.path(), &checkpoint).is_err());
    assert!(worktree.live.is_deleted("base.rs"));
}

#[test]
fn invalid_live_paths_fail_before_replacing_a_checkpoint() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[("base.rs", b"original")]);
    let base = SharedBase::open(directory.path()).unwrap();
    let mut worktree = base.create_worktree(root.path()).unwrap();
    let checkpoint = root.path().join("overlay.json");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let before = fs::read(&checkpoint).unwrap();
    worktree.live.upsert_file("../outside.rs", b"invalid");
    assert!(base.save_overlay(&worktree, &checkpoint).is_err());
    assert_eq!(fs::read(&checkpoint).unwrap(), before);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn checkpoint_cannot_overwrite_base_or_leave_temporary_files_after_failure() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[("base.rs", b"original")]);
    let base = SharedBase::open(directory.path()).unwrap();
    let worktree = base.create_worktree(root.path()).unwrap();
    let before = fs::read(directory.path().join("index.bin")).unwrap();
    assert!(
        base.save_overlay(&worktree, &directory.path().join("index.bin"))
            .is_err()
    );
    assert_eq!(
        fs::read(directory.path().join("index.bin")).unwrap(),
        before
    );
    assert!(
        base.save_overlay(&worktree, &root.path().join("missing").join("overlay.json"))
            .is_err()
    );
    let destination = root.path().join("directory-not-file");
    fs::create_dir(&destination).unwrap();
    assert!(base.save_overlay(&worktree, &destination).is_err());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    assert_eq!(candidates(&worktree, "original", false), ["base.rs"]);
}

#[test]
fn incomplete_or_legacy_metadata_does_not_change_existing_open_behavior() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[("base.rs", b"original")]);
    let original = IndexMeta::load(directory.path()).unwrap();
    let mut variants = Vec::new();
    let mut meta = original.clone();
    meta.complete = false;
    variants.push(meta);
    let mut meta = original.clone();
    meta.hidden_complete = false;
    variants.push(meta);
    let mut meta = original.clone();
    meta.file_table_id = None;
    variants.push(meta);
    let mut meta = original;
    meta.version += 1;
    variants.push(meta);
    for meta in variants {
        meta.save(directory.path()).unwrap();
        assert!(SharedBase::open(directory.path()).is_err());
        let legacy = HybridIndex::open(directory.path(), root.path()).unwrap();
        assert_eq!(candidates(&legacy, "original", false), ["base.rs"]);
    }
}

#[test]
fn shared_base_rejects_unsafe_or_duplicate_paths() {
    let root = tempfile::tempdir().unwrap();
    for files in [
        vec![("../outside.rs", b"original".as_slice())],
        vec![
            ("same.rs", b"first".as_slice()),
            ("same.rs", b"second".as_slice()),
        ],
    ] {
        let directory = build_base(root.path(), &files);
        assert!(SharedBase::open(directory.path()).is_err());
    }
}

#[test]
fn ordinary_builder_produces_a_compatible_shared_base() {
    let root = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    write(root.path(), ".hidden.rs", b"hidden_content");
    write(root.path(), "source.rs", b"source_content");
    builder::build_index(root.path(), Some(directory.path()), true, false, &[]).unwrap();
    let base = SharedBase::open(directory.path()).unwrap();
    let worktree = base.create_worktree(root.path()).unwrap();
    assert_eq!(matches(&worktree, "hidden_content"), [".hidden.rs"]);
    assert_eq!(matches(&worktree, "source_content"), ["source.rs"]);
}

#[test]
fn many_worktrees_share_one_reader_while_querying_independent_overlays() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[("base.rs", b"original")]);
    let base = SharedBase::open(directory.path()).unwrap();
    let reference = base.create_worktree(root.path()).unwrap().reader_arc();
    std::thread::scope(|scope| {
        for number in 0..8 {
            let base = base.clone();
            let reference = Arc::clone(&reference);
            scope.spawn(move || {
                let root = tempfile::tempdir().unwrap();
                let mut worktree = base.create_worktree(root.path()).unwrap();
                assert!(Arc::ptr_eq(&reference, &worktree.reader_arc()));
                let content = format!("private_content_{number}");
                worktree.live.upsert_file("base.rs", content.as_bytes());
                assert!(candidates(&worktree, "original", false).is_empty());
                assert_eq!(candidates(&worktree, &content, false), ["base.rs"]);
                let checkpoint = root.path().join("overlay.json");
                base.save_overlay(&worktree, &checkpoint).unwrap();
                let restored = base.restore_worktree(root.path(), &checkpoint).unwrap();
                assert_eq!(candidates(&restored, &content, false), ["base.rs"]);
                assert!(Arc::ptr_eq(&reference, &restored.reader_arc()));
            });
        }
    });
}

#[test]
fn worktree_root_must_exist_and_be_a_directory() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[]);
    let base = SharedBase::open(directory.path()).unwrap();
    assert!(base.create_worktree(&root.path().join("missing")).is_err());
    let file = root.path().join("file");
    fs::write(&file, b"").unwrap();
    assert!(base.create_worktree(&file).is_err());
    let worktree = base.create_worktree(root.path()).unwrap();
    let checkpoint = root.path().join("empty-overlay.json");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let restored = base.restore_worktree(root.path(), &checkpoint).unwrap();
    assert_eq!(restored.num_files(), 0);
    assert!(!restored.live.has_pending_changes());
}
