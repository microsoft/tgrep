use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tgrep_core::generations::{
    EntryContent, EntryMode, Generation, GenerationError, GenerationManager, IndexingProfile,
    Repository, RetentionPolicy,
};
use tgrep_core::{query, trigram};

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
    command.arg("-C").arg(root).args([
        "-c",
        "user.name=Generation Tests",
        "-c",
        "user.email=generations@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.autocrlf=false",
    ]);
    command
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = command(root).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn write(root: &Path, name: &str, bytes: &[u8]) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

struct Fixture {
    _temp: TempDir,
    repo: PathBuf,
    storage: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("tgrep generation spaces ")
            .tempdir()
            .unwrap();
        let repo = temp.path().join("repository with spaces");
        let storage = temp.path().join("shared storage");
        fs::create_dir(&repo).unwrap();
        fs::create_dir(&storage).unwrap();
        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        Self {
            _temp: temp,
            repo,
            storage,
        }
    }

    fn commit(&self) -> String {
        git(&self.repo, &["add", "--all"]);
        git(
            &self.repo,
            &["commit", "--quiet", "--allow-empty", "-m", "fixture"],
        );
        git(&self.repo, &["rev-parse", "HEAD"])
    }

    fn manager(&self) -> GenerationManager {
        GenerationManager::with_storage(Repository::discover(&self.repo).unwrap(), &self.storage)
            .unwrap()
    }
}

fn candidates(generation: &Generation, pattern: &str) -> Vec<String> {
    let root = tempfile::tempdir().unwrap();
    let view = generation.base().create_worktree(root.path()).unwrap();
    let plan = query::build_query_plan(pattern, false).unwrap();
    let (ids, reader) = view.execute_query_with_masks(&plan);
    let mut paths: Vec<_> = ids
        .into_iter()
        .map(|id| view.resolve_path(id, &reader).unwrap())
        .collect();
    paths.sort_unstable();
    paths
}

#[test]
fn linked_worktrees_and_equal_trees_share_one_arc() {
    let fixture = Fixture::new();
    write(
        &fixture.repo,
        "folder with spaces/source.txt",
        b"committed needle",
    );
    let first_commit = fixture.commit();
    let linked = fixture._temp.path().join("linked worktree with spaces");
    let output = command(&fixture.repo)
        .args(["worktree", "add", "--detach"])
        .arg(&linked)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let repository = Repository::discover(&fixture.repo).unwrap();
    assert_eq!(repository, Repository::discover(&linked).unwrap());
    assert_eq!(
        repository,
        Repository::discover(&fixture.repo.join("folder with spaces")).unwrap()
    );
    let manager = fixture.manager();
    let first = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap();
    assert!(first.stats.published);
    let second =
        GenerationManager::with_storage(Repository::discover(&linked).unwrap(), &fixture.storage)
            .unwrap()
            .ensure("HEAD", IndexingProfile::default(), None)
            .unwrap();
    assert!(Arc::ptr_eq(&first.generation, &second.generation));
    assert!(Arc::ptr_eq(
        first.generation.base(),
        second.generation.base()
    ));
    assert_eq!(second.stats.blobs_extracted, 0);
    assert!(second.stats.reused_generation);

    let second_commit = fixture.commit();
    assert_ne!(first_commit, second_commit);
    let same_tree = manager
        .ensure(&second_commit, IndexingProfile::default(), None)
        .unwrap();
    assert!(Arc::ptr_eq(&same_tree.generation, &first.generation));
    assert_eq!(same_tree.requested_commit, second_commit);
    assert_eq!(same_tree.generation.commit_oid(), first_commit);
    assert_eq!(manager.list().unwrap(), [first.generation.key().clone()]);
    let default_manager = GenerationManager::new(repository.clone()).unwrap();
    assert!(
        default_manager
            .directory()
            .starts_with(repository.common_dir())
    );
    assert!(!default_manager.directory().starts_with(&linked));
    assert_eq!(default_manager.retention(), RetentionPolicy::RetainAll);

    write(&linked, "linked.txt", b"linked HEAD content");
    git(&linked, &["add", "--all"]);
    git(&linked, &["commit", "--quiet", "-m", "linked divergence"]);
    let linked_manager =
        GenerationManager::with_storage(Repository::discover(&linked).unwrap(), &fixture.storage)
            .unwrap();
    let linked_head = linked_manager
        .ensure("HEAD", IndexingProfile::default(), Some(&first.generation))
        .unwrap();
    assert_eq!(
        candidates(&linked_head.generation, "linked HEAD"),
        ["linked.txt"]
    );
    assert_ne!(linked_head.generation.key(), first.generation.key());
    assert_eq!(
        manager
            .ensure("HEAD", IndexingProfile::default(), None)
            .unwrap()
            .generation
            .key(),
        first.generation.key()
    );
}

