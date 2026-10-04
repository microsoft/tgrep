use super::*;
use crate::generations::{GenerationManager, IndexingProfile};
use crate::{encoding, query, trigram};
use std::process::{Command, Output};
use std::sync::Barrier;
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    storage: PathBuf,
}

fn command(root: &Path) -> Command {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    command.current_dir(root).args([
        "-c",
        "user.name=Worktree Tests",
        "-c",
        "user.email=worktree@example.invalid",
        "-c",
        "commit.gpgsign=false",
    ]);
    command
}

fn checked(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn git(root: &Path, args: &[&str]) -> String {
    checked(command(root).args(args).output().unwrap())
}

fn write(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn commit(root: &Path) {
    git(root, &["add", "--all"]);
    git(
        root,
        &["commit", "--quiet", "--allow-empty", "-m", "fixture"],
    );
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("tgrep worktree spaces ")
            .tempdir()
            .unwrap();
        let root = temp.path().join("repo");
        let storage = temp.path().join("shared storage");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&storage).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        Self {
            _temp: temp,
            root,
            storage,
        }
    }

    fn generation(&self) -> Arc<Generation> {
        GenerationManager::with_storage(Repository::discover(&self.root).unwrap(), &self.storage)
            .unwrap()
            .ensure("HEAD", IndexingProfile::default(), None)
            .unwrap()
            .generation
    }

    fn linked(&self, name: &str) -> PathBuf {
        let path = self._temp.path().join(name);
        checked(
            command(&self.root)
                .args(["worktree", "add", "--quiet", "--detach"])
                .arg(&path)
                .output()
                .unwrap(),
        );
        path
    }

    fn view(&self, root: &Path, generation: &Arc<Generation>) -> WorktreeView {
        WorktreeView::new(root, generation.clone(), WorktreeOptions::default()).unwrap()
    }
}

fn candidates(view: &WorktreeView, pattern: &str) -> Vec<String> {
    let plan = query::build_query_plan(pattern, false).unwrap();
    view.with_snapshot(|snapshot| snapshot.candidates(&plan, "", true))
        .unwrap()
}

fn matches(view: &WorktreeView, pattern: &str) -> Vec<String> {
    let plan = query::build_query_plan(pattern, false).unwrap();
    let regex = regex::bytes::Regex::new(pattern).unwrap();
    view.with_snapshot(|snapshot| {
        snapshot
            .candidates(&plan, "", true)
            .into_iter()
            .filter(|path| {
                let bytes = fs::read(snapshot.root().join(path)).unwrap();
                regex.is_match(&encoding::decode_for_index(&bytes))
            })
            .collect()
    })
    .unwrap()
}

fn assert_scan_parity(view: &WorktreeView, patterns: &[&str]) {
    let options = walker::WalkOptions {
        include_hidden: true,
        no_ignore: view.options.walk.no_ignore,
        no_require_git: view.options.walk.no_require_git,
        max_file_size: None,
        exclude_paths: view.options.walk.exclude_paths.clone(),
        exclude_dirs: view.options.walk.exclude_dirs.clone(),
        ..walker::WalkOptions::default()
    };
    let walk = walker::walk_dir(view.root(), &options);
    assert_eq!(walk.skipped_error, 0);
    let text: BTreeMap<String, Vec<u8>> = walk
        .files
        .into_iter()
        .filter_map(|path| {
            let bytes = fs::read(&path).unwrap();
            if view
                .options
                .walk
                .max_file_size
                .is_some_and(|limit| bytes.len() as u64 > limit)
            {
                return None;
            }
            let text = encoding::decode_for_index(&bytes).into_owned();
            (!trigram::is_binary(&text)).then(|| {
                (
                    path.strip_prefix(view.root())
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .replace('\\', "/"),
                    text,
                )
            })
        })
        .collect();
    // Independently extracted whole-file postings check candidate parity,
    // not just matches (a stale base can create false positives as well).
    let mut scan = LiveIndex::new();
    for (path, text) in &text {
        scan.upsert_file(path, text);
    }
    for pattern in patterns {
        let regex = regex::bytes::Regex::new(pattern).unwrap();
        let expected_matches: Vec<_> = text
            .iter()
            .filter(|(_, text)| regex.is_match(text))
            .map(|(path, _)| path.clone())
            .collect();
        assert_eq!(
            matches(view, pattern),
            expected_matches,
            "matches: {pattern}"
        );
        let plan = query::build_query_plan(pattern, false).unwrap();
        let ids = if plan.is_match_all() {
            scan.all_file_ids()
        } else {
            query::execute_plan_with_masks(&plan, &|tri| scan.lookup_trigram_with_masks(tri))
        };
        let mut expected_candidates: Vec<_> = ids
            .into_iter()
            .map(|id| scan.file_path(id).unwrap().to_string())
            .collect();
        expected_candidates.sort_unstable();
        assert_eq!(
            candidates(view, pattern),
            expected_candidates,
            "candidates: {pattern}"
        );
    }
}

