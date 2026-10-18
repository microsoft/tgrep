use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;
use tempfile::TempDir;
use tgrep_core::{builder, meta::IndexMeta};

struct Fixture {
    temp: TempDir,
    root: PathBuf,
    index: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join("nested/dir")).unwrap();
        fs::write(root.join("nested/dir/file.txt"), "needle inside\n").unwrap();
        fs::write(root.join("kept.txt"), "needle kept\n").unwrap();
        let index = root.join(".tgrep");
        builder::build_index(&root, Some(&index), true, false, &[]).unwrap();
        Self { temp, root, index }
    }

    fn query(&self, root: &Path, args: &[&str]) -> Output {
        Command::cargo_bin("tgrep")
            .unwrap()
            .timeout(Duration::from_secs(30))
            .current_dir(&self.root)
            .arg("--index-path")
            .arg(&self.index)
            .arg("--stats")
            .args(args)
            .arg(root)
            .output()
            .unwrap()
    }
}

fn link_directory(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let output = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/j"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}

#[test]
fn local_index_rejects_directory_links_in_whole_and_subtree_searches() {
    let fixture = Fixture::new();
    let outside = fixture.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("file.txt"), "needle OUTSIDE_SENTINEL\n").unwrap();
    let link = fixture.root.join("nested").join("dir");
    fs::rename(&link, fixture.temp.path().join("saved")).unwrap();
    link_directory(&outside, &link);
    for root in [&fixture.root, &fixture.root.join("nested")] {
        for args in [
            vec!["-F", "--", "needle"],
            vec!["--", "."],
            vec!["--passthru", "--", "needle"],
            vec!["--sort", "modified", "--", "needle"],
        ] {
            let output = fixture.query(root, &args);
            assert_eq!(
                output.status.code(),
                Some(if root == &fixture.root { 0 } else { 1 })
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(!stdout.contains("OUTSIDE_SENTINEL"), "{stdout}");
            assert!(stderr.contains("cannot read indexed file"), "{stderr}");
            assert!(
                stderr.contains("Query plan:"),
                "must exercise the index: {stderr}"
            );
            if root == &fixture.root {
                assert!(stdout.contains("needle kept"), "{stdout}");
            }
        }
    }
    #[cfg(unix)]
    fs::remove_file(link).unwrap();
    #[cfg(windows)]
    fs::remove_dir(link).unwrap();
}

#[cfg(unix)]
#[test]
fn local_index_rejects_final_file_symlinks() {
    let fixture = Fixture::new();
    let outside = fixture.temp.path().join("outside.txt");
    fs::write(&outside, "needle OUTSIDE_SENTINEL\n").unwrap();
    let link = fixture.root.join("nested/dir/file.txt");
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let output = fixture.query(&fixture.root, &["--", "."]);
    assert!(output.status.success());
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("OUTSIDE_SENTINEL")
    );
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("cannot read indexed file")
    );
}

/// Pipe backpressure holds the real CLI inside the first sibling's output
/// while the test replaces their parent, so the next sibling must be resolved
/// again instead of read through the replaced parent. Windows pins an open
/// parent against replacement, so the transition is Unix-only.
#[cfg(unix)]
#[test]
fn local_index_streams_siblings_without_reading_through_a_replaced_parent() {
    use std::io::Read;
    use std::process::{Command as Process, Stdio};
    use std::time::Instant;

    const FIRST_LINES: usize = 100_000;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let parent = root.join("parent");
    fs::create_dir_all(&parent).unwrap();
    // Megabytes of output: far beyond a pipe, the stdout buffer and the probe.
    fs::write(parent.join("a.txt"), "needle first\n".repeat(FIRST_LINES)).unwrap();
    fs::write(parent.join("b.txt"), "needle ORIGINAL_SENTINEL\n").unwrap();
    let index = root.join(".tgrep");
    builder::build_index(&root, Some(&index), true, false, &[]).unwrap();

    let mut child = Process::new(env!("CARGO_BIN_EXE_tgrep"))
        .current_dir(&root)
        .arg("--index-path")
        .arg(&index)
        .args(["--stats", "--sort", "path", "-F", "--", "needle"])
        .arg(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (status, stdout, stderr) = std::thread::scope(|scope| {
        let errors = scope.spawn(move || {
            let mut text = String::new();
            stderr.read_to_string(&mut text).unwrap();
            text
        });
        let output = scope.spawn(|| {
            // First-sibling output proves its parent was already resolved;
            // the unread remainder keeps the search inside that sibling.
            let mut first = vec![0; 4096];
            stdout.read_exact(&mut first).unwrap();
            fs::rename(&parent, temp.path().join("saved")).unwrap();
            fs::create_dir(&parent).unwrap();
            fs::write(parent.join("b.txt"), "needle REPLACEMENT_SENTINEL\n").unwrap();
            stdout.read_to_end(&mut first).unwrap();
            String::from_utf8(first).unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("indexed search did not finish");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        (status, output.join().unwrap(), errors.join().unwrap())
    });

    assert!(
        !stdout.contains("ORIGINAL_SENTINEL"),
        "printed a sibling through the replaced parent; status {status:?}: {stderr}"
    );
    assert!(status.success(), "{status:?}: {stderr}");
    assert!(
        stderr.contains("Query plan:"),
        "must exercise the index: {stderr}"
    );
    assert!(stdout.contains("needle REPLACEMENT_SENTINEL"), "{stderr}");
    assert_eq!(stdout.matches("needle first").count(), FIRST_LINES);
}

#[test]
fn unavailable_relative_and_unrelated_metadata_roots_fall_back_to_scanning() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("fresh.txt"), "needle fresh\n").unwrap();
    let unrelated = fixture.temp.path().join("unrelated");
    fs::create_dir(&unrelated).unwrap();
    for root in [
        fixture.temp.path().join("missing"),
        PathBuf::from("relative-root"),
        unrelated,
    ] {
        let mut meta = IndexMeta::load(&fixture.index).unwrap();
        meta.root_path = root.to_string_lossy().into_owned();
        meta.save(&fixture.index).unwrap();
        let output = fixture.query(&fixture.root, &["-F", "--", "needle"]);
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("needle fresh")
        );
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("index root unavailable or unrelated")
        );
    }
}