#[test]
fn clone_tree_and_profile_keys_do_not_collide() {
    let fixture = Fixture::new();
    write(&fixture.repo, "source.txt", b"first generation");
    fixture.commit();
    let clone = fixture._temp.path().join("independent clone");
    let output = command(fixture._temp.path())
        .arg("clone")
        .arg("--quiet")
        .arg(&fixture.repo)
        .arg(&clone)
        .output()
        .unwrap();
    assert!(output.status.success());
    let manager = fixture.manager();
    let first = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    let other =
        GenerationManager::with_storage(Repository::discover(&clone).unwrap(), &fixture.storage)
            .unwrap()
            .ensure("HEAD", IndexingProfile::default(), None)
            .unwrap()
            .generation;
    assert_eq!(first.key().tree_oid(), other.key().tree_oid());
    assert_ne!(first.key(), other.key());
    assert_ne!(first.directory(), other.directory());
    assert!(matches!(
        manager.open(other.key()),
        Err(GenerationError::Incompatible(_))
    ));

    let small = IndexingProfile {
        max_blob_bytes: Some(3),
        ..IndexingProfile::default()
    };
    let capped = manager.ensure("HEAD", small, None).unwrap().generation;
    assert_ne!(capped.key(), first.key());
    assert_eq!(
        capped.entry("source.txt").unwrap().content,
        EntryContent::TooLarge
    );
    assert!(candidates(&capped, "first").is_empty());
    assert!(matches!(
        manager.ensure("HEAD", small, Some(&first)),
        Err(GenerationError::Incompatible(_))
    ));
    write(&fixture.repo, "source.txt", b"second generation");
    fixture.commit();
    let changed = manager
        .ensure("HEAD", IndexingProfile::default(), Some(&first))
        .unwrap();
    assert_ne!(changed.generation.key(), first.key());
    assert_eq!(changed.stats.blobs_extracted, 1);
}

#[test]
fn builds_committed_blobs_not_staged_dirty_or_untracked_contents() {
    let fixture = Fixture::new();
    write(&fixture.repo, "source.txt", b"committed content");
    write(&fixture.repo, ".hidden", b"hidden committed");
    write(&fixture.repo, "asset.bin", b"text despite extension");
    write(&fixture.repo, ".gitignore", b"ignored.txt\n");
    write(&fixture.repo, "ignored.txt", b"tracked ignored content");
    git(&fixture.repo, &["add", "-f", "ignored.txt"]);
    fixture.commit();
    write(&fixture.repo, "source.txt", b"staged content");
    write(&fixture.repo, "staged-new.txt", b"staged new content");
    git(&fixture.repo, &["add", "--all"]);
    write(&fixture.repo, "source.txt", b"dirty content");
    write(&fixture.repo, "untracked.txt", b"untracked content");
    let generation = fixture
        .manager()
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    assert_eq!(candidates(&generation, "committed content"), ["source.txt"]);
    for pattern in ["staged content", "dirty content", "untracked content"] {
        assert!(candidates(&generation, pattern).is_empty(), "{pattern}");
    }
    assert!(generation.entry("staged-new.txt").is_none());
    assert!(generation.entry("untracked.txt").is_none());
    assert!(generation.entry(".hidden").unwrap().content_id().is_some());
    assert!(
        generation
            .entry("ignored.txt")
            .unwrap()
            .content_id()
            .is_some()
    );
    assert!(
        generation
            .entry("asset.bin")
            .unwrap()
            .content_id()
            .is_some()
    );
}

#[test]
fn incremental_reuses_masks_renames_and_copies_extracting_only_new_blobs() {
    let fixture = Fixture::new();
    let unchanged = b"unchanged long content with masks and several interesting trigrams";
    write(&fixture.repo, "keep.txt", unchanged);
    write(&fixture.repo, "rename.bin", b"renamed text content");
    write(&fixture.repo, "change.txt", b"before change");
    write(&fixture.repo, "delete.txt", b"deleted content");
    fixture.commit();
    let manager = fixture.manager();
    let previous = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    fs::rename(
        fixture.repo.join("rename.bin"),
        fixture.repo.join("renamed.txt"),
    )
    .unwrap();
    write(&fixture.repo, "copy.bin", unchanged);
    write(&fixture.repo, "change.txt", b"after change");
    write(&fixture.repo, "new.txt", b"brand new content");
    fs::remove_file(fixture.repo.join("delete.txt")).unwrap();
    fixture.commit();
    let next = manager
        .ensure("HEAD", IndexingProfile::default(), Some(&previous))
        .unwrap();
    assert_eq!(next.stats.blobs_read, 2);
    assert_eq!(next.stats.blobs_extracted, 2);
    assert_eq!(next.stats.reused_indexed_files, 3);
    assert!(next.stats.postings_reused > 0);
    assert_eq!(
        next.stats.predecessor_posting_lists_read,
        previous.base().reader().num_trigrams() as u64
    );
    assert_eq!(
        next.stats.blob_bytes_read,
        (b"after change".len() + b"brand new content".len()) as u64
    );
    assert_eq!(
        candidates(&next.generation, "unchanged"),
        ["copy.bin", "keep.txt"]
    );
    assert_eq!(
        candidates(&next.generation, "renamed text"),
        ["renamed.txt"]
    );
    assert!(next.generation.entry("delete.txt").is_none());
    assert_eq!(candidates(&previous, "before change"), ["change.txt"]);
    for (tri, masks) in trigram::extract_merged_masks(unchanged) {
        let reader = next.generation.base().reader();
        for posting in reader.lookup_trigram_with_masks(tri) {
            if matches!(
                reader.file_path(posting.file_id),
                Some("keep.txt" | "copy.bin")
            ) {
                assert_eq!(posting.loc_mask, masks.loc_mask);
                assert_eq!(posting.next_mask, masks.next_mask);
            }
        }
    }
    let reopened = manager.open(previous.key()).unwrap();
    assert!(Arc::ptr_eq(&previous, &reopened));
    let full_storage = fixture._temp.path().join("independent full build");
    fs::create_dir(&full_storage).unwrap();
    let full = GenerationManager::with_storage(
        Repository::discover(&fixture.repo).unwrap(),
        &full_storage,
    )
    .unwrap()
    .ensure("HEAD", IndexingProfile::default(), None)
    .unwrap();
    assert_eq!(full.stats.blobs_extracted, 4);
    assert_eq!(
        full.generation.base().snapshot_id(),
        next.generation.base().snapshot_id()
    );
}