#[test]
fn identical_worktrees_share_reader_and_verify_without_extraction() {
    let fixture = Fixture::new();
    write(&fixture.root, "one.txt", b"base needle\n");
    write(&fixture.root, "empty.txt", b"");
    write(&fixture.root, "short.txt", b"xy");
    commit(&fixture.root);
    let pin = fixture.generation();
    let linked = fixture.linked("linked");
    let first = fixture.view(&fixture.root, &pin);
    let second = fixture.view(&linked, &pin);
    assert!(matches!(
        first.with_snapshot(|_| ()),
        Err(WorktreeError::NotReady)
    ));
    first.invalidate_path(Path::new("one.txt")).unwrap();
    for view in [&first, &second] {
        let stats = view.refresh().unwrap();
        assert!(stats.full);
        assert_eq!(stats.files_read, 3);
        assert_eq!(stats.files_decoded, 3);
        assert_eq!(stats.files_extracted, 0);
        assert_eq!(stats.base_reused, 3);
        assert_scan_parity(view, &["needle", "xy", "^$", "need.e", "x|needle"]);
    }
    assert!(Arc::ptr_eq(
        &first.state.read().unwrap().index.reader_arc(),
        &second.state.read().unwrap().index.reader_arc()
    ));
    assert!(Arc::ptr_eq(first.generation(), second.generation()));
    let no_op = first.refresh().unwrap();
    assert!(no_op.full);
    assert_eq!(no_op.files_read, 3);
    assert_eq!(no_op.files_extracted, 0);
}

#[test]
fn divergent_commits_staging_renames_deletes_and_whole_file_overlays_are_isolated() {
    let fixture = Fixture::new();
    for path in ["modified.txt", "deleted.txt", "renamed.txt", "cross.txt"] {
        write(&fixture.root, path, b"original base needle\n");
    }
    commit(&fixture.root);
    let pin = fixture.generation();
    let a = fixture.linked("a");
    let b = fixture.linked("b");
    write(&a, "modified.txt", b"committed divergence\n");
    commit(&a);
    assert!(git(&a, &["status", "--porcelain"]).is_empty());
    fs::remove_file(a.join("deleted.txt")).unwrap();
    git(&a, &["mv", "renamed.txt", "newname.txt"]);
    write(&a, "staged.txt", b"staged addition\n");
    git(&a, &["add", "staged.txt"]);
    write(&a, "staged.txt", b"unstaged after staging\n");
    write(&a, "untracked.txt", b"untracked addition\n");
    write(&a, "cross.txt", b"prefix ABmiddleCD suffix\n");
    write(&b, "modified.txt", b"other worktree version\n");
    let va = fixture.view(&a, &pin);
    let vb = fixture.view(&b, &pin);
    let initial = va.refresh().unwrap();
    assert_eq!(initial.files_extracted, 4);
    assert_eq!(initial.base_files_copied, 1);
    assert!(initial.postings_copied > 0);
    vb.refresh().unwrap();
    assert_eq!(candidates(&va, "original"), ["newname.txt"]);
    assert_eq!(
        candidates(&vb, "original"),
        ["cross.txt", "deleted.txt", "renamed.txt"]
    );
    assert_eq!(matches(&va, "committed"), ["modified.txt"]);
    assert!(matches(&vb, "committed").is_empty());
    assert!(matches(&va, "other worktree").is_empty());
    assert_eq!(matches(&vb, "other worktree"), ["modified.txt"]);
    assert_scan_parity(
        &va,
        &[
            "original",
            "divergence",
            "staged",
            "untracked",
            "ABmiddleCD",
            "prefix.*suffix",
            "^",
            ".*",
        ],
    );
    commit(&a);
    let stats = va.reconcile_full().unwrap();
    assert_eq!(
        stats.files_extracted, 0,
        "committing must preserve overrides relative to the pin"
    );
    assert_eq!(stats.overlay_reused, 5);
    assert_eq!(matches(&va, "committed"), ["modified.txt"]);
    write(&a, "modified.txt", b"original base needle\n");
    va.invalidate_path(Path::new("modified.txt")).unwrap();
    let stats = va.refresh().unwrap();
    assert_eq!(stats.files_read, 1);
    assert_eq!(stats.files_extracted, 0);
    assert!(!va.state.read().unwrap().index.live.has_path("modified.txt"));
    assert_eq!(matches(&va, "original"), ["modified.txt", "newname.txt"]);
}

