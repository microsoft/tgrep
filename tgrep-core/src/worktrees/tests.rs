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
        self.generation_with(IndexingProfile::default())
    }

    fn generation_with(&self, profile: IndexingProfile) -> Arc<Generation> {
        GenerationManager::with_storage(Repository::discover(&self.root).unwrap(), &self.storage)
            .unwrap()
            .ensure("HEAD", profile, None)
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

    fn controlled(
        &self,
    ) -> (
        Arc<crate::managed::Namespace>,
        Arc<crate::managed::WorkPermit>,
        WorktreeView,
    ) {
        let namespace = crate::managed::Namespace::initialize(
            &Repository::discover(&self.root).unwrap(),
            &self.storage,
            crate::managed::policy::fixture_policy(),
        )
        .unwrap();
        namespace.activate().unwrap();
        let operation = namespace
            .accept_system_operation(
                crate::managed::Token::parse("reconciliation").unwrap(),
                "build",
                serde_json::json!({}),
            )
            .unwrap();
        let permit = namespace
            .reserve(
                &operation.id,
                None,
                crate::managed::WorkRequest {
                    allocation_version: 1,
                    private_bytes: 32 * 1024 * 1024,
                    staging_bytes: 32 * 1024 * 1024,
                    slots: 1,
                },
            )
            .unwrap();
        let view = WorktreeView::new_controlled(
            &self.root,
            self.generation(),
            WorktreeOptions::default(),
            &permit,
        )
        .unwrap();
        (namespace, permit, view)
    }
}

#[test]
fn managed_batches_reuse_exact_contents_copies_and_overlay_postings() {
    let fixture = Fixture::new();
    for index in 0..80 {
        write(
            &fixture.root,
            &format!("siblings/file-{index:03}.txt"),
            b"committed common content\n",
        );
    }
    commit(&fixture.root);
    write(
        &fixture.root,
        "siblings/copied.txt",
        b"committed common content\n",
    );
    write(
        &fixture.root,
        "siblings/file-000.txt",
        b"private changed content\n",
    );
    write(&fixture.root, "hidden/.extra", b"private hidden content\n");
    let (_namespace, permit, view) = fixture.controlled();
    let first = view.refresh_controlled(&permit).unwrap();
    assert_eq!(first.files_read, 82);
    assert_eq!(first.base_reused, 79);
    assert_eq!(first.base_files_copied, 1);
    assert_eq!(first.files_extracted, 2);
    assert!(first.batches > 0);
    assert_eq!(first.parent_batches, 2);
    assert!(first.peak_read_bytes <= crate::builder::INDEX_BUILD_BATCH_BYTES);
    assert!(first.peak_private_bytes <= permit.private_limit());
    assert!(first.timings.discovery_us > 0);
    assert!(first.timings.content_verification_us > 0);
    assert!(first.timings.worker_content_us > 0);
    assert!(first.timings.assembly_us > 0);
    assert!(first.control_checks > 0);
    assert_scan_parity(&view, &["common", "private", "hidden", "changed"]);
    view.invalidate_all().unwrap();
    let second = view.refresh_controlled(&permit).unwrap();
    assert_eq!(second.files_read, 82);
    assert_eq!(second.overlay_reused, 3);
    assert_eq!(second.files_extracted, 0);
    assert_eq!(second.base_files_copied, 0);
    assert_scan_parity(&view, &["common", "private", "hidden", "changed"]);
}

#[test]
fn managed_crlf_overlay_fits_its_existing_budget_through_checkpoint_and_refresh() {
    let fixture = Fixture::new();
    write(&fixture.root, ".gitattributes", b"*.txt text eol=crlf\n");
    let mut seed = 121_u32;
    let mut bytes = b"CRLF-MEMORY-REGRESSION\nfollowing_position_marker ".to_vec();
    while bytes.len() < 8189 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        bytes.push(32 + ((seed >> 24) % 95) as u8);
    }
    bytes.push(b'\n');
    for index in 0..8 {
        write(&fixture.root, &format!("files/{index}.txt"), &bytes);
    }
    commit(&fixture.root);
    git(&fixture.root, &["config", "core.autocrlf", "true"]);
    let crlf = String::from_utf8(bytes).unwrap().replace('\n', "\r\n");
    assert_eq!(crlf.len(), 8192);
    for index in 0..8 {
        write(
            &fixture.root,
            &format!("files/{index}.txt"),
            crlf.as_bytes(),
        );
    }
    git(&fixture.root, &["add", "--all"]);
    let status = git(&fixture.root, &["status", "--porcelain"]);
    assert!(status.is_empty(), "{status}");
    let (namespace, permit, view) = fixture.controlled();
    let first = view.refresh_controlled(&permit).unwrap();
    assert!(view.status().unwrap().ready);
    assert_eq!(first.files_read, 9);
    assert_eq!(first.files_extracted, 8);
    assert_eq!(first.base_reused, 1);
    assert_eq!(view.overlay_cost().unwrap().files, 8);
    assert!(first.peak_private_bytes <= permit.private_limit());
    assert_scan_parity(
        &view,
        &["CRLF-MEMORY-REGRESSION", "following_position_marker", r"\r"],
    );
    let checkpoint = view.capture_checkpoint(&permit).unwrap();
    drop(checkpoint);
    view.invalidate_all().unwrap();
    let refreshed = view.refresh_controlled(&permit).unwrap();
    assert_eq!(refreshed.files_read, 9);
    assert_eq!(refreshed.files_extracted, 0);
    assert_eq!(refreshed.overlay_reused, 8);
    assert!(permit.peak_private_bytes() <= permit.private_limit());
    assert_scan_parity(&view, &["following_position_marker", r"\r"]);
    let changed = crlf.replace("following_position_marker", "following_revision_marker");
    assert_eq!(changed.len(), crlf.len());
    write(&fixture.root, "files/0.txt", changed.as_bytes());
    view.invalidate_all().unwrap();
    let changed_stats = view.refresh_controlled(&permit).unwrap();
    assert_eq!(changed_stats.files_extracted, 1);
    assert_eq!(changed_stats.overlay_reused, 7);
    assert_scan_parity(
        &view,
        &[
            "following_position_marker",
            "following_revision_marker",
            r"\r",
        ],
    );
    drop(view.capture_checkpoint(&permit).unwrap());
    assert!(permit.peak_private_bytes() <= permit.private_limit());
    let retained = namespace
        .work_usage()
        .unwrap()
        .memory
        .retained_private_estimate_bytes;
    let available = permit.memory(permit.private_limit() - retained).unwrap();
    drop(available);
}