#[test]
fn raw_blob_semantics_require_decoded_identity_not_git_cleanliness() {
    let fixture = Fixture::new();
    write(&fixture.repo, ".gitattributes",
        b"crlf.txt text eol=crlf\nfiltered.txt filter=demo\nencoded.txt working-tree-encoding=UTF-16LE\n");
    write(&fixture.repo, "crlf.txt", b"first line\nsecond line\n");
    write(&fixture.repo, "filtered.txt", b"raw filter input\n");
    write(&fixture.repo, "utf16.txt", b"\xff\xfeh\0e\0l\0l\0o\0");
    write(&fixture.repo, "binary.txt", b"binary\0contents");
    write(&fixture.repo, "empty.txt", b"");
    write(&fixture.repo, "short.txt", b"x");
    write(&fixture.repo, "encoded.txt", b"e\0n\0c\0o\0d\0e\0d\0");
    fixture.commit();
    fs::remove_file(fixture.repo.join("crlf.txt")).unwrap();
    git(&fixture.repo, &["checkout", "--", "crlf.txt"]);
    assert_eq!(
        fs::read(fixture.repo.join("crlf.txt")).unwrap(),
        b"first line\r\nsecond line\r\n"
    );
    assert!(git(&fixture.repo, &["status", "--porcelain"]).is_empty());
    // Base construction must not invoke configured checkout transformations.
    git(
        &fixture.repo,
        &[
            "config",
            "filter.demo.smudge",
            "tgrep-missing-filter-command",
        ],
    );
    let generation = fixture
        .manager()
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    assert!(
        generation
            .entry("crlf.txt")
            .unwrap()
            .matches_worktree_bytes(b"first line\nsecond line\n")
    );
    assert!(
        !generation
            .entry("crlf.txt")
            .unwrap()
            .matches_worktree_bytes(b"first line\r\nsecond line\r\n")
    );
    assert!(
        !generation
            .entry("filtered.txt")
            .unwrap()
            .matches_worktree_bytes(b"smudged filter output\n")
    );
    assert!(
        generation
            .entry("utf16.txt")
            .unwrap()
            .matches_worktree_bytes(b"hello")
    );
    assert_eq!(candidates(&generation, "hello"), ["utf16.txt"]);
    assert_eq!(
        candidates(&generation, "encoded"),
        [".gitattributes", "encoded.txt"]
    );
    assert!(
        !generation
            .entry("encoded.txt")
            .unwrap()
            .matches_worktree_bytes(&fs::read(fixture.repo.join("encoded.txt")).unwrap())
    );
    assert_eq!(
        generation.entry("binary.txt").unwrap().content,
        EntryContent::Binary
    );
    for name in ["empty.txt", "short.txt"] {
        assert!(generation.entry(name).unwrap().content_id().is_some());
        assert!(generation.base().reader().contains_path(name));
    }
    assert!(!generation.base().reader().contains_path("binary.txt"));
}