#[test]
fn hints_are_bounded_and_full_refresh_repairs_unreported_same_stat_edits() {
    let fixture = Fixture::new();
    write(&fixture.root, "one.txt", b"aaaaaa");
    write(&fixture.root, "two.txt", b"bbbbbb");
    write(&fixture.root, "three.txt", b"cccccc");
    commit(&fixture.root);
    let view = WorktreeView::new(
        &fixture.root,
        fixture.generation(),
        WorktreeOptions {
            hint_capacity: 1,
            ..WorktreeOptions::default()
        },
    )
    .unwrap();
    view.refresh().unwrap();
    let path = fixture.root.join("one.txt");
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    git(
        &fixture.root,
        &["update-index", "--assume-unchanged", "one.txt"],
    );
    fs::write(&path, b"zzzzzz").unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert!(git(&fixture.root, &["status", "--porcelain"]).is_empty());
    assert!(
        candidates(&view, "zzz").is_empty(),
        "unreported edits need a full check"
    );
    let full = view.reconcile_full().unwrap();
    assert_eq!(full.files_read, 3);
    assert_eq!(full.files_extracted, 1);
    assert_eq!(matches(&view, "zzz"), ["one.txt"]);
    let epoch = view.invalidate_path(Path::new("one.txt")).unwrap();
    assert!(matches!(
        view.with_snapshot(|_| ()),
        Err(WorktreeError::NotReady)
    ));
    let hinted = view.refresh().unwrap();
    assert!(hinted.epoch >= epoch);
    assert!(!hinted.full);
    assert_eq!(hinted.files_read, 1);
    assert_eq!(hinted.content_reads_avoided, 2);
    assert_eq!(hinted.files_extracted, 0);
    assert_eq!(hinted.overlay_reused, 1);
    view.invalidate_path(Path::new("one.txt")).unwrap();
    view.invalidate_path(Path::new("two.txt")).unwrap();
    let status = view.status().unwrap();
    assert!(!status.ready);
    assert!(status.full_required);
    assert_eq!(status.pending_paths, 0);
    assert_eq!(view.refresh().unwrap().files_read, 3);
    assert!(view.invalidate_path(Path::new("..\\escape")).is_err());
    assert!(!view.status().unwrap().ready);
    assert!(view.refresh().unwrap().full);
    let before = view.status().unwrap().published_epoch.unwrap();
    assert!(
        view.refresh().unwrap().epoch > before,
        "no-hint refresh gets a new publication epoch"
    );
}