#[test]
fn managed_changed_files_fit_the_same_private_budget_at_any_worker_width() {
    use crate::managed::faults::{Action, Point, Specification, Stage};
    let fixture = Fixture::new();
    for index in 0..4 {
        write(
            &fixture.root,
            &format!("siblings/{index}.txt"),
            b"committed\n",
        );
    }
    commit(&fixture.root);
    let mut seed = 1_u32;
    let bytes: Vec<_> = (0..1024 * 1024)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            b'a' + ((seed >> 24) % 26) as u8
        })
        .collect();
    for index in 0..4 {
        write(&fixture.root, &format!("siblings/{index}.txt"), &bytes);
    }
    let (namespace, permit, view) = fixture.controlled();
    let fault = namespace
        .install_test_fault(Specification {
            point: Point::ReconcileRetryDrained,
            operation: Some(permit.operation_id().clone()),
            skip_hits: 0,
            action: Action::Pause { timeout_ms: 30_000 },
        })
        .unwrap();
    let stats = std::thread::scope(|scope| {
        let worker = scope.spawn(|| view.refresh_controlled(&permit));
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        let mut retried = false;
        while !worker.is_finished() {
            assert!(Instant::now() < deadline, "reconciliation did not finish");
            if namespace.test_fault_status().unwrap().unwrap().stage == Stage::Waiting {
                // Retained discovery/base-map evidence is small. Every input
                // and partial posting charge must already have been dropped.
                let capacity = permit.memory(permit.private_limit() - 1024 * 1024).unwrap();
                drop(capacity);
                let parent = fixture.root.join("siblings");
                let moved = fixture.root.join("released");
                fs::rename(&parent, &moved).unwrap();
                fs::rename(&moved, &parent).unwrap();
                namespace.release_test_fault(&fault.ticket).unwrap();
                retried = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let stats = worker.join().unwrap().unwrap();
        assert_eq!(stats.batch_retries, u64::from(retried));
        stats
    });
    assert_eq!(stats.files_extracted, 4);
    assert!(stats.peak_private_bytes <= permit.private_limit());
    assert!(view.status().unwrap().ready);
}

#[test]
fn substantive_verification_errors_override_secondary_pressure_and_cancellation() {
    use crate::managed::{Error, ErrorCategory};
    for category in [
        ErrorCategory::Io,
        ErrorCategory::StaleVersion,
        ErrorCategory::Deadline,
    ] {
        for secondary in [
            Error::pressure("reserved-private-memory-exhausted"),
            Error::new(
                ErrorCategory::Cancelled,
                "operation-cancelled",
                "peer cancellation",
            ),
        ] {
            let mut failure = Some(WorktreeError::Index(secondary.into()));
            record_failure(
                &mut failure,
                WorktreeError::Index(
                    Error::new(
                        category,
                        "substantive-verification-failure",
                        "verification failed",
                    )
                    .into(),
                ),
            );
            assert!(
                matches!(failure, Some(WorktreeError::Index(crate::Error::Managed(error)))
                if error.category == category)
            );
        }
    }
    assert!(!is_private_pressure(&WorktreeError::Index(
        Error::new(
            ErrorCategory::Io,
            "reserved-private-memory-exhausted",
            "I/O failure"
        )
        .into()
    )));
}

#[cfg(unix)]
#[test]
fn managed_and_unmanaged_parent_pins_fit_a_low_descriptor_limit() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "worktrees::tests::low_descriptor_limit_child",
            "--ignored",
            "--test-threads=1",
        ])
        .env("TGREP_TEST_LOW_FD_CHILD", "1")
        .env("RAYON_NUM_THREADS", "4")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[cfg(unix)]
#[test]
#[ignore = "subprocess entry point; lowers only this child process's descriptor limit"]
fn low_descriptor_limit_child() {
    assert_eq!(std::env::var("TGREP_TEST_LOW_FD_CHILD").as_deref(), Ok("1"));
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: valid output storage; this opt-in child runs no other tests.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0
    );
    limit.rlim_cur = limit.rlim_cur.min(256);
    // SAFETY: only lowering this dedicated subprocess's soft limit.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    let fixture = Fixture::new();
    for index in 0..512 {
        write(
            &fixture.root,
            &format!("parents/{index:04}/nested/file.txt"),
            b"stable content\n",
        );
    }
    commit(&fixture.root);
    let generation = fixture.generation();
    let unmanaged = fixture.view(&fixture.root, &generation);
    let stats = unmanaged.refresh().unwrap();
    assert_eq!(stats.files_read, 512);
    assert_eq!(stats.base_reused, 512);
    assert_eq!(stats.batches, 512);
    drop(unmanaged);
    let (_namespace, permit, managed) = fixture.controlled();
    let stats = managed.refresh_controlled(&permit).unwrap();
    assert_eq!(stats.files_read, 512);
    assert_eq!(stats.base_reused, 512);
    assert!(stats.batches < 512);
    assert!(stats.peak_parent_handles <= crate::managed::work::RECONCILE_PARENT_HANDLES);
    assert!(managed.status().unwrap().ready);
}

#[test]
fn managed_worker_failure_drains_buffers_and_parent_handles_before_returning() {
    use crate::managed::faults::{Action, Point, Specification};
    let fixture = Fixture::new();
    for index in 0..40 {
        write(
            &fixture.root,
            &format!("siblings-{}/file-{index:03}.txt", index % 2),
            b"stable content\n",
        );
    }
    commit(&fixture.root);
    let (namespace, permit, view) = fixture.controlled();
    namespace
        .install_test_fault(Specification {
            point: Point::ReconcileBatchOpened,
            operation: Some(permit.operation_id().clone()),
            skip_hits: 1,
            action: Action::Error {
                category: crate::managed::ErrorCategory::Io,
            },
        })
        .unwrap();
    assert!(view.refresh_controlled(&permit).is_err());
    assert!(!view.status().unwrap().ready);
    assert!(view.status().unwrap().full_required);
    for index in 0..2 {
        fs::rename(
            fixture.root.join(format!("siblings-{index}")),
            fixture.root.join(format!("released-{index}")),
        )
        .unwrap();
    }
    drop((view, permit));
    assert_eq!(namespace.work_usage().unwrap().reservations, 0);
}

#[test]
fn managed_cancellation_and_policy_changes_drain_paused_directory_batches() {
    use crate::managed::faults::{Action, Point, Specification, Stage};
    for policy_change in [false, true] {
        let fixture = Fixture::new();
        for index in 0..40 {
            write(
                &fixture.root,
                &format!("siblings/file-{index:03}.txt"),
                b"stable content\n",
            );
        }
        commit(&fixture.root);
        let (namespace, permit, view) = fixture.controlled();
        let fault = namespace
            .install_test_fault(Specification {
                point: Point::ReconcileBatchOpened,
                operation: Some(permit.operation_id().clone()),
                skip_hits: 0,
                action: Action::Pause { timeout_ms: 30_000 },
            })
            .unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| view.refresh_controlled(&permit));
            let deadline = Instant::now() + std::time::Duration::from_secs(20);
            while namespace.test_fault_status().unwrap().unwrap().stage != Stage::Waiting {
                assert!(Instant::now() < deadline, "worker did not open the batch");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if policy_change {
                let mut policy = namespace.policy().unwrap();
                policy.policy.work.blob_bytes /= 2;
                namespace
                    .update_policy(policy.version, policy.policy)
                    .unwrap();
            } else {
                permit.cancel();
            }
            namespace.release_test_fault(&fault.ticket).unwrap();
            let error = worker.join().unwrap().unwrap_err();
            let expected = if policy_change {
                crate::managed::ErrorCategory::StaleVersion
            } else {
                crate::managed::ErrorCategory::Cancelled
            };
            assert!(
                matches!(error, WorktreeError::Index(crate::Error::Managed(error))
                if error.category == expected)
            );
        });
        assert!(!view.status().unwrap().ready);
        assert!(view.status().unwrap().full_required);
        fs::rename(fixture.root.join("siblings"), fixture.root.join("released")).unwrap();
        drop((view, permit));
        assert_eq!(namespace.work_usage().unwrap().reservations, 0);
    }
}

#[test]
fn managed_verified_read_classifies_growth_at_blob_limit_as_unstable() {
    let fixture = Fixture::new();
    write(&fixture.root, "file.txt", b"committed\n");
    commit(&fixture.root);
    let (_namespace, permit, view) = fixture.controlled();
    let path = fixture.root.join("file.txt");
    let blob_bytes = permit.read_limits().blob_bytes;
    let writer = File::options().write(true).open(&path).unwrap();
    writer.set_len(blob_bytes).unwrap();
    let reader = view.files.batch(Path::new("")).unwrap();
    let relative = Path::new("file.txt");
    let opened = reader.open_observed(relative).unwrap();
    assert_eq!(opened.metadata.len(), blob_bytes);
    writer.set_len(blob_bytes + 1).unwrap();

    let read = read_file(
        || reader.open_observed(relative),
        relative,
        opened,
        None,
        Some(&permit),
    );
    assert!(matches!(read, Err(WorktreeError::UnstableFile(_))));
}