#[test]
fn same_blob_destination_mode_is_reclassified() {
    let fixture = Fixture::new();
    write(&fixture.repo, "asset.bin", b"identical blob content");
    fixture.commit();
    let manager = fixture.manager();
    let original = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    let oid = original.entry("asset.bin").unwrap().oid.clone();
    git(
        &fixture.repo,
        &[
            "update-index",
            "--cacheinfo",
            &format!("120000,{oid},asset.bin"),
        ],
    );
    git(
        &fixture.repo,
        &["commit", "--quiet", "-m", "regular to symlink"],
    );
    let symlink = manager
        .ensure("HEAD", IndexingProfile::default(), Some(&original))
        .unwrap();
    assert_eq!(symlink.stats.blobs_extracted, 0);
    assert_eq!(
        symlink.generation.entry("asset.bin").unwrap().mode,
        EntryMode::Symlink
    );
    assert_eq!(
        symlink.generation.entry("asset.bin").unwrap().content,
        EntryContent::NotRegular
    );
    assert!(
        !symlink
            .generation
            .base()
            .reader()
            .contains_path("asset.bin")
    );
    git(
        &fixture.repo,
        &["update-index", "--force-remove", "asset.bin"],
    );
    git(
        &fixture.repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{oid},source.txt"),
        ],
    );
    git(
        &fixture.repo,
        &["commit", "--quiet", "-m", "symlink to regular new path"],
    );
    let text = manager
        .ensure(
            "HEAD",
            IndexingProfile::default(),
            Some(&symlink.generation),
        )
        .unwrap();
    assert_eq!(text.stats.blobs_extracted, 1);
    assert_eq!(candidates(&text.generation, "identical"), ["source.txt"]);
}

#[test]
fn empty_tree_is_a_valid_complete_generation() {
    let fixture = Fixture::new();
    fixture.commit();
    let manager = fixture.manager();
    let result = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap();
    assert_eq!(result.stats.blobs_read, 0);
    assert!(result.generation.entries().is_empty());
    assert_eq!(result.generation.base().reader().num_files(), 0);
    assert_eq!(manager.list().unwrap(), [result.generation.key().clone()]);
}