#[test]
fn membership_visibility_binary_size_and_storage_rules_follow_the_requesting_root() {
    let fixture = Fixture::new();
    for (path, bytes) in [
        ("source.txt", &b"needle"[..]),
        ("asset.bin", b"textual needle"),
        ("binary-content.txt", b"needle\0binary"),
        (".secret", b"hidden needle"),
        (".hidden/visible.txt", b"hidden directory needle"),
        ("ignored.txt", b"ignored needle"),
        ("large.txt", b"needle012345678901234567890123456789"),
        ("empty.txt", b""),
    ] {
        write(&fixture.root, path, bytes);
    }
    commit(&fixture.root);
    let pin = fixture.generation();
    assert!(pin.base().reader().contains_path("asset.bin"));
    let linked = fixture.linked("filtered");
    write(&linked, ".gitignore", b"ignored.txt\n");
    write(&linked, ".ignore", b"source.txt\n");
    for path in [
        ".tgrep/staging/leak.txt",
        "custom-index/.retired/leak.txt",
        "checkpoints/staging.txt",
        ".git-fake/ok.txt",
    ] {
        write(&linked, path, b"needle");
    }
    let options = WorktreeOptions {
        walk: MetaWalkOptions {
            max_file_size: Some(24),
            exclude_paths: vec![PathBuf::from("custom-index")],
            ..MetaWalkOptions::default()
        },
        checkpoint_directory: Some(PathBuf::from("checkpoints")),
        ..WorktreeOptions::default()
    };
    let view = WorktreeView::new(&linked, pin.clone(), options).unwrap();
    view.refresh().unwrap();
    let files = view
        .with_snapshot(|snapshot| snapshot.files("", true))
        .unwrap();
    assert!(files.contains(&"asset.bin".into()));
    assert!(files.contains(&"binary-content.txt".into()));
    assert!(files.contains(&"empty.txt".into()));
    for path in [
        "source.txt",
        "ignored.txt",
        "large.txt",
        ".tgrep/staging/leak.txt",
        "custom-index/.retired/leak.txt",
        "checkpoints/staging.txt",
    ] {
        assert!(!files.contains(&path.into()), "{path}");
    }
    assert!(!candidates(&view, "needle").contains(&"asset.bin".into()));
    assert!(!candidates(&view, "needle").contains(&"binary-content.txt".into()));
    assert!(
        !view
            .with_snapshot(|snapshot| snapshot.files("", false))
            .unwrap()
            .contains(&".secret".into())
    );
    assert_eq!(
        view.with_snapshot(|snapshot| snapshot.files(".hidden/", false))
            .unwrap(),
        [".hidden/visible.txt"]
    );
    assert_scan_parity(&view, &["needle", "^$", "textual", "ignored", "source"]);
    fs::rename(linked.join("asset.bin"), linked.join("asset.txt")).unwrap();
    write(&linked, "source.txt", b"needle");
    fs::remove_file(linked.join(".ignore")).unwrap();
    view.invalidate_all().unwrap();
    view.refresh().unwrap();
    assert!(candidates(&view, "textual").contains(&"asset.txt".into()));
    assert!(candidates(&view, "needle").contains(&"source.txt".into()));
    let other = fixture.view(&fixture.root, &pin);
    other.refresh().unwrap();
    assert!(candidates(&other, "ignored").contains(&"ignored.txt".into()));
    assert_scan_parity(&view, &["needle", "textual", ".*"]);
}

#[test]
fn crlf_and_smudge_checkouts_are_verified_even_when_git_reports_clean() {
    let fixture = Fixture::new();
    write(&fixture.root, "one.txt", b"first\nsecond needle\n");
    write(&fixture.root, "two.txt", b"other\nmultiline term\n");
    write(&fixture.root, "filtered.txt", b"CLEAN needle\n");
    write(
        &fixture.root,
        ".gitattributes",
        b"filtered.txt filter=fixture\n",
    );
    commit(&fixture.root);
    let pin = fixture.generation();
    let identical = fixture.view(&fixture.root, &pin);
    assert_eq!(identical.refresh().unwrap().files_extracted, 0);
    git(&fixture.root, &["config", "core.autocrlf", "true"]);
    // Git runs these deliberately configured fixture filters through its shell.
    git(
        &fixture.root,
        &["config", "filter.fixture.clean", "sed s/SMUDGED/CLEAN/g"],
    );
    git(
        &fixture.root,
        &["config", "filter.fixture.smudge", "sed s/CLEAN/SMUDGED/g"],
    );
    let linked = fixture.linked("transformed");
    assert!(
        fs::read(linked.join("one.txt"))
            .unwrap()
            .windows(2)
            .any(|bytes| bytes == b"\r\n")
    );
    assert!(
        fs::read(linked.join("filtered.txt"))
            .unwrap()
            .starts_with(b"SMUDGED")
    );
    assert!(git(&linked, &["status", "--porcelain"]).is_empty());
    let view = fixture.view(&linked, &pin);
    let stats = view.refresh().unwrap();
    assert_eq!(stats.files_read, 4);
    assert_eq!(stats.files_decoded, 4);
    assert_eq!(
        stats.files_extracted, 4,
        "all four clean files differ from raw LF blobs"
    );
    assert_eq!(stats.base_reused, 0);
    assert_eq!(matches(&view, "SMUDGED"), ["filtered.txt"]);
    assert!(matches(&view, "CLEAN").is_empty());
    assert_scan_parity(
        &view,
        &["second needle", "SMUDGED", "CLEAN", "term", "^first"],
    );
    let no_op = view.reconcile_full().unwrap();
    assert_eq!(no_op.files_extracted, 0);
    assert_eq!(no_op.overlay_reused, 4);
}