#[test]
fn ordinary_indexes_beneath_unrelated_store_named_directories_stay_indexed() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("tgrep-managed-v2");
    let root = store.join("project");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("kept.txt"), "needle kept\n").unwrap();
    let tgrep = |args: &[&str]| {
        Command::cargo_bin("tgrep")
            .unwrap()
            .timeout(Duration::from_secs(30))
            .current_dir(&root)
            .args(args)
            .output()
            .unwrap()
    };
    let output = tgrep(&["index", "."]);
    assert!(output.status.success(), "{output:?}");
    let output = tgrep(&["--stats", "-F", "--", "needle", "."]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stdout.contains("needle kept"), "{stdout}");
    assert!(
        stderr.contains("Query plan:"),
        "must exercise the index: {stderr}"
    );
    assert!(!stderr.contains("scanning every file"), "{stderr}");

    // A repository-namespace-shaped path beneath the same name stays reserved.
    let reserved = store.join("0".repeat(64)).join("index");
    let output = tgrep(&["index", ".", "--index-path", reserved.to_str().unwrap()]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("protected managed reader"), "{stderr}");
    assert!(!reserved.join("meta.json").exists());
}

#[test]
fn index_and_serve_preserve_temporary_output_owned_by_another_writer() {
    let fixture = Fixture::new();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(fixture.index.join("serve.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    let spill = fixture.index.join("spill-123-0.tmp");
    let stage = fixture.index.join(".reload-build");
    fs::create_dir(&spill).unwrap();
    fs::create_dir(&stage).unwrap();
    for path in [&spill, &stage] {
        fs::write(path.join("segment.bin"), b"active build").unwrap();
    }
    let before = fs::read(fixture.index.join("meta.json")).unwrap();

    for command in ["index", "serve"] {
        let output = Command::cargo_bin("tgrep")
            .unwrap()
            .timeout(Duration::from_secs(30))
            .current_dir(&fixture.root)
            .args([command, ".", "--index-path"])
            .arg(&fixture.index)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("another tgrep server is already running or an index build"),
            "{stderr}"
        );
        for path in [&spill, &stage] {
            assert_eq!(fs::read(path.join("segment.bin")).unwrap(), b"active build");
        }
        assert_eq!(fs::read(fixture.index.join("meta.json")).unwrap(), before);
    }

    drop(lock);
    let output = Command::cargo_bin("tgrep")
        .unwrap()
        .timeout(Duration::from_secs(30))
        .current_dir(&fixture.root)
        .args(["index", ".", "--index-path"])
        .arg(&fixture.index)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!spill.exists());
    assert!(!stage.exists());
    let output = fixture.query(&fixture.root, &["-F", "--", "needle"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("Query plan:")
    );
}

#[test]
fn index_and_serve_reject_linked_staging_without_touching_external_files() {
    let fixture = Fixture::new();
    let outside = fixture.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("index.bin"), b"external sentinel").unwrap();
    link_directory(&outside, &fixture.index.join(".bootstrap-build"));
    let mut metadata = IndexMeta::load(&fixture.index).unwrap();
    metadata.complete = false;
    metadata.save(&fixture.index).unwrap();
    let before = fs::read(fixture.index.join("meta.json")).unwrap();

    for command in ["index", "serve"] {
        let output = Command::cargo_bin("tgrep")
            .unwrap()
            .timeout(Duration::from_secs(30))
            .current_dir(&fixture.root)
            .args([command, ".", "--index-path"])
            .arg(&fixture.index)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("fixed index staging path"), "{stderr}");
        assert_eq!(
            fs::read(outside.join("index.bin")).unwrap(),
            b"external sentinel"
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
        assert_eq!(fs::read(fixture.index.join("meta.json")).unwrap(), before);
        assert!(!fixture.index.join("serve.json").exists());
    }
}