#[test]
fn managed_verified_read_preserves_blob_limit_for_stable_oversized_file() {
    let fixture = Fixture::new();
    write(&fixture.root, "file.txt", b"committed\n");
    commit(&fixture.root);
    let (_namespace, permit, view) = fixture.controlled();
    let path = fixture.root.join("file.txt");
    let blob_bytes = permit.read_limits().blob_bytes;
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(blob_bytes + 1)
        .unwrap();
    let reader = view.files.batch(Path::new("")).unwrap();
    let relative = Path::new("file.txt");
    let opened = reader.open_observed(relative).unwrap();
    assert_eq!(opened.metadata.len(), blob_bytes + 1);

    let read = read_file(
        || reader.open_observed(relative),
        relative,
        opened,
        None,
        Some(&permit),
    );
    assert!(
        matches!(read, Err(WorktreeError::Index(crate::Error::Managed(error)))
        if error.category == crate::managed::ErrorCategory::ResourcePressure
            && error.reason_code == "checkout-blob-read-limit")
    );
}

#[test]
fn managed_verified_read_rejects_restored_mtime_mutation_and_replacement() {
    for replace in [false, true] {
        let fixture = Fixture::new();
        write(&fixture.root, "file.txt", b"before");
        commit(&fixture.root);
        let (_namespace, permit, view) = fixture.controlled();
        let path = fixture.root.join("file.txt");
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let reader = view.files.batch(Path::new("")).unwrap();
        let relative = Path::new("file.txt");
        let file = reader.open_observed(relative).unwrap();
        let read = read_file(
            || {
                if replace {
                    fs::rename(&path, fixture.root.join("previous"))?;
                }
                fs::write(&path, b"after!")?;
                File::options()
                    .write(true)
                    .open(&path)?
                    .set_modified(modified)?;
                reader.open_observed(relative)
            },
            relative,
            file,
            None,
            Some(&permit),
        );
        assert!(matches!(read, Err(WorktreeError::UnstableFile(_))));
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
                let mut bytes = Vec::new();
                snapshot
                    .open_file(path)
                    .unwrap()
                    .read_to_end(&mut bytes)
                    .unwrap();
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
        search_binary: view.generation.key().profile().eligibility
            == crate::generations::ContentEligibility::ContentOnly,
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
fn linked_gitfile_matches_hidden_scan_without_exposing_metadata_directories() {
    let fixture = Fixture::new();
    write(&fixture.root, ".gitignore", b"ignored/\n");
    write(&fixture.root, ".hidden.txt", b"hidden needle");
    write(&fixture.root, "asset.bin", b"filename only");
    write(&fixture.root, "src/one.txt", b"source needle");
    commit(&fixture.root);
    let pin = fixture.generation();
    let linked = fixture.linked("gitfile parity");
    write(&linked, "ignored/private.txt", b"ignored needle");
    assert!(linked.join(".git").is_file());
    let view = fixture.view(&linked, &pin);
    view.refresh().unwrap();

    // Use the ordinary walker without the view's exclusions: inheriting them
    // here would hide precisely the membership regression this test detects.
    for include_hidden in [false, true] {
        let scan = walker::walk_dir(
            view.root(),
            &walker::WalkOptions {
                include_hidden,
                ..walker::WalkOptions::default()
            },
        );
        assert_eq!(scan.skipped_error, 0);
        let mut expected: Vec<_> = scan
            .listed_files
            .iter()
            .map(|path| {
                path.strip_prefix(view.root())
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .replace('\\', "/")
            })
            .collect();
        expected.sort_unstable();
        assert_eq!(
            view.with_snapshot(|snapshot| snapshot.files("", include_hidden))
                .unwrap(),
            expected,
        );
        assert_eq!(expected.iter().any(|path| path == ".git"), include_hidden);

        let plan = query::build_query_plan("gitdir:", false).unwrap();
        let expected_matches: Vec<_> = scan
            .files
            .iter()
            .filter(|path| {
                let bytes = fs::read(path).unwrap();
                encoding::decode_for_index(&bytes)
                    .windows(b"gitdir:".len())
                    .any(|window| window == b"gitdir:")
            })
            .map(|path| {
                path.strip_prefix(view.root())
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(
            view.with_snapshot(|snapshot| snapshot.candidates(&plan, "", include_hidden))
                .unwrap(),
            expected_matches,
        );
        assert_eq!(
            expected_matches.iter().any(|path| path == ".git"),
            include_hidden
        );
    }
    assert_eq!(matches(&view, "^gitdir:"), [".git"]);
    assert_scan_parity(&view, &["gitdir:", "^gitdir:", "needle", ".*"]);

    for root in [&fixture.root, &linked] {
        let view = WorktreeView::new(
            root,
            pin.clone(),
            WorktreeOptions {
                walk: MetaWalkOptions {
                    no_ignore: true,
                    ..MetaWalkOptions::default()
                },
                ..WorktreeOptions::default()
            },
        )
        .unwrap();
        view.refresh().unwrap();
        let files = view
            .with_snapshot(|snapshot| snapshot.files("", true))
            .unwrap();
        assert!(!files.iter().any(|path| path.starts_with(".git/")));
        assert_eq!(files.iter().any(|path| path == ".git"), root == &linked);
        for metadata in [view.repository().common_dir(), view.repository().git_dir()] {
            assert!(
                view.options
                    .walk
                    .exclude_paths
                    .iter()
                    .any(|path| metadata.starts_with(path))
            );
        }
    }

    write(&linked, ".ignore", b".git\n");
    view.invalidate_path(Path::new(".ignore")).unwrap();
    view.refresh().unwrap();
    assert!(candidates(&view, "gitdir:").is_empty());
    assert!(
        !view
            .with_snapshot(|snapshot| snapshot.files("", true))
            .unwrap()
            .contains(&".git".to_string())
    );
}

#[test]
fn identical_tracked_files_share_reader_without_extraction() {
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
    for (view, gitfile_count) in [(&first, 0), (&second, 1)] {
        let stats = view.refresh().unwrap();
        assert!(stats.full);
        assert_eq!(stats.files_read, 3 + gitfile_count);
        assert_eq!(stats.files_decoded, 3 + gitfile_count);
        assert_eq!(stats.files_extracted, gitfile_count);
        assert_eq!(stats.base_reused, 3);
        assert_eq!(
            view.state.read().unwrap().index.live.overlay_paths(),
            if gitfile_count == 0 {
                vec![]
            } else {
                vec![".git".to_string()]
            },
        );
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
    let linked_no_op = second.refresh().unwrap();
    assert_eq!(linked_no_op.files_read, 4);
    assert_eq!(linked_no_op.files_extracted, 0);
    assert_eq!(linked_no_op.overlay_reused, 1);
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
    assert_eq!(initial.files_extracted, 5);
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
    assert_eq!(stats.overlay_reused, 6);
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
    assert_eq!(stats.files_read, 5);
    assert_eq!(stats.files_decoded, 5);
    assert_eq!(
        stats.files_extracted, 5,
        "four clean files differ from raw LF blobs, plus the private .git file"
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
    assert_eq!(no_op.overlay_reused, 5);
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
    assert!(candidates(&view, "sparse needle").is_empty());
    assert!(candidates(&view, "original").is_empty());
    assert_eq!(matches(&view, "changed"), ["skip.txt"]);
    assert_scan_parity(&view, &["needle", "original", "changed", ".*"]);
    git(&linked, &["sparse-checkout", "disable"]);
    view.reconcile_full().unwrap();
    assert_eq!(matches(&view, "sparse needle"), ["omit/source.txt"]);
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
fn snapshot_sibling_batches_reject_foreign_roots_and_record_open_failures() {
    let fixture = Fixture::new();
    write(&fixture.root, "dir/one.txt", b"first needle");
    write(&fixture.root, "dir/two.txt", b"second needle");
    commit(&fixture.root);
    let generation = fixture.generation();
    let view = fixture.view(&fixture.root, &generation);
    view.refresh().unwrap();
    let linked = fixture.linked("other");
    let other = fixture.view(&linked, &generation);
    other.refresh().unwrap();
    let batch = view
        .with_snapshot(|snapshot| snapshot.candidate_batch("dir/one.txt"))
        .unwrap()
        .unwrap();
    let mut bytes = String::new();
    view.with_snapshot(|snapshot| snapshot.open_candidate_in(&batch, "dir/one.txt"))
        .unwrap()
        .unwrap()
        .read_to_string(&mut bytes)
        .unwrap();
    assert_eq!(bytes, "first needle");
    assert!(
        other
            .with_snapshot(|snapshot| snapshot.open_candidate_in(&batch, "dir/one.txt"))
            .unwrap()
            .is_err()
    );
    assert!(other.status().unwrap().ready);
    fs::remove_file(fixture.root.join("dir/one.txt")).unwrap();
    let before = view.status().unwrap();
    let error = view
        .with_snapshot(|snapshot| {
            assert!(snapshot.open_candidate_in(&batch, "dir/one.txt").is_err());
            assert!(snapshot.open_candidate_in(&batch, "dir/two.txt").is_ok());
        })
        .unwrap_err();
    assert!(matches!(error, WorktreeError::Io(_)));
    let after = view.status().unwrap();
    assert!(!after.ready);
    assert!(after.full_required);
    assert!(after.epoch > before.epoch);
    batch.finish().unwrap();
}

#[test]
fn snapshot_candidate_opens_reject_directory_link_swaps() {
    let fixture = Fixture::new();
    write(&fixture.root, "dir/file.txt", b"inside needle");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    view.refresh().unwrap();
    let mut bytes = Vec::new();
    view.with_snapshot(|snapshot| snapshot.open_file("dir/file.txt"))
        .unwrap()
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"inside needle");
    let before = view.status().unwrap();

    let outside = fixture._temp.path().join("outside");
    write(&outside, "file.txt", b"outside secret");
    fs::rename(fixture.root.join("dir"), fixture.root.join("saved")).unwrap();
    crate::rooted::tests::link_directory(&outside, &fixture.root.join("dir"));
    assert!(matches!(
        view.with_snapshot(|snapshot| snapshot.open_file("dir/file.txt")),
        Err(WorktreeError::Io(_))
    ));
    let after = view.status().unwrap();
    assert!(!after.ready);
    assert!(after.full_required);
    assert!(after.epoch > before.epoch);
    assert_eq!(after.published_epoch, before.published_epoch);
    assert!(view.refresh().unwrap().full);
    assert!(candidates(&view, "outside secret").is_empty());
    assert_eq!(matches(&view, "inside needle"), ["saved/file.txt"]);
}

#[test]
fn snapshot_open_failures_require_full_repair_even_if_the_callback_ignores_them() {
    let fixture = Fixture::new();
    write(&fixture.root, "file.txt", b"inside needle");
    write(&fixture.root, "other.txt", b"old marker");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    view.refresh().unwrap();
    let before = view.status().unwrap();
    let other = fixture.root.join("other.txt");
    let modified = fs::metadata(&other).unwrap().modified().unwrap();
    write(&fixture.root, "other.txt", b"new marker");
    fs::OpenOptions::new()
        .write(true)
        .open(&other)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    fs::remove_file(fixture.root.join("file.txt")).unwrap();

    let error = view
        .with_snapshot(|snapshot| {
            assert!(snapshot.open_file("file.txt").is_err());
            assert!(snapshot.open_file("second-missing.txt").is_err());
            assert!(snapshot.open_file("other.txt").is_ok());
            "a swallowed error must not publish this callback result"
        })
        .unwrap_err();
    assert!(
        matches!(&error, WorktreeError::Io(error) if error.kind() == std::io::ErrorKind::NotFound)
    );
    assert!(error.to_string().contains("file.txt"));
    assert!(!error.to_string().contains("second-missing.txt"));
    let after = view.status().unwrap();
    assert!(!after.ready);
    assert!(after.full_required);
    assert_eq!(after.pending_paths, 0);
    assert!(after.epoch > before.epoch);
    assert_eq!(after.published_epoch, before.published_epoch);
    assert!(matches!(
        view.with_snapshot(|_| panic!("not-ready callbacks must not run")),
        Err(WorktreeError::NotReady)
    ));

    view.invalidate_path(Path::new("unrelated.txt")).unwrap();
    assert!(view.refresh().unwrap().full);
    assert!(candidates(&view, "inside needle").is_empty());
    assert_eq!(matches(&view, "new marker"), ["other.txt"]);
}

#[cfg(unix)]
#[test]
fn snapshot_fifo_swaps_invalidate_readiness_without_blocking() {
    if !crate::rooted::tests::bounded_child(
        "worktrees::tests::snapshot_fifo_swaps_invalidate_readiness_without_blocking",
    ) {
        return;
    }
    let fixture = Fixture::new();
    write(&fixture.root, "file.txt", b"inside needle");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    view.refresh().unwrap();
    let path = fixture.root.join("file.txt");
    fs::remove_file(&path).unwrap();
    crate::rooted::tests::make_fifo(&path);
    assert!(matches!(
        view.with_snapshot(|snapshot| {
            assert!(snapshot.open_file("file.txt").is_err());
        }),
        Err(WorktreeError::Io(_))
    ));
    assert!(!view.status().unwrap().ready);
    assert!(view.status().unwrap().full_required);
    assert!(matches!(
        view.with_snapshot(|_| ()),
        Err(WorktreeError::NotReady)
    ));
    fs::remove_file(&path).unwrap();
    write(&fixture.root, "file.txt", b"repaired needle");
    assert!(view.refresh().unwrap().full);
    assert_eq!(matches(&view, "repaired needle"), ["file.txt"]);
}

#[cfg(unix)]
#[test]
fn snapshots_reject_root_replacement_before_and_during_empty_queries() {
    for during_callback in [false, true] {
        let fixture = Fixture::new();
        write(&fixture.root, "file.txt", b"inside needle");
        commit(&fixture.root);
        let view = fixture.view(&fixture.root, &fixture.generation());
        view.refresh().unwrap();
        let before = view.status().unwrap();
        let saved = fixture._temp.path().join("saved");
        let replace = || {
            fs::rename(&fixture.root, &saved).unwrap();
            fs::create_dir(&fixture.root).unwrap();
            write(&fixture.root, "file.txt", b"outside secret");
        };
        if !during_callback {
            replace();
        }
        let result = view.with_snapshot(|snapshot| {
            assert!(
                during_callback,
                "verify the root before invoking the callback"
            );
            replace();
            snapshot.files("missing/", true)
        });
        assert!(matches!(result, Err(WorktreeError::Io(_))));
        let after = view.status().unwrap();
        assert!(!after.ready);
        assert!(after.full_required);
        assert!(after.epoch > before.epoch);
        assert_eq!(after.published_epoch, before.published_epoch);
        assert!(matches!(
            view.with_snapshot(|_| ()),
            Err(WorktreeError::NotReady)
        ));

        fs::remove_file(fixture.root.join("file.txt")).unwrap();
        fs::remove_dir(&fixture.root).unwrap();
        fs::rename(saved, &fixture.root).unwrap();
        assert!(view.refresh().unwrap().full);
        assert_eq!(matches(&view, "inside needle"), ["file.txt"]);
    }
}

#[test]
fn directory_link_swap_after_discovery_keeps_the_view_not_ready() {
    let fixture = Fixture::new();
    write(&fixture.root, "dir/file.txt", b"inside needle");
    commit(&fixture.root);
    let outside = fixture._temp.path().join("outside");
    write(&outside, "file.txt", b"outside secret");
    let view = fixture.view(&fixture.root, &fixture.generation());
    view.refresh().unwrap();
    let published = view.status().unwrap().published_epoch;
    let result = view.refresh_inner(
        || {
            fs::rename(fixture.root.join("dir"), fixture.root.join("saved")).unwrap();
            crate::rooted::tests::link_directory(&outside, &fixture.root.join("dir"));
        },
        || {},
    );
    assert!(matches!(result, Err(WorktreeError::Io(_))));
    let status = view.status().unwrap();
    assert!(!status.ready);
    assert!(status.full_required);
    assert_eq!(status.published_epoch, published);
    assert!(matches!(
        view.with_snapshot(|_| ()),
        Err(WorktreeError::NotReady)
    ));
    view.refresh().unwrap();
    assert!(candidates(&view, "outside secret").is_empty());
    assert_eq!(matches(&view, "inside needle"), ["saved/file.txt"]);
}

#[cfg(unix)]
#[test]
fn root_replacement_before_publication_keeps_the_view_not_ready() {
    let fixture = Fixture::new();
    write(&fixture.root, "file.txt", b"inside needle");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    view.refresh().unwrap();
    let result = view.refresh_inner(
        || {},
        || {
            fs::rename(&fixture.root, fixture._temp.path().join("saved")).unwrap();
            fs::create_dir(&fixture.root).unwrap();
            write(&fixture.root, "file.txt", b"outside secret");
        },
    );
    assert!(matches!(result, Err(WorktreeError::Io(_))));
    assert!(!view.status().unwrap().ready);
    assert!(matches!(
        view.with_snapshot(|_| ()),
        Err(WorktreeError::NotReady)
    ));
}

#[test]
fn observed_parent_replacement_after_batch_drain_blocks_publication() {
    for managed in [false, true] {
        let fixture = Fixture::new();
        write(&fixture.root, "dir/nested/file.txt", b"original marker");
        commit(&fixture.root);
        let exercise = |view: &WorktreeView, permit: Option<&Arc<crate::managed::WorkPermit>>| {
            let result = view.refresh_controlled_inner(
                || {},
                || {
                    fs::rename(fixture.root.join("dir"), fixture._temp.path().join("saved"))
                        .unwrap();
                    write(&fixture.root, "dir/nested/file.txt", b"replacement marker");
                    view.files.verify_root().unwrap();
                },
                permit,
            );
            assert!(matches!(result, Err(WorktreeError::Io(_))), "{result:?}");
            let status = view.status().unwrap();
            assert!(!status.ready);
            assert!(status.full_required);
            assert!(status.published_epoch.is_none());
            assert!(matches!(
                view.with_snapshot(|_| ()),
                Err(WorktreeError::NotReady)
            ));
            let released = fixture._temp.path().join("after-check");
            fs::rename(fixture.root.join("dir"), &released).unwrap();
            fs::rename(&released, fixture.root.join("dir")).unwrap();
            let repaired = view.refresh_controlled_inner(|| {}, || {}, permit).unwrap();
            assert!(repaired.full);
            assert_eq!(repaired.files_read, 1);
            assert_eq!(repaired.content_reads_avoided, 0);
            assert!(candidates(view, "original").is_empty());
            assert_eq!(matches(view, "replacement"), ["dir/nested/file.txt"]);
        };
        if managed {
            let (_namespace, permit, view) = fixture.controlled();
            exercise(&view, Some(&permit));
        } else {
            let view = fixture.view(&fixture.root, &fixture.generation());
            exercise(&view, None);
        }
    }
}

#[cfg(unix)]
#[test]
fn fifo_swap_after_discovery_is_bounded_and_keeps_the_view_not_ready() {
    if !crate::rooted::tests::bounded_child(
        "worktrees::tests::fifo_swap_after_discovery_is_bounded_and_keeps_the_view_not_ready",
    ) {
        return;
    }
    let fixture = Fixture::new();
    write(&fixture.root, "file.txt", b"inside needle");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    view.refresh().unwrap();
    let path = fixture.root.join("file.txt");
    let result = view.refresh_inner(
        || {
            fs::remove_file(&path).unwrap();
            crate::rooted::tests::make_fifo(&path);
        },
        || {},
    );
    assert!(matches!(result, Err(WorktreeError::Io(_))));
    assert!(!view.status().unwrap().ready);
    assert!(view.status().unwrap().full_required);
    assert!(matches!(
        view.with_snapshot(|_| ()),
        Err(WorktreeError::NotReady)
    ));
    fs::remove_file(&path).unwrap();
    write(&fixture.root, "file.txt", b"repaired needle");
    view.refresh().unwrap();
    assert_eq!(matches(&view, "repaired"), ["file.txt"]);
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
fn checkpoint_configuration_rejects_regular_files() {
    let fixture = Fixture::new();
    write(&fixture.root, "source.txt", b"needle");
    commit(&fixture.root);
    let pin = fixture.generation();
    for path in [
        fixture.root.join("checkpoint.json"),
        fixture._temp.path().join("checkpoint.json"),
    ] {
        fs::write(&path, b"preserve checkpoint file").unwrap();
        let options = WorktreeOptions {
            checkpoint_directory: Some(path.clone()),
            ..WorktreeOptions::default()
        };
        for restored in [false, true] {
            let result = if restored {
                WorktreeView::restore(&fixture.root, pin.clone(), options.clone())
            } else {
                WorktreeView::new(&fixture.root, pin.clone(), options.clone())
            };
            let error = result
                .err()
                .expect("regular files cannot be checkpoint directories");
            assert!(matches!(error, WorktreeError::InvalidInput(_)), "{error}");
            assert_eq!(fs::read(&path).unwrap(), b"preserve checkpoint file");
        }
    }
}

#[test]
fn checkpoint_save_rejects_a_directory_replaced_by_a_file() {
    let fixture = Fixture::new();
    write(&fixture.root, "source.txt", b"needle");
    commit(&fixture.root);
    let directory = fixture.root.join("private checkpoint");
    fs::create_dir(&directory).unwrap();
    let view = WorktreeView::new(
        &fixture.root,
        fixture.generation(),
        WorktreeOptions {
            checkpoint_directory: Some(directory.clone()),
            ..WorktreeOptions::default()
        },
    )
    .unwrap();
    view.refresh().unwrap();
    fs::remove_dir(&directory).unwrap();
    fs::write(&directory, b"preserve replacement file").unwrap();
    assert!(matches!(
        view.save_checkpoint(),
        Err(WorktreeError::InvalidInput(_))
    ));
    assert_eq!(fs::read(&directory).unwrap(), b"preserve replacement file");
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
fn subtree_hint_spellings_reverify_restored_mtime_edits() {
    let fixture = Fixture::new();
    write(&fixture.root, "dir/nested/one.txt", b"term00");
    write(&fixture.root, "dir/nested/two.txt", b"unchanged");
    write(&fixture.root, "outside.txt", b"outside");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    view.refresh().unwrap();
    let path = fixture.root.join("dir/nested/one.txt");
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    for (number, hint) in [
        "dir/",
        "dir//",
        "dir/.",
        "dir//./",
        "dir/./nested//",
        "dir//nested/.",
    ]
    .into_iter()
    .enumerate()
    {
        let content = format!("term{:02}", number + 1);
        fs::write(&path, &content).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        assert_eq!(fs::metadata(&path).unwrap().len(), 6);
        view.invalidate_path(Path::new(hint)).unwrap();
        let stats = view.refresh().unwrap();
        assert_eq!(matches(&view, &content), ["dir/nested/one.txt"], "{hint}");
        // The unchanged sibling also needs verification: this assertion
        // detects lost subtree hints even when Unix ctime catches the edit.
        assert_eq!(stats.files_read, 2, "{hint}");
        assert_eq!(stats.content_reads_avoided, 1, "{hint}");
        assert!(!stats.full, "{hint}");
    }
}

#[test]
fn accepted_hint_spellings_have_one_canonical_queue_entry() {
    let fixture = Fixture::new();
    write(&fixture.root, "dir/file.txt", b"needle");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    view.refresh().unwrap();
    for hint in ["dir", "dir/", "dir//", "dir/.", "dir//./"] {
        view.invalidate_path(Path::new(hint)).unwrap();
        let control = view.control.lock().unwrap();
        assert_eq!(control.hints, BTreeSet::from(["dir".to_string()]), "{hint}");
        assert!(!control.full);
    }
    #[cfg(windows)]
    for hint in [r"dir\", r"dir\\.\", r"dir//.\"] {
        view.invalidate_path(Path::new(hint)).unwrap();
        assert_eq!(
            view.control.lock().unwrap().hints,
            BTreeSet::from(["dir".to_string()])
        );
    }
    for hint in ["", ".", "./dir", "../dir", "dir/../file"] {
        assert!(matches!(
            view.invalidate_path(Path::new(hint)),
            Err(WorktreeError::InvalidInput(_))
        ));
        assert!(view.status().unwrap().full_required);
        assert!(!view.status().unwrap().ready);
    }
}

#[test]
fn hint_lookup_work_depends_on_path_depth_not_queue_size() {
    let fixture = Fixture::new();
    write(&fixture.root, "dir/nested/one.txt", b"old needle");
    write(&fixture.root, "directory/other.txt", b"unaffected");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    assert_eq!(view.refresh().unwrap().hint_lookups, 0);
    let path = fixture.root.join("dir/nested/one.txt");
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&path, b"new needle").unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    for index in 0..2000 {
        view.invalidate_path(Path::new(&format!("unrelated-{index}/file")))
            .unwrap();
    }
    view.invalidate_path(Path::new("DIR//.")).unwrap();
    let stats = view.refresh().unwrap();
    assert!(!stats.full);
    assert_eq!(
        stats.hint_lookups, 5,
        "three prefixes plus two, independent of 2001 hints"
    );
    assert_eq!(stats.files_read, 1);
    assert_eq!(
        stats.content_reads_avoided, 1,
        "dir must not match directory"
    );
    assert_eq!(stats.files_extracted, 1);
    assert_eq!(matches(&view, "new needle"), ["dir/nested/one.txt"]);

    view.invalidate_path(Path::new("dir/nested/one.txt"))
        .unwrap();
    let stats = view.refresh().unwrap();
    assert_eq!(stats.hint_lookups, 3, "exact matches stop at the full path");
    assert_eq!(stats.files_read, 1);
    assert_eq!(stats.files_extracted, 0);
    view.invalidate_path(Path::new("\u{00e9}")).unwrap();
    let stats = view.refresh().unwrap();
    assert!(stats.full);
    assert_eq!(stats.hint_lookups, 0);
    assert_eq!(stats.files_read, 2);
}

#[cfg(unix)]
fn assert_native_alias_is_rejected(native_name: &std::ffi::OsStr, alias: &str) {
    let fixture = Fixture::new();
    write(&fixture.root, "source.txt", b"tracked needle");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    view.refresh().unwrap();
    let published = view.status().unwrap().published_epoch;
    write(&fixture.root, alias, b"ignored alias secret");
    write(
        &fixture.root,
        ".gitignore",
        format!("/{alias}\n").as_bytes(),
    );
    let native = fixture.root.join(native_name);
    match fs::write(&native, b"native visible needle") {
        Ok(()) => {}
        Err(error) if cfg!(target_os = "macos") && error.raw_os_error() == Some(92) => {
            eprintln!("APFS rejected the non-UTF-8 fixture filename: {error}");
            return;
        }
        Err(error) => panic!("create native alias fixture: {error}"),
    }
    let scan = walker::walk_dir(
        view.root(),
        &walker::WalkOptions {
            include_hidden: true,
            ..walker::WalkOptions::default()
        },
    );
    assert_eq!(scan.skipped_error, 0);
    assert!(
        scan.listed_files
            .contains(&fs::canonicalize(&native).unwrap())
    );
    assert!(
        !scan
            .listed_files
            .contains(&fs::canonicalize(fixture.root.join(alias)).unwrap())
    );

    assert!(
        matches!(view.refresh(), Err(WorktreeError::IncompleteWalk(_))),
        "unsupported native path must not read an ignored alias: {native_name:?}"
    );
    assert!(!view.status().unwrap().ready);
    assert!(view.status().unwrap().full_required);
    assert_eq!(view.status().unwrap().published_epoch, published);
    assert!(matches!(
        view.with_snapshot(|_| ()),
        Err(WorktreeError::NotReady)
    ));
    let metadata = walker::walk_file_metadata(view.root(), &MetaWalkOptions::default());
    assert!(metadata.skipped_error > 0);
    assert!(!metadata.listed_files.iter().any(|path| path == alias));
    assert!(
        !metadata
            .files
            .iter()
            .any(|file| file.relative_path == alias)
    );

    fs::remove_file(native).unwrap();
    view.refresh().unwrap();
    assert!(matches(&view, "ignored alias secret").is_empty());
    assert_eq!(matches(&view, "tracked needle"), ["source.txt"]);
    fs::remove_file(fixture.root.join(".gitignore")).unwrap();
    view.refresh().unwrap();
    assert!(
        view.with_snapshot(|snapshot| snapshot.files("", true))
            .unwrap()
            .contains(&alias.to_string())
    );
    if !walker::is_binary_extension(Path::new(alias)) {
        assert_eq!(matches(&view, "ignored alias secret"), [alias]);
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_checkout_paths_cannot_alias_literal_replacement_paths() {
    use std::os::unix::ffi::OsStrExt;
    assert_native_alias_is_rejected(
        std::ffi::OsStr::from_bytes(b"native-\xff.txt"),
        "native-\u{fffd}.txt",
    );
    assert_native_alias_is_rejected(
        std::ffi::OsStr::from_bytes(b"native-\xff.bin"),
        "native-\u{fffd}.bin",
    );
}

#[cfg(unix)]
#[test]
fn literal_backslash_checkout_paths_cannot_alias_directory_paths() {
    assert_native_alias_is_rejected(
        std::ffi::OsStr::new(r"ignored\alias.txt"),
        "ignored/alias.txt",
    );
    assert_native_alias_is_rejected(
        std::ffi::OsStr::new(r"ignored\alias.bin"),
        "ignored/alias.bin",
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_directory_paths_are_rejected_before_visibility_or_descent() {
    use std::os::unix::ffi::OsStrExt;
    let fixture = Fixture::new();
    write(&fixture.root, "source.txt", b"tracked needle");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    let native = fixture
        .root
        .join(std::ffi::OsStr::from_bytes(b".native-\xff"));
    match fs::create_dir(&native) {
        Ok(()) => {}
        Err(error) if cfg!(target_os = "macos") && error.raw_os_error() == Some(92) => {
            eprintln!("APFS rejected the non-UTF-8 fixture directory: {error}");
            return;
        }
        Err(error) => panic!("create native directory fixture: {error}"),
    }
    fs::write(native.join("one.txt"), b"first").unwrap();
    fs::write(native.join("two.txt"), b"second").unwrap();
    let metadata = walker::walk_file_metadata(view.root(), &view.options.walk);
    assert_eq!(
        metadata.skipped_error, 1,
        "reject the directory without descending"
    );
    assert_eq!(metadata.listed_files, ["source.txt"]);
    assert!(
        metadata.visibility.is_empty(),
        "do not record a lossy hidden path"
    );
    assert!(matches!(
        view.refresh(),
        Err(WorktreeError::IncompleteWalk(1))
    ));
    assert!(!view.status().unwrap().ready);
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

fn same_stat_write(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&path, bytes).unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

fn query_in_thread(view: &WorktreeView, pattern: &str) -> Vec<String> {
    std::thread::scope(|scope| scope.spawn(|| matches(view, pattern)).join().unwrap())
}

#[test]
fn serving_verification_keeps_unchanged_views_ready_and_repairs_missed_changes() {
    let fixture = Fixture::new();
    write(&fixture.root, "one.txt", b"alpha needle\n");
    write(&fixture.root, "two.txt", b"beta needle\n");
    write(&fixture.root, "nested/deep/three.txt", b"gamma needle\n");
    write(&fixture.root, "binary.dat", b"bin\0needle");
    write(&fixture.root, ".gitignore", b"*.log\n");
    write(&fixture.root, "ignored.log", b"ignored needle\n");
    commit(&fixture.root);
    write(&fixture.root, "untracked.txt", b"overlay needle\n");
    let view = fixture.view(&fixture.root, &fixture.generation());
    view.refresh().unwrap();
    let published = view.status().unwrap().published_epoch;
    let expected = query_in_thread(&view, "needle");
    assert_eq!(
        expected,
        [
            "nested/deep/three.txt",
            "one.txt",
            "two.txt",
            "untracked.txt"
        ]
    );

    let mut during = None;
    let verified = view
        .verify_inner(
            || {
                assert!(view.status().unwrap().ready);
                during = Some(query_in_thread(&view, "needle"));
            },
            || assert!(view.status().unwrap().ready),
            None,
        )
        .unwrap();
    assert_eq!(during.as_deref(), Some(expected.as_slice()));
    let Verification::Unchanged(stats) = verified else {
        panic!("unchanged checkout must verify while serving: {verified:?}");
    };
    assert!(stats.full);
    assert_eq!(stats.content_reads_avoided, 0, "no FileVersion shortcut");
    assert_eq!(stats.files_read, 6, "every admitted file is reread");
    assert_eq!(stats.files_extracted, 0);
    let status = view.status().unwrap();
    assert!(status.ready && !status.full_required);
    assert_eq!(status.published_epoch, published, "no republication");

    // A same-stat edit no watcher reported, invisible to Git status.
    git(
        &fixture.root,
        &["update-index", "--assume-unchanged", "one.txt"],
    );
    same_stat_write(&fixture.root, "one.txt", b"zzzzzz needle\n");
    assert!(
        git(
            &fixture.root,
            &["status", "--porcelain", "--untracked-files=no"]
        )
        .is_empty()
    );
    assert!(
        candidates(&view, "zzz").is_empty(),
        "the missed edit is stale"
    );
    let mut closed = false;
    let repaired = view
        .verify_inner(
            || {},
            || {
                let status = view.status().unwrap();
                closed = !status.ready
                    && status.full_required
                    && matches!(view.with_snapshot(|_| ()), Err(WorktreeError::NotReady));
            },
            None,
        )
        .unwrap();
    assert!(
        closed,
        "a detected difference closes readiness before publication"
    );
    assert!(matches!(repaired, Verification::Published(ref stats) if stats.full));
    assert!(view.status().unwrap().ready);
    assert!(view.status().unwrap().published_epoch > published);
    assert_eq!(matches(&view, "zzz"), ["one.txt"]);
    assert_scan_parity(&view, &["needle", "zzz", "alpha"]);

    // Membership: additions in new directories and deletions.
    write(&fixture.root, "added/dir/four.txt", b"delta needle\n");
    fs::remove_file(fixture.root.join("two.txt")).unwrap();
    assert!(matches!(view.verify().unwrap(), Verification::Published(_)));
    assert_eq!(matches(&view, "delta"), ["added/dir/four.txt"]);
    assert!(candidates(&view, "beta").is_empty());
    // Ignore rules changing admitted membership.
    write(&fixture.root, ".gitignore", b"*.log\nadded/\n");
    assert!(matches!(view.verify().unwrap(), Verification::Published(_)));
    assert!(candidates(&view, "delta").is_empty());
    // Reverting an overlay to the committed base content.
    git(
        &fixture.root,
        &["update-index", "--no-assume-unchanged", "one.txt"],
    );
    same_stat_write(&fixture.root, "one.txt", b"alpha needle\n");
    assert!(matches!(view.verify().unwrap(), Verification::Published(_)));
    assert!(candidates(&view, "zzz").is_empty());
    assert_scan_parity(&view, &["needle", "zzz", "alpha", "delta", "beta"]);
    assert!(matches!(view.verify().unwrap(), Verification::Unchanged(_)));
}

#[test]
fn serving_verification_yields_to_invalidations_and_fails_closed() {
    let fixture = Fixture::new();
    write(&fixture.root, "one.txt", b"alpha needle\n");
    write(&fixture.root, "two.txt", b"beta needle\n");
    commit(&fixture.root);
    let view = fixture.view(&fixture.root, &fixture.generation());
    assert_eq!(view.verify().unwrap(), Verification::Ineligible);
    view.refresh().unwrap();

    view.invalidate_path(Path::new("one.txt")).unwrap();
    assert_eq!(view.verify().unwrap(), Verification::Ineligible);
    assert_eq!(view.status().unwrap().pending_paths, 1);
    assert!(!view.refresh().unwrap().full);

    // A concurrent event during an unchanged verification keeps its hint.
    let result = view.verify_inner(
        || {
            view.invalidate_path(Path::new("one.txt")).unwrap();
        },
        || {},
        None,
    );
    assert!(matches!(result, Err(WorktreeError::ChangedDuringReconcile)));
    let status = view.status().unwrap();
    assert!(!status.ready && !status.full_required);
    assert_eq!(status.pending_paths, 1);
    assert!(!view.refresh().unwrap().full);

    // A concurrent event after an observed difference requires a full pass.
    same_stat_write(&fixture.root, "two.txt", b"zzzzz needle\n");
    let result = view.verify_inner(
        || {},
        || {
            view.invalidate_path(Path::new("one.txt")).unwrap();
        },
        None,
    );
    assert!(matches!(result, Err(WorktreeError::ChangedDuringReconcile)));
    let status = view.status().unwrap();
    assert!(!status.ready && status.full_required);
    assert!(view.refresh().unwrap().full);
    assert_eq!(matches(&view, "zzz"), ["two.txt"]);

    // Read failures are not budget exhaustion: fail closed.
    let error = view
        .verify_inner(
            || fs::remove_file(fixture.root.join("two.txt")).unwrap(),
            || {},
            None,
        )
        .unwrap_err();
    assert!(matches!(error, WorktreeError::Io(_)), "{error:?}");
    let status = view.status().unwrap();
    assert!(!status.ready && status.full_required);
    assert!(matches!(
        view.with_snapshot(|_| ()),
        Err(WorktreeError::NotReady)
    ));
    assert!(view.refresh().unwrap().full);
    assert!(candidates(&view, "zzz").is_empty());
    assert_scan_parity(&view, &["needle", "zzz"]);
}

#[test]
fn managed_serving_verification_stays_available_and_respects_budget_semantics() {
    use crate::managed::faults::{Action, Point, Specification, Stage};
    for policy_change in [false, true] {
        let fixture = Fixture::new();
        for index in 0..40 {
            write(
                &fixture.root,
                &format!("siblings/file-{index:03}.txt"),
                b"stable needle\n",
            );
        }
        commit(&fixture.root);
        let (namespace, permit, view) = fixture.controlled();
        view.refresh_controlled(&permit).unwrap();
        let published = view.status().unwrap().published_epoch;
        let fault = namespace
            .install_test_fault(Specification {
                point: Point::ReconcileBatchOpened,
                operation: Some(permit.operation_id().clone()),
                skip_hits: 0,
                action: Action::Pause { timeout_ms: 30_000 },
            })
            .unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| view.verify_controlled(&permit));
            let deadline = Instant::now() + std::time::Duration::from_secs(20);
            while namespace.test_fault_status().unwrap().unwrap().stage != Stage::Waiting {
                assert!(
                    Instant::now() < deadline,
                    "verification did not open a batch"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(view.status().unwrap().ready);
            assert_eq!(
                matches(&view, "needle").len(),
                40,
                "serving mid-verification"
            );
            if policy_change {
                let mut policy = namespace.policy().unwrap();
                policy.policy.work.blob_bytes /= 2;
                namespace
                    .update_policy(policy.version, policy.policy)
                    .unwrap();
            } else {
                permit.cancel();
            }
            namespace.release_test_fault(&fault.ticket).unwrap();
            let error = worker.join().unwrap().unwrap_err();
            let expected = if policy_change {
                crate::managed::ErrorCategory::StaleVersion
            } else {
                crate::managed::ErrorCategory::Cancelled
            };
            assert!(
                matches!(error, WorktreeError::Index(crate::Error::Managed(ref error))
                if error.category == expected),
                "{error:?}"
            );
        });
        let status = view.status().unwrap();
        if policy_change {
            assert!(
                !status.ready && status.full_required,
                "stale policy fails closed"
            );
        } else {
            assert!(
                status.ready && !status.full_required,
                "cancellation proves nothing"
            );
            assert_eq!(status.published_epoch, published);
            assert_eq!(matches(&view, "needle").len(), 40);
        }
        drop((view, permit));
        assert_eq!(namespace.work_usage().unwrap().reservations, 0);
    }
}

#[test]
fn managed_serving_verification_budget_failure_after_a_difference_fails_closed() {
    use crate::managed::faults::{Action, Point, Specification, Stage};
    for membership in [true, false] {
        let fixture = Fixture::new();
        for index in 0..40 {
            write(
                &fixture.root,
                &format!("siblings/file-{index:03}.txt"),
                b"stable needle\n",
            );
        }
        commit(&fixture.root);
        let (namespace, permit, view) = fixture.controlled();
        view.refresh_controlled(&permit).unwrap();
        let assert_closed = |context: &str| {
            let status = view.status().unwrap();
            assert!(
                !status.ready && status.full_required,
                "{context}: {status:?}"
            );
            assert!(
                matches!(view.with_snapshot(|_| ()), Err(WorktreeError::NotReady)),
                "{context}"
            );
        };
        let error = if membership {
            // Discovery observes an unreported addition before any read.
            write(&fixture.root, "siblings/added.txt", b"added needle\n");
            let fault = namespace
                .install_test_fault(Specification {
                    point: Point::ReconcileBatchOpened,
                    operation: Some(permit.operation_id().clone()),
                    skip_hits: 0,
                    action: Action::Pause { timeout_ms: 30_000 },
                })
                .unwrap();
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| view.verify_controlled(&permit));
                let deadline = Instant::now() + std::time::Duration::from_secs(20);
                while namespace.test_fault_status().unwrap().unwrap().stage != Stage::Waiting {
                    assert!(
                        Instant::now() < deadline,
                        "verification did not open a batch"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                assert_closed("difference observed before cancellation");
                permit.cancel();
                namespace.release_test_fault(&fault.ticket).unwrap();
                worker.join().unwrap().unwrap_err()
            })
        } else {
            use crate::managed::faults::{Action, Point, Specification, Stage};

            // The changed file is hashed, then cancellation interrupts the
            // fallible base lookup before the rest of the batch can complete.
            same_stat_write(&fixture.root, "siblings/file-007.txt", b"zzzzzz needle\n");
            let fault = namespace
                .install_test_fault(Specification {
                    point: Point::VerificationContentHashed,
                    operation: Some(permit.operation_id().clone()),
                    skip_hits: 0,
                    action: Action::Pause { timeout_ms: 30_000 },
                })
                .unwrap();
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| view.verify_controlled(&permit));
                let deadline = Instant::now() + std::time::Duration::from_secs(20);
                while namespace.test_fault_status().unwrap().unwrap().stage != Stage::Waiting {
                    assert!(
                        Instant::now() < deadline,
                        "verification did not hash the changed content"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                assert_closed("changed content closes readiness before base lookup");
                permit.cancel();
                namespace.release_test_fault(&fault.ticket).unwrap();
                worker.join().unwrap().unwrap_err()
            })
        };
        assert!(
            matches!(error, WorktreeError::Index(crate::Error::Managed(ref error))
            if error.category == crate::managed::ErrorCategory::Cancelled),
            "{error:?}"
        );
        assert!(is_budget_failure(&error));
        assert_closed("budget failure after a difference never restores readiness");
        assert_eq!(
            view.verify_controlled(&permit).unwrap(),
            Verification::Ineligible
        );
        drop((view, permit));
        assert_eq!(namespace.work_usage().unwrap().reservations, 0);
    }
}

#[test]
fn content_only_profile_admits_extension_named_text_through_base_overlay_and_hints() {
    use crate::generations::ContentEligibility;
    let fixture = Fixture::new();
    write(
        &fixture.root,
        "docs/report.doc",
        b"{\\rtf1\\ansi tracked rtf needle}\n",
    );
    write(&fixture.root, "data/table.pdf", b"tracked pdf needle\n");
    write(&fixture.root, "data/early.doc", b"\0nul needle\n");
    write(&fixture.root, "plain.txt", b"plain needle\n");
    commit(&fixture.root);
    write(
        &fixture.root,
        "docs/untracked.dat",
        b"untracked dat needle\n",
    );
    write(
        &fixture.root,
        "docs/untracked.exe",
        b"untracked\0exe needle\n",
    );

    let default_pin = fixture.generation();
    let content_profile = IndexingProfile {
        eligibility: ContentEligibility::ContentOnly,
        ..IndexingProfile::default()
    };
    let content_pin = fixture.generation_with(content_profile);
    assert_eq!(*content_pin.key().profile(), content_profile);
    assert_ne!(
        content_pin.key().storage_name(),
        default_pin.key().storage_name()
    );

    let legacy = fixture.view(&fixture.root, &default_pin);
    legacy.refresh().unwrap();
    assert_eq!(matches(&legacy, "needle"), ["plain.txt"]);
    assert_scan_parity(&legacy, &["needle", "rtf1", ".*"]);

    let view = fixture.view(&fixture.root, &content_pin);
    let first = view.refresh().unwrap();
    assert_eq!(
        matches(&view, "needle"),
        [
            "data/table.pdf",
            "docs/report.doc",
            "docs/untracked.dat",
            "plain.txt"
        ]
    );
    // Clean tracked text is served by the base, not copied into the overlay.
    assert_eq!(first.files_extracted, 1, "{first:?}");
    assert_eq!(view.overlay_cost().unwrap().files, 1, "only untracked text");
    assert_eq!(legacy.overlay_cost().unwrap().files, 0);
    let files = |view: &WorktreeView| {
        view.with_snapshot(|snapshot| snapshot.files("", true))
            .unwrap()
    };
    assert_eq!(files(&view), files(&legacy));
    assert_scan_parity(&view, &["needle", "rtf1", "nul", ".*"]);

    write(
        &fixture.root,
        "docs/report.doc",
        b"{\\rtf1 revised marker}\n",
    );
    write(&fixture.root, "data/table.pdf", b"pdf\0needle\n");
    view.invalidate_path(Path::new("docs/report.doc")).unwrap();
    view.invalidate_path(Path::new("data/table.pdf")).unwrap();
    let hinted = view.refresh().unwrap();
    assert!(!hinted.full);
    assert_eq!(matches(&view, "revised marker"), ["docs/report.doc"]);
    assert_eq!(
        matches(&view, "needle"),
        ["docs/untracked.dat", "plain.txt"]
    );
    assert_scan_parity(&view, &["needle", "revised", "rtf1", ".*"]);

    // Unreported edits are repaired by full verification like any other path.
    write(&fixture.root, "data/early.doc", b"now plain needle\n");
    view.invalidate_all().unwrap();
    view.refresh().unwrap();
    assert_eq!(
        matches(&view, "needle"),
        ["data/early.doc", "docs/untracked.dat", "plain.txt"]
    );
    assert_scan_parity(&view, &["needle", ".*"]);
    legacy.invalidate_all().unwrap();
    legacy.refresh().unwrap();
    assert_eq!(matches(&legacy, "needle"), ["plain.txt"]);

    // A checkpoint is bound to the exact profile it was verified under.
    let directory = fixture._temp.path().join("content checkpoint");
    fs::create_dir(&directory).unwrap();
    let options = WorktreeOptions {
        checkpoint_directory: Some(directory),
        ..WorktreeOptions::default()
    };
    let saved = WorktreeView::new(&fixture.root, content_pin.clone(), options.clone()).unwrap();
    saved.refresh().unwrap();
    saved.save_checkpoint().unwrap();
    assert!(WorktreeView::restore(&fixture.root, default_pin, options.clone()).is_err());
    let restored = WorktreeView::restore(&fixture.root, content_pin, options).unwrap();
    restored.refresh().unwrap();
    assert_eq!(matches(&restored, "revised marker"), ["docs/report.doc"]);
}
