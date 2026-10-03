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
    for root in [
        json!(null),
        json!({"encoding": "unknown", "units": [1, 2, 3]}),
        json!({"encoding": "windows-wide", "units": [65536]}),
        json!({"encoding": "unix-bytes", "units": [256]}),
    ] {
        let mut bad = good.clone();
        bad["root"] = root;
        bad_values.push(bad);
    }
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
fn directory_shaped_checkpoint_paths_do_not_replace_existing_files() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[("base.rs", b"original")]);
    let base = SharedBase::open(directory.path()).unwrap();
    let mut worktree = base.create_worktree(root.path()).unwrap();
    let checkpoint = root.path().join("overlay.json");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let before = fs::read(&checkpoint).unwrap();
    worktree.live.upsert_file("new.rs", b"changed overlay");
    #[cfg(not(windows))]
    let suffixes = ["/", "/.", "/./"];
    #[cfg(windows)]
    let suffixes = ["/", "/.", "/./", "\\", "\\.", "\\.\\"];
    for suffix in suffixes {
        let mut malformed = checkpoint.as_os_str().to_os_string();
        malformed.push(suffix);
        assert!(
            base.save_overlay(&worktree, Path::new(&malformed)).is_err(),
            "accepted directory-shaped suffix {suffix:?}"
        );
        assert_eq!(fs::read(&checkpoint).unwrap(), before);
    }
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn shared_base_rejects_unaligned_overlapping_and_gapped_posting_ranges() {
    let root = tempfile::tempdir().unwrap();
    for (kind, entry, offset) in [
        ("unaligned", 0, 1_u64),
        ("overlapping", 1, 0),
        ("gapped", 0, 6),
    ] {
        let directory = build_base(root.path(), &[("base.rs", b"original")]);
        let mut lookup = fs::read(directory.path().join("lookup.bin")).unwrap();
        let start = entry * 16 + 4;
        lookup[start..start + 8].copy_from_slice(&offset.to_le_bytes());
        fs::write(directory.path().join("lookup.bin"), lookup).unwrap();
        let ordinary = IndexReader::open(directory.path()).unwrap();
        assert!(ordinary.validate_lookup().is_ok());
        assert!(SharedBase::open(directory.path()).is_err(), "{kind}");
    }
}

#[test]
fn shared_base_rejects_unreferenced_posting_bytes() {
    let root = tempfile::tempdir().unwrap();
    for extra_bytes in [1, 6] {
        let directory = build_base(root.path(), &[("base.rs", b"original")]);
        let mut postings = fs::read(directory.path().join("index.bin")).unwrap();
        postings.extend_from_within(..extra_bytes);
        fs::write(directory.path().join("index.bin"), postings).unwrap();
        assert!(SharedBase::open(directory.path()).is_err());
    }
}

#[test]
fn shared_base_rejects_invalid_posting_ids_and_location_masks() {
    let root = tempfile::tempdir().unwrap();
    for last in [false, true] {
        for replacement in [
            PostingEntry {
                file_id: 2,
                loc_mask: 1,
                next_mask: 0,
            },
            PostingEntry {
                file_id: u32::MAX,
                loc_mask: 1,
                next_mask: 0,
            },
            PostingEntry {
                file_id: 0,
                loc_mask: 0,
                next_mask: 0,
            },
        ] {
            let directory = build_base(
                root.path(),
                &[("first.rs", b"original"), ("second.rs", b"original")],
            );
            let mut postings = fs::read(directory.path().join("index.bin")).unwrap();
            let start = if last { postings.len() - 6 } else { 0 };
            postings[start..start + 6].copy_from_slice(&replacement.encode());
            fs::write(directory.path().join("index.bin"), postings).unwrap();
            let ordinary = IndexReader::open(directory.path()).unwrap();
            assert!(ordinary.validate_lookup().is_ok());
            assert!(
                SharedBase::open(directory.path()).is_err(),
                "accepted {replacement:?} at byte offset {start}"
            );
        }
    }
}

#[test]
fn shared_base_rejects_out_of_range_trigrams() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[("base.rs", b"original")]);
    let mut lookup = fs::read(directory.path().join("lookup.bin")).unwrap();
    let start = lookup.len() - 16;
    lookup[start..start + 4].copy_from_slice(&0x01000000_u32.to_le_bytes());
    fs::write(directory.path().join("lookup.bin"), lookup).unwrap();
    assert!(SharedBase::open(directory.path()).is_err());
}

#[test]
fn shared_base_rejects_duplicate_or_descending_posting_ids() {
    let root = tempfile::tempdir().unwrap();
    for ids in [[0_u32, 0_u32], [1, 1], [1, 0]] {
        let directory = build_base(root.path(), &[("first.rs", b"abc"), ("second.rs", b"abc")]);
        let mut postings = fs::read(directory.path().join("index.bin")).unwrap();
        assert_eq!(postings.len(), 12);
        postings[..4].copy_from_slice(&ids[0].to_le_bytes());
        postings[6..10].copy_from_slice(&ids[1].to_le_bytes());
        postings[5] = 1;
        postings[11] = 0xff;
        fs::write(directory.path().join("index.bin"), postings).unwrap();
        let ordinary = IndexReader::open(directory.path()).unwrap();
        assert!(ordinary.validate_lookup().is_ok());
        assert!(
            SharedBase::open(directory.path()).is_err(),
            "accepted non-increasing IDs {ids:?} with distinct masks"
        );
    }
}