#[test]
fn checkout_decoding_can_reuse_base_despite_different_raw_bytes() {
    let fixture = Fixture::new();
    write(&fixture.root, "text.txt", b"decoded needle");
    commit(&fixture.root);
    let pin = fixture.generation();
    let bytes: Vec<_> = [0xff, 0xfe]
        .into_iter()
        .chain("decoded needle".encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    write(&fixture.root, "text.txt", &bytes);
    let view = fixture.view(&fixture.root, &pin);
    let stats = view.refresh().unwrap();
    assert_eq!(stats.files_extracted, 0);
    assert_eq!(stats.base_reused, 1);
    assert_scan_parity(&view, &["decoded", "needle", ".*"]);
}

#[test]
fn sparse_missing_and_skip_worktree_paths_never_leak_base_terms() {
    let fixture = Fixture::new();
    write(&fixture.root, "keep/source.txt", b"visible needle");
    write(&fixture.root, "omit/source.txt", b"sparse needle");
    write(&fixture.root, "skip.txt", b"skip original");
    commit(&fixture.root);
    let pin = fixture.generation();
    let linked = fixture.linked("sparse");
    git(&linked, &["sparse-checkout", "init", "--cone"]);
    git(&linked, &["sparse-checkout", "set", "keep"]);
    assert!(!linked.join("omit/source.txt").exists());
    git(&linked, &["update-index", "--skip-worktree", "skip.txt"]);
    write(&linked, "skip.txt", b"skip changed");
    let view = fixture.view(&linked, &pin);
    view.refresh().unwrap();
    assert!(candidates(&view, "sparse").is_empty());
    assert!(candidates(&view, "original").is_empty());
    assert_eq!(matches(&view, "changed"), ["skip.txt"]);
    assert_scan_parity(&view, &["needle", "original", "changed", ".*"]);
    git(&linked, &["sparse-checkout", "disable"]);
    view.reconcile_full().unwrap();
    assert_eq!(matches(&view, "sparse"), ["omit/source.txt"]);
}

#[test]
fn epoch_change_after_preparation_prevents_stale_ready_publication() {
    let fixture = Fixture::new();
    write(&fixture.root, "file.txt", b"old needle");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| {
        let task = scope.spawn(|| {
            view.refresh_inner(
                || {},
                || {
                    barrier.wait();
                    barrier.wait();
                },
            )
        });
        barrier.wait();
        assert!(!view.status().unwrap().ready);
        assert!(matches!(
            view.with_snapshot(|_| ()),
            Err(WorktreeError::NotReady)
        ));
        write(&fixture.root, "file.txt", b"new needle");
        view.invalidate_path(Path::new("file.txt")).unwrap();
        barrier.wait();
        assert!(matches!(
            task.join().unwrap(),
            Err(WorktreeError::ChangedDuringReconcile)
        ));
    });
    assert!(!view.status().unwrap().ready);
    assert!(view.status().unwrap().full_required);
    assert!(view.refresh().unwrap().full);
    assert_eq!(matches(&view, "new"), ["file.txt"]);
    assert!(candidates(&view, "old").is_empty());
}