#[test]
fn simultaneous_managers_share_publication_and_reader() {
    let fixture = Fixture::new();
    write(&fixture.repo, "source.txt", b"simultaneous generation");
    fixture.commit();
    let barrier = Arc::new(Barrier::new(6));
    let threads: Vec<_> = (0..6)
        .map(|_| {
            let manager = fixture.manager();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                manager
                    .ensure("HEAD", IndexingProfile::default(), None)
                    .unwrap()
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(
        results
            .iter()
            .filter(|result| result.stats.published)
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .map(|result| result.stats.blobs_extracted)
            .sum::<u64>(),
        1
    );
    for result in &results[1..] {
        assert!(Arc::ptr_eq(&results[0].generation, &result.generation));
        assert!(Arc::ptr_eq(
            results[0].generation.base().reader(),
            result.generation.base().reader()
        ));
    }
}

#[test]
fn process_worker() {
    let Some(root) = std::env::var_os("TGREP_GENERATION_TEST_ROOT") else {
        return;
    };
    let storage = PathBuf::from(std::env::var_os("TGREP_GENERATION_TEST_STORAGE").unwrap());
    let result = PathBuf::from(std::env::var_os("TGREP_GENERATION_TEST_RESULT").unwrap());
    let gate = PathBuf::from(std::env::var_os("TGREP_GENERATION_TEST_GATE").unwrap());
    if std::env::var_os("TGREP_GENERATION_TEST_INTERRUPT").is_some() {
        let manager = GenerationManager::with_storage(
            Repository::discover(Path::new(&root)).unwrap(),
            &storage,
        )
        .unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(manager.directory().join("publication.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();
        let stage = manager.directory().join(".stage-killed-process");
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("index.bin"), b"partial bytes").unwrap();
        fs::write(result.with_extension("ready"), b"ready").unwrap();
        thread::sleep(Duration::from_secs(30));
        return;
    }
    fs::write(result.with_extension("ready"), b"ready").unwrap();
    let start = Instant::now();
    while !gate.exists() {
        assert!(start.elapsed() < Duration::from_secs(30));
        thread::sleep(Duration::from_millis(10));
    }
    let manager =
        GenerationManager::with_storage(Repository::discover(Path::new(&root)).unwrap(), &storage)
            .unwrap();
    let built = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap();
    fs::write(
        result,
        format!("{} {}", built.stats.published, built.stats.blobs_extracted),
    )
    .unwrap();
}

#[test]
fn simultaneous_processes_deduplicate_builds() {
    let fixture = Fixture::new();
    write(&fixture.repo, "source.txt", b"cross process generation");
    fixture.commit();
    let gate = fixture._temp.path().join("start");
    let mut children = Vec::new();
    let mut outputs = Vec::new();
    for index in 0..4 {
        let output = fixture._temp.path().join(format!("result-{index}"));
        children.push(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "process_worker", "--nocapture"])
                .env("TGREP_GENERATION_TEST_ROOT", &fixture.repo)
                .env("TGREP_GENERATION_TEST_STORAGE", &fixture.storage)
                .env("TGREP_GENERATION_TEST_RESULT", &output)
                .env("TGREP_GENERATION_TEST_GATE", &gate)
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        outputs.push(output);
    }

    let start = Instant::now();
    while !outputs
        .iter()
        .all(|path| path.with_extension("ready").exists())
    {
        assert!(start.elapsed() < Duration::from_secs(30));
        thread::sleep(Duration::from_millis(10));
    }
    fs::write(gate, b"go").unwrap();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let outputs: Vec<_> = outputs
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect();
    assert_eq!(
        outputs.iter().filter(|output| *output == "true 1").count(),
        1
    );
    assert_eq!(
        outputs.iter().filter(|output| *output == "false 0").count(),
        3
    );
    assert_eq!(fixture.manager().list().unwrap().len(), 1);
}

#[test]
fn killed_starter_releases_lock_and_leaves_only_undiscoverable_staging() {
    let fixture = Fixture::new();
    write(&fixture.repo, "source.txt", b"surviving crash");
    fixture.commit();
    let result = fixture._temp.path().join("interrupted");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process_worker", "--nocapture"])
        .env("TGREP_GENERATION_TEST_ROOT", &fixture.repo)
        .env("TGREP_GENERATION_TEST_STORAGE", &fixture.storage)
        .env("TGREP_GENERATION_TEST_RESULT", &result)
        .env(
            "TGREP_GENERATION_TEST_GATE",
            fixture._temp.path().join("unused"),
        )
        .env("TGREP_GENERATION_TEST_INTERRUPT", "1")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !result.with_extension("ready").exists() {
        assert!(start.elapsed() < Duration::from_secs(20));
        thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let manager = fixture.manager();
    assert!(manager.list().unwrap().is_empty());
    let generation = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    assert_eq!(candidates(&generation, "surviving"), ["source.txt"]);
}

#[test]
fn retained_views_and_checkpoints_outlive_managers_and_new_generations() {
    let fixture = Fixture::new();
    write(&fixture.repo, "source.txt", b"old generation");
    fixture.commit();
    let manager = fixture.manager();
    let previous = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    let key = previous.key().clone();
    let old_directory = previous.directory().to_path_buf();
    let checkpoint = fixture._temp.path().join("overlay.json");
    let mut view = previous.base().create_worktree(&fixture.repo).unwrap();
    view.live.upsert_file("private.txt", b"private overlay");
    previous.base().save_overlay(&view, &checkpoint).unwrap();
    write(&fixture.repo, "source.txt", b"new generation");
    fixture.commit();
    let next = manager
        .ensure("HEAD", IndexingProfile::default(), Some(&previous))
        .unwrap()
        .generation;
    assert_ne!(next.key(), &key);
    drop(previous);
    drop(manager);
    assert!(view.reader_arc().contains_path("source.txt"));
    assert!(old_directory.is_dir());
    let reopened = fixture.manager().open(&key).unwrap();
    let restored = reopened
        .base()
        .restore_worktree(&fixture.repo, &checkpoint)
        .unwrap();
    assert_eq!(restored.live.num_files(), 1);
    assert_eq!(candidates(&reopened, "old generation"), ["source.txt"]);
}

#[test]
fn interrupted_and_failed_publications_are_not_discoverable() {
    let fixture = Fixture::new();
    write(&fixture.repo, "source.txt", b"published content");
    fixture.commit();
    let manager = fixture.manager();
    let abandoned = manager.directory().join(".stage-interrupted");
    fs::create_dir(&abandoned).unwrap();
    fs::write(abandoned.join("index.bin"), b"incomplete").unwrap();
    fs::write(abandoned.join("generation.json"), b"{").unwrap();
    assert!(manager.list().unwrap().is_empty());
    assert!(matches!(
        manager.ensure("--help", IndexingProfile::default(), None),
        Err(GenerationError::Git { .. })
    ));
    assert!(manager.list().unwrap().is_empty());
    let first = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    let bytes = fs::read(first.directory().join("index.bin")).unwrap();
    write(&fixture.repo, "source.txt", b"next content");
    fixture.commit();
    // An unreadable lock is an error, not an unlocked publication fallback.
    fs::remove_file(manager.directory().join("publication.lock")).unwrap();
    fs::create_dir(manager.directory().join("publication.lock")).unwrap();
    assert!(
        manager
            .ensure("HEAD", IndexingProfile::default(), Some(&first))
            .is_err()
    );
    assert_eq!(
        fs::read(first.directory().join("index.bin")).unwrap(),
        bytes
    );
}

#[test]
fn malformed_metadata_is_rejected_without_overwriting_generation() {
    let fixture = Fixture::new();
    write(&fixture.repo, "source.txt", b"valid content");
    fixture.commit();
    let manager = fixture.manager();
    let generation = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    let key = generation.key().clone();
    let manifest = generation.directory().join("generation.json");
    let original = fs::read(&manifest).unwrap();
    drop(generation);
    let mut json: serde_json::Value = serde_json::from_slice(&original).unwrap();
    json["manifest"]["entries"][0]["oid"] = serde_json::json!("0".repeat(40));
    fs::write(&manifest, serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(matches!(
        manager.open(&key),
        Err(GenerationError::InvalidMetadata(_))
    ));
    let invalid = fs::read(&manifest).unwrap();
    assert!(
        manager
            .ensure("HEAD", IndexingProfile::default(), None)
            .is_err()
    );
    assert_eq!(fs::read(&manifest).unwrap(), invalid);
    assert!(manager.list().is_err());
    fs::write(&manifest, original).unwrap();
    let generation = manager.open(&key).unwrap();
    assert_eq!(candidates(&generation, "valid"), ["source.txt"]);
}

#[test]
fn replace_refs_do_not_change_exact_committed_tree() {
    let fixture = Fixture::new();
    write(&fixture.repo, "source.txt", b"original committed content");
    let original = fixture.commit();
    write(
        &fixture.repo,
        "source.txt",
        b"replacement committed content",
    );
    let replacement = fixture.commit();
    git(&fixture.repo, &["replace", &original, &replacement]);
    let generation = fixture
        .manager()
        .ensure(&original, IndexingProfile::default(), None)
        .unwrap()
        .generation;
    assert_eq!(candidates(&generation, "original"), ["source.txt"]);
    assert!(candidates(&generation, "replacement").is_empty());
}

#[test]
fn gitlinks_are_membership_only_and_missing_blobs_are_errors() {
    let fixture = Fixture::new();
    write(&fixture.repo, "source.txt", b"blob about to disappear");
    let commit = fixture.commit();
    git(
        &fixture.repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{commit},submodule"),
        ],
    );
    git(&fixture.repo, &["commit", "--quiet", "-m", "gitlink"]);
    let manager = fixture.manager();
    let generation = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    let entry = generation.entry("submodule").unwrap();
    assert_eq!(entry.mode, EntryMode::Gitlink);
    assert_eq!(entry.content, EntryContent::NotRegular);
    assert_eq!(entry.size, None);
    assert!(!generation.base().reader().contains_path("submodule"));
    write(&fixture.repo, "new.txt", b"new missing object");
    fixture.commit();
    let oid = git(&fixture.repo, &["rev-parse", "HEAD:new.txt"]);
    let object = fixture
        .repo
        .join(".git")
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..]);
    #[cfg(windows)]
    #[allow(clippy::permissions_set_readonly_false)]
    {
        // Windows Git marks loose objects read-only; Unix unlink needs no chmod.
        let mut writable = fs::metadata(&object).unwrap().permissions();
        writable.set_readonly(false);
        fs::set_permissions(&object, writable).unwrap();
    }
    fs::remove_file(object).unwrap();
    assert!(
        manager
            .ensure("HEAD", IndexingProfile::default(), Some(&generation))
            .is_err()
    );
    assert_eq!(manager.list().unwrap(), [generation.key().clone()]);
    assert_eq!(candidates(&generation, "disappear"), ["source.txt"]);
}

#[test]
fn unsupported_storage_and_discovery_return_errors() {
    let fixture = Fixture::new();
    let nonrepo = tempfile::tempdir().unwrap();
    assert!(matches!(
        Repository::discover(nonrepo.path()),
        Err(GenerationError::Git { .. })
    ));
    let repository = Repository::discover(&fixture.repo).unwrap();
    assert!(GenerationManager::with_storage(repository.clone(), &fixture.repo).is_err());
    assert!(GenerationManager::with_storage(repository.clone(), repository.common_dir()).is_err());
    let file = fixture._temp.path().join("not a storage directory");
    fs::write(&file, b"file").unwrap();
    assert!(GenerationManager::with_storage(repository, &file).is_err());
    // An unborn repository is not treated as an empty committed tree.
    assert!(
        fixture
            .manager()
            .ensure("HEAD", IndexingProfile::default(), None)
            .is_err()
    );
}

#[test]
fn effective_storage_cannot_be_a_linked_worktree() {
    let fixture = Fixture::new();
    write(&fixture.repo, "source.txt", b"original worktree content");
    fixture.commit();
    let repository = Repository::discover(&fixture.repo).unwrap();
    let effective = fixture.storage.join(repository.identity());
    let output = command(&fixture.repo)
        .args(["worktree", "add", "--detach"])
        .arg(&effective)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(matches!(
        GenerationManager::with_storage(repository.clone(), &fixture.storage),
        Err(GenerationError::Unsupported(_))
    ));
    assert!(!effective.join("publication.lock").exists());
    assert!(git(&effective, &["status", "--porcelain"]).is_empty());
    let manager = GenerationManager::new(repository.clone()).unwrap();
    let generation = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    assert!(generation.directory().starts_with(repository.common_dir()));
    assert!(
        !generation
            .directory()
            .starts_with(fs::canonicalize(effective).unwrap())
    );
}

#[test]
fn effective_storage_cannot_be_an_existing_snapshot() {
    let fixture = Fixture::new();
    fixture.commit();
    let repository = Repository::discover(&fixture.repo).unwrap();
    let effective = fixture.storage.join(repository.identity());
    fs::create_dir(&effective).unwrap();
    tgrep_core::builder::write_index_from_snapshot(
        &fixture.repo,
        &effective,
        &[],
        &std::collections::HashMap::new(),
        true,
    )
    .unwrap();
    let mut meta = tgrep_core::meta::IndexMeta::load(&effective).unwrap();
    meta.hidden_complete = true;
    meta.save(&effective).unwrap();
    let base = tgrep_core::shared::SharedBase::open(&effective).unwrap();
    assert!(matches!(
        GenerationManager::with_storage(repository, &fixture.storage),
        Err(GenerationError::Unsupported(_))
    ));
    assert!(!effective.join("publication.lock").exists());
    assert_eq!(
        tgrep_core::shared::SharedBase::open(&effective)
            .unwrap()
            .snapshot_id(),
        base.snapshot_id()
    );
}

#[test]
fn unchanged_binary_or_empty_destination_does_not_scan_predecessor_postings() {
    let fixture = Fixture::new();
    write(
        &fixture.repo,
        "source.txt",
        b"previous indexed contents with many trigrams",
    );
    write(&fixture.repo, "binary.txt", b"unchanged\0binary");
    fixture.commit();
    let manager = fixture.manager();
    let original = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    assert!(original.base().reader().num_trigrams() > 0);
    write(
        &fixture.repo,
        "source.txt",
        b"entirely different indexed data",
    );
    fixture.commit();
    let changed = manager
        .ensure("HEAD", IndexingProfile::default(), Some(&original))
        .unwrap();
    assert_eq!(changed.stats.blobs_extracted, 1);
    assert_eq!(changed.stats.reused_indexed_files, 0);
    assert_eq!(changed.stats.postings_reused, 0);
    assert_eq!(changed.stats.predecessor_posting_lists_read, 0);
    assert_eq!(candidates(&changed.generation, "entirely"), ["source.txt"]);
    assert_eq!(
        changed.generation.entry("binary.txt").unwrap().content,
        EntryContent::Binary
    );

    fs::remove_file(fixture.repo.join("source.txt")).unwrap();
    fs::remove_file(fixture.repo.join("binary.txt")).unwrap();
    fixture.commit();
    let empty = manager
        .ensure("HEAD", IndexingProfile::default(), Some(&original))
        .unwrap();
    assert!(empty.generation.entries().is_empty());
    assert_eq!(empty.stats.blobs_extracted, 0);
    assert_eq!(empty.stats.reused_indexed_files, 0);
    assert_eq!(empty.stats.postings_reused, 0);
    assert_eq!(empty.stats.predecessor_posting_lists_read, 0);
}

#[test]
fn posting_free_indexed_files_reuse_without_predecessor_scan() {
    let fixture = Fixture::new();
    let short_files: &[(&str, &[u8])] = &[
        ("empty.txt", b""),
        ("one.txt", b"a"),
        ("two.txt", b"ab"),
        ("bom-empty.txt", b"\xef\xbb\xbf"),
        ("bom-two.txt", b"\xef\xbb\xbfab"),
        ("utf16-one.txt", b"\xff\xfea\0"),
        ("utf16-two.txt", b"\xff\xfea\0b\0"),
    ];
    for &(path, bytes) in short_files {
        write(&fixture.repo, path, bytes);
    }
    write(
        &fixture.repo,
        "source.txt",
        b"original indexed text with postings",
    );
    fixture.commit();
    let manager = fixture.manager();
    let original = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    assert!(original.base().reader().num_trigrams() > 0);
    let key = original.key().clone();
    let identities: Vec<_> = short_files
        .iter()
        .map(|&(path, _)| original.entry(path).unwrap().content_id().unwrap())
        .collect();
    drop(original);
    // Reopening an existing generation must recover posting presence without new metadata.
    let original = manager.open(&key).unwrap();

    for replacement in [Some(b"replacement indexed content".as_slice()), None] {
        if let Some(bytes) = replacement {
            write(&fixture.repo, "source.txt", bytes);
        } else {
            fs::remove_file(fixture.repo.join("source.txt")).unwrap();
        }
        fixture.commit();
        let next = manager
            .ensure("HEAD", IndexingProfile::default(), Some(&original))
            .unwrap();
        assert_eq!(next.stats.blobs_read, u64::from(replacement.is_some()));
        assert_eq!(next.stats.blobs_extracted, u64::from(replacement.is_some()));
        assert_eq!(next.stats.reused_indexed_files, short_files.len() as u64);
        assert_eq!(next.stats.postings_reused, 0);
        assert_eq!(next.stats.predecessor_posting_lists_read, 0);
        for (&(path, bytes), &identity) in short_files.iter().zip(&identities) {
            let entry = next.generation.entry(path).unwrap();
            assert_eq!(entry.content_id(), Some(identity));
            assert!(entry.matches_worktree_bytes(bytes));
            assert!(next.generation.base().reader().contains_path(path));
        }
        assert_eq!(candidates(&next.generation, "a"), {
            let mut paths: Vec<_> = next.generation.base().reader().all_paths().to_vec();
            paths.sort_unstable();
            paths
        });
        assert!(candidates(&next.generation, "original indexed").is_empty());
        if replacement.is_some() {
            assert_eq!(candidates(&next.generation, "replacement"), ["source.txt"]);
        } else {
            assert_eq!(next.generation.base().reader().num_trigrams(), 0);
        }
    }
}

#[test]
fn short_raw_bytes_with_decoded_postings_still_reuse_postings() {
    let fixture = Fixture::new();
    write(&fixture.repo, "invalid.txt", b"\xff");
    write(&fixture.repo, "source.txt", b"original indexed text");
    fixture.commit();
    let manager = fixture.manager();
    let original = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap()
        .generation;
    write(&fixture.repo, "source.txt", b"replacement indexed text");
    fixture.commit();
    let next = manager
        .ensure("HEAD", IndexingProfile::default(), Some(&original))
        .unwrap();
    assert_eq!(next.stats.blobs_extracted, 1);
    assert_eq!(next.stats.reused_indexed_files, 1);
    assert!(next.stats.postings_reused > 0);
    assert!(next.stats.predecessor_posting_lists_read > 0);
    assert_eq!(candidates(&next.generation, "\u{fffd}"), ["invalid.txt"]);
}

#[test]
fn sha256_repositories_and_unicode_paths_are_supported() {
    let fixture = Fixture::new();
    let root = fixture._temp.path().join("repository \u{03bb}");
    fs::create_dir(&root).unwrap();
    git(
        &root,
        &[
            "init",
            "--quiet",
            "--object-format=sha256",
            "--initial-branch=main",
        ],
    );
    write(&root, "source-\u{03bb}.txt", b"unicode path contents");
    git(&root, &["add", "--all"]);
    git(&root, &["commit", "--quiet", "-m", "sha256"]);
    let manager =
        GenerationManager::with_storage(Repository::discover(&root).unwrap(), &fixture.storage)
            .unwrap();
    let result = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap();
    assert_eq!(result.generation.key().tree_oid().len(), 64);
    assert_eq!(result.generation.commit_oid().len(), 64);
    assert_eq!(result.generation.entries()[0].oid.len(), 64);
    assert_eq!(
        candidates(&result.generation, "unicode"),
        ["source-\u{03bb}.txt"]
    );
}

#[cfg(unix)]
#[test]
fn non_unicode_repository_roots_are_lossless_but_tracked_paths_are_unsupported() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = Fixture::new();
    let native = fixture
        ._temp
        .path()
        .join(std::ffi::OsString::from_vec(b"repo-\xff".to_vec()));
    match fs::rename(&fixture.repo, &native) {
        Ok(()) => {}
        Err(error) if cfg!(target_os = "macos") && error.raw_os_error() == Some(92) => {
            // APFS rejects non-UTF-8 names before Git or the manager can read them.
            assert!(matches!(
                Repository::discover(&native),
                Err(GenerationError::Io(_))
            ));
            return;
        }
        Err(error) => panic!("create native repository path: {error}"),
    }
    write(&native, "source.txt", b"native root");
    git(&native, &["add", "--all"]);
    git(&native, &["commit", "--quiet", "-m", "native"]);
    let repository = Repository::discover(&native).unwrap();
    assert_eq!(
        repository.common_dir(),
        fs::canonicalize(native.join(".git")).unwrap()
    );
    let manager = GenerationManager::with_storage(repository, &fixture.storage).unwrap();
    let base = manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap();
    assert_eq!(candidates(&base.generation, "native"), ["source.txt"]);
    fs::write(
        native.join(std::ffi::OsString::from_vec(b"path-\xff".to_vec())),
        b"bad path",
    )
    .unwrap();
    git(&native, &["add", "--all"]);
    git(&native, &["commit", "--quiet", "-m", "non-Unicode path"]);
    assert!(matches!(
        manager.ensure("HEAD", IndexingProfile::default(), None),
        Err(GenerationError::Unsupported(_))
    ));
}

#[cfg(windows)]
#[test]
fn read_only_publication_lock_returns_permission_error() {
    let fixture = Fixture::new();
    fixture.commit();
    let manager = fixture.manager();
    manager
        .ensure("HEAD", IndexingProfile::default(), None)
        .unwrap();
    let lock = manager.directory().join("publication.lock");
    let permissions = fs::metadata(&lock).unwrap().permissions();
    let mut readonly = permissions.clone();
    readonly.set_readonly(true);
    fs::set_permissions(&lock, readonly).unwrap();
    let result = manager.ensure("HEAD", IndexingProfile::default(), None);
    fs::set_permissions(&lock, permissions).unwrap();
    assert!(
        matches!(result, Err(GenerationError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
    );
}