#[test]
fn shared_base_rejects_mismatched_empty_sections_even_with_zero_metadata_count() {
    let root = tempfile::tempdir().unwrap();
    for name in ["lookup.bin", "index.bin"] {
        let directory = build_base(root.path(), &[("base.rs", b"original")]);
        fs::write(directory.path().join(name), []).unwrap();
        let mut meta = IndexMeta::load(directory.path()).unwrap();
        meta.num_trigrams = 0;
        meta.save(directory.path()).unwrap();

        assert!(
            SharedBase::open(directory.path()).is_err(),
            "accepted an empty {name} with a nonempty companion section"
        );
        // Preserve the ordinary reader's existing empty-section behavior.
        let ordinary = IndexReader::open(directory.path()).unwrap();
        assert_eq!(ordinary.num_trigrams(), 0);
    }
}

#[test]
fn shared_base_rejects_truncated_sections_with_stale_metadata() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[("base.rs", b"original")]);
    let lookup = fs::read(directory.path().join("lookup.bin")).unwrap();
    assert!(lookup.len() > 16);
    fs::write(
        directory.path().join("lookup.bin"),
        &lookup[..lookup.len() - 16],
    )
    .unwrap();
    assert!(
        SharedBase::open(directory.path()).is_err(),
        "accepted a missing lookup entry with a stale trigram count"
    );

    for name in ["lookup.bin", "index.bin"] {
        fs::write(directory.path().join(name), []).unwrap();
    }
    assert!(
        SharedBase::open(directory.path()).is_err(),
        "accepted empty sections with nonzero metadata counts"
    );
}

#[test]
fn shared_base_rejects_inconsistent_file_and_trigram_counts() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[("base.rs", b"original")]);
    let original = IndexMeta::load(directory.path()).unwrap();
    for count in [0, original.num_files + 1, u64::MAX] {
        let mut meta = original.clone();
        meta.num_files = count;
        meta.save(directory.path()).unwrap();
        assert!(SharedBase::open(directory.path()).is_err());
    }
    for count in [0, original.num_trigrams + 1, u64::MAX] {
        let mut meta = original.clone();
        meta.num_trigrams = count;
        meta.save(directory.path()).unwrap();
        assert!(SharedBase::open(directory.path()).is_err());
    }
    original.save(directory.path()).unwrap();
    assert!(SharedBase::open(directory.path()).is_ok());
}

#[test]
fn shared_base_accepts_empty_sections_for_empty_and_short_files() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[("empty.rs", b""), ("short.rs", b"ab")]);
    let base = SharedBase::open(directory.path()).unwrap();
    let worktree = base.create_worktree(root.path()).unwrap();
    assert_eq!(worktree.reader_arc().num_trigrams(), 0);
    assert_eq!(candidates(&worktree, ".", false), ["empty.rs", "short.rs"]);
    let checkpoint = root.path().join("overlay.json");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let restored = base.restore_worktree(root.path(), &checkpoint).unwrap();
    assert_eq!(restored.num_files(), 2);
}

#[test]
fn unicode_root_checkpoint_retains_legacy_string_representation() {
    let root = tempfile::tempdir().unwrap();
    let directory = build_base(root.path(), &[]);
    let base = SharedBase::open(directory.path()).unwrap();
    let worktree = base.create_worktree(root.path()).unwrap();
    let checkpoint = root.path().join("overlay.json");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let value = read_checkpoint(&checkpoint);
    assert_eq!(value["version"], 1);
    assert_eq!(value["root"], json!(fs::canonicalize(root.path()).unwrap()));
    let legacy: std::path::PathBuf = serde_json::from_value(value["root"].clone()).unwrap();
    assert_eq!(legacy, worktree.root);
    assert!(base.restore_worktree(root.path(), &checkpoint).is_ok());
}

// Native macOS filesystems reject these names; the encoding itself is covered
// without filesystem I/O in the shared module's Unix unit tests.
#[cfg(any(target_os = "linux", windows))]
#[test]
fn non_unicode_roots_roundtrip_without_lossy_identity_collisions() {
    use std::ffi::OsString;

    fn component(last: u8) -> OsString {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(vec![b'r', b'o', b'o', b't', last])
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            OsString::from_wide(&[114, 111, 111, 116, 0xd800 + u16::from(last)])
        }
    }

    let parent = tempfile::tempdir().unwrap();
    let first = parent.path().join(component(0xfe));
    let second = parent.path().join(component(0xff));
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let directory = build_base(parent.path(), &[("base.rs", b"original")]);
    let base = SharedBase::open(directory.path()).unwrap();
    let mut worktree = base.create_worktree(&first).unwrap();
    assert!(worktree.root.to_str().is_none());
    assert_eq!(first.to_string_lossy(), second.to_string_lossy());
    worktree.live.upsert_file("base.rs", b"replacement");
    let checkpoint = first.join("overlay.json");
    base.save_overlay(&worktree, &checkpoint).unwrap();
    let encoded_root = read_checkpoint(&checkpoint)["root"].clone();
    assert!(encoded_root.is_object());
    #[cfg(unix)]
    assert_eq!(encoded_root["encoding"], "unix-bytes");
    #[cfg(windows)]
    assert_eq!(encoded_root["encoding"], "windows-wide");
    let restored = base.restore_worktree(&first, &checkpoint).unwrap();
    assert_eq!(restored.root, worktree.root);
    assert_eq!(candidates(&restored, "replacement", false), ["base.rs"]);
    assert!(
        base.restore_worktree(&second, &checkpoint)
            .err()
            .unwrap()
            .to_string()
            .contains("different root")
    );
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