#[test]
fn read_and_discovery_errors_close_the_gate_and_force_full_retry() {
    let fixture = Fixture::new();
    write(&fixture.root, "file.txt", b"old needle");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    view.refresh().unwrap();
    let error = view
        .refresh_inner(
            || fs::remove_file(fixture.root.join("file.txt")).unwrap(),
            || {},
        )
        .unwrap_err();
    assert!(matches!(error, WorktreeError::Io(_)));
    assert!(!view.status().unwrap().ready);
    write(&fixture.root, "file.txt", b"new needle");
    view.refresh().unwrap();
    write(&fixture.root, ".gitignore", b"[z-a]\n");
    assert!(matches!(
        view.refresh(),
        Err(WorktreeError::IncompleteWalk(_))
    ));
    assert!(!view.status().unwrap().ready);
    fs::remove_file(fixture.root.join(".gitignore")).unwrap();
    view.refresh().unwrap();
    assert_eq!(matches(&view, "new"), ["file.txt"]);
}

#[test]
fn actual_bytes_not_discovery_size_decide_size_and_binary_classification() {
    let fixture = Fixture::new();
    write(&fixture.root, "file.txt", b"large original needle");
    commit(&fixture.root);
    let options = WorktreeOptions {
        walk: MetaWalkOptions {
            max_file_size: Some(8),
            ..MetaWalkOptions::default()
        },
        ..WorktreeOptions::default()
    };
    let view = WorktreeView::new(&fixture.root, fixture.generation(), options).unwrap();
    view.refresh_inner(|| write(&fixture.root, "file.txt", b"small"), || {})
        .unwrap();
    assert_eq!(matches(&view, "small"), ["file.txt"]);
    view.refresh_inner(|| write(&fixture.root, "file.txt", b"too large now"), || {})
        .unwrap();
    assert!(
        view.with_snapshot(|snapshot| snapshot.files("", true))
            .unwrap()
            .is_empty()
    );
    view.refresh_inner(|| write(&fixture.root, "file.txt", b"nul\0text"), || {})
        .unwrap();
    assert!(candidates(&view, "nul").is_empty());
    assert_eq!(
        view.with_snapshot(|snapshot| snapshot.files("", true))
            .unwrap(),
        ["file.txt"]
    );
}

#[test]
fn restored_checkpoints_revalidate_contents_membership_and_exact_generation() {
    let fixture = Fixture::new();
    write(&fixture.root, "file.txt", b"base needle");
    commit(&fixture.root);
    let pin = fixture.generation();
    let directory = fixture.root.join("private checkpoint");
    fs::create_dir(&directory).unwrap();
    let options = WorktreeOptions {
        checkpoint_directory: Some(directory.clone()),
        ..WorktreeOptions::default()
    };
    assert!(WorktreeView::restore(&fixture.root, pin.clone(), options.clone()).is_err());
    let view = WorktreeView::new(&fixture.root, pin.clone(), options.clone()).unwrap();
    assert!(matches!(
        view.save_checkpoint(),
        Err(WorktreeError::NotReady)
    ));
    write(&fixture.root, "file.txt", b"saved delta");
    view.refresh().unwrap();
    view.save_checkpoint().unwrap();
    view.save_checkpoint().unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("overlay.json")).unwrap()).unwrap();
    assert_eq!(
        saved["generation"],
        serde_json::to_value(pin.key()).unwrap()
    );
    assert!(saved["overlay"]["files"].as_array().unwrap().len() == 1);
    assert!(!directory.join("index.bin").exists());
    write(&fixture.root, "file.txt", b"current delta");
    write(&fixture.root, "new.txt", b"untracked current");
    let restored = WorktreeView::restore(&fixture.root, pin.clone(), options.clone()).unwrap();
    assert!(matches!(
        restored.with_snapshot(|_| ()),
        Err(WorktreeError::NotReady)
    ));
    restored.refresh().unwrap();
    assert!(matches(&restored, "saved").is_empty());
    assert_eq!(matches(&restored, "current"), ["file.txt", "new.txt"]);
    assert_eq!(
        restored
            .with_snapshot(|snapshot| snapshot.files("", true))
            .unwrap(),
        ["file.txt", "new.txt"]
    );
    assert!(Arc::ptr_eq(restored.generation(), &pin));
    let linked = fixture.linked("wrong root");
    assert!(WorktreeView::restore(&linked, pin.clone(), options.clone()).is_err());
    let different = GenerationManager::with_storage(
        Repository::discover(&fixture.root).unwrap(),
        &fixture.storage,
    )
    .unwrap()
    .ensure(
        "HEAD",
        IndexingProfile {
            max_blob_bytes: None,
            ..IndexingProfile::default()
        },
        None,
    )
    .unwrap()
    .generation;
    assert!(WorktreeView::restore(&fixture.root, different, options.clone()).is_err());
    fs::write(directory.join("overlay.json"), b"invalid json").unwrap();
    assert!(WorktreeView::restore(&fixture.root, pin, options).is_err());
}

#[test]
fn registration_rejects_other_repositories_subdirectories_and_immutable_checkpoint_paths() {
    let fixture = Fixture::new();
    write(&fixture.root, "sub/file.txt", b"needle");
    commit(&fixture.root);
    let pin = fixture.generation();
    let base_bytes: BTreeMap<_, _> = fs::read_dir(pin.directory())
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect();
    let other = Fixture::new();
    commit(&other.root);
    assert!(WorktreeView::new(&other.root, pin.clone(), WorktreeOptions::default()).is_err());
    assert!(
        WorktreeView::new(
            &fixture.root.join("sub"),
            pin.clone(),
            WorktreeOptions::default()
        )
        .is_err()
    );
    for directory in [
        fixture.root.clone(),
        pin.directory().to_path_buf(),
        fixture.root.join(".git"),
    ] {
        assert!(
            WorktreeView::new(
                &fixture.root,
                pin.clone(),
                WorktreeOptions {
                    checkpoint_directory: Some(directory),
                    ..WorktreeOptions::default()
                }
            )
            .is_err()
        );
    }
    let view = WorktreeView::new(
        &fixture.root.join("."),
        pin.clone(),
        WorktreeOptions::default(),
    )
    .unwrap();
    assert_eq!(view.root(), fs::canonicalize(&fixture.root).unwrap());
    assert_eq!(
        view.repository().git_dir(),
        Repository::discover(&fixture.root).unwrap().git_dir()
    );
    let after: BTreeMap<_, _> = fs::read_dir(pin.directory())
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect();
    assert_eq!(
        base_bytes, after,
        "rejected checkpoint destinations cannot modify base bytes"
    );
}

#[test]
fn subtree_hints_and_case_aliases_reverify_unchanged_metadata() {
    let fixture = Fixture::new();
    write(&fixture.root, "dir/one.txt", b"old one");
    write(&fixture.root, "dir/two.txt", b"old two");
    write(&fixture.root, "outside.txt", b"outside");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    view.refresh().unwrap();
    let path = fixture.root.join("dir/one.txt");
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    write(&fixture.root, "dir/one.txt", b"new one");
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    view.invalidate_path(Path::new("DIR")).unwrap();
    let stats = view.refresh().unwrap();
    assert_eq!(stats.files_read, 2);
    assert_eq!(stats.content_reads_avoided, 1);
    assert_eq!(matches(&view, "new one"), ["dir/one.txt"]);
    view.invalidate_path(Path::new("\u{00e9}")).unwrap();
    assert!(view.status().unwrap().full_required);
}

#[test]
fn no_ignore_still_excludes_private_storage_and_larger_view_can_overlay_capped_base() {
    let fixture = Fixture::new();
    write(&fixture.root, "source.txt", b"larger than base cap needle");
    write(&fixture.root, ".gitignore", b"ignored.txt\n");
    commit(&fixture.root);
    let pin = GenerationManager::with_storage(
        Repository::discover(&fixture.root).unwrap(),
        &fixture.storage,
    )
    .unwrap()
    .ensure(
        "HEAD",
        IndexingProfile {
            max_blob_bytes: Some(3),
            ..IndexingProfile::default()
        },
        None,
    )
    .unwrap()
    .generation;
    write(&fixture.root, "ignored.txt", b"ignored needle");
    for path in [
        ".tgrep/.flush-staging/private.txt",
        "private/.stage/private.txt",
        "checkpoint/.temporary.txt",
    ] {
        write(&fixture.root, path, b"storage needle");
    }
    let view = WorktreeView::new(
        &fixture.root,
        pin,
        WorktreeOptions {
            walk: MetaWalkOptions {
                no_ignore: true,
                max_file_size: None,
                exclude_paths: vec![PathBuf::from("private")],
                ..MetaWalkOptions::default()
            },
            checkpoint_directory: Some(PathBuf::from("checkpoint")),
            ..WorktreeOptions::default()
        },
    )
    .unwrap();
    view.refresh().unwrap();
    assert_eq!(matches(&view, "needle"), ["ignored.txt", "source.txt"]);
    assert!(
        !view
            .with_snapshot(|snapshot| snapshot.files("", true))
            .unwrap()
            .iter()
            .any(|path| path.starts_with(".git/")
                || path.starts_with(".tgrep/")
                || path.starts_with("private/")
                || path.starts_with("checkpoint/"))
    );
    assert_scan_parity(&view, &["needle", "storage", ".*"]);
}

#[cfg(windows)]
#[test]
fn windows_hidden_attributes_and_unreadable_files_invalidate_correctly() {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, SetFileAttributesW,
    };
    let fixture = Fixture::new();
    write(&fixture.root, "source.txt", b"hidden attribute needle");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    view.refresh().unwrap();
    let path = fixture.root.join("source.txt");
    let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // The NUL-terminated buffer remains live for the Win32 call.
    assert_ne!(
        unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) },
        0
    );
    view.invalidate_path(Path::new("source.txt")).unwrap();
    view.refresh().unwrap();
    assert!(
        view.with_snapshot(|snapshot| snapshot.files("", false))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        view.with_snapshot(|snapshot| snapshot.files("", true))
            .unwrap(),
        ["source.txt"]
    );
    assert_ne!(
        unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_NORMAL) },
        0
    );
    let exclusive = File::options().read(true).share_mode(0).open(path).unwrap();
    assert!(view.refresh().is_err());
    assert!(!view.status().unwrap().ready);
    drop(exclusive);
    view.refresh().unwrap();
    assert_eq!(
        view.with_snapshot(|snapshot| snapshot.files("", false))
            .unwrap(),
        ["source.txt"]
    );
}

#[test]
fn case_insensitive_ignore_snapshot_tracks_index_membership_coherently() {
    let fixture = Fixture::new();
    write(&fixture.root, "TRACKED.txt", b"tracked needle");
    commit(&fixture.root);
    let pin = fixture.generation();
    git(&fixture.root, &["config", "core.ignorecase", "true"]);
    write(&fixture.root, ".gitignore", b"tracked.txt\nuntracked.txt\n");
    write(&fixture.root, "UNTRACKED.txt", b"untracked needle");
    let view = fixture.view(&fixture.root, &pin);
    view.refresh().unwrap();
    assert_eq!(matches(&view, "needle"), ["TRACKED.txt"]);
    git(&fixture.root, &["rm", "--cached", "TRACKED.txt"]);
    view.invalidate_all().unwrap();
    view.refresh().unwrap();
    assert!(matches(&view, "needle").is_empty());
    fs::write(view.repository().git_dir().join("index"), b"broken index").unwrap();
    assert!(view.refresh().is_err());
    assert!(!view.status().unwrap().ready);
}

#[cfg(unix)]
#[test]
fn symlinks_executable_modes_and_materialized_gitlinks_match_walker() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    write(&fixture.root, "source.txt", b"source needle");
    write(&fixture.root, "executable.sh", b"executable needle");
    fs::set_permissions(
        fixture.root.join("executable.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("source.txt", fixture.root.join("link.txt")).unwrap();
    commit(&fixture.root);
    let oid = git(&fixture.root, &["rev-parse", "HEAD"]);
    git(
        &fixture.root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{oid},module"),
        ],
    );
    git(&fixture.root, &["commit", "--quiet", "-m", "gitlink"]);
    let pin = fixture.generation();
    write(&fixture.root, "module/local.txt", b"materialized needle");
    fs::remove_file(fixture.root.join("source.txt")).unwrap();
    symlink("executable.sh", fixture.root.join("source.txt")).unwrap();
    fs::remove_file(fixture.root.join("link.txt")).unwrap();
    write(&fixture.root, "link.txt", b"now regular needle");
    let view = fixture.view(&fixture.root, &pin);
    view.refresh().unwrap();
    assert!(!candidates(&view, "source").contains(&"source.txt".into()));
    assert_eq!(matches(&view, "now regular"), ["link.txt"]);
    assert_scan_parity(
        &view,
        &["needle", "source", "executable", "materialized", ".*"],
    );
}
