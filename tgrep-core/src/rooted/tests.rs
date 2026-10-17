use super::*;
use std::io::Read;

pub(crate) fn link_directory(target: &Path, link: &Path) {
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
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(unix)]
pub(crate) fn make_fifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: a live NUL-terminated fixture path and a valid file mode.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

/// A broken nonblocking open must fail the test, not hang the entire suite.
#[cfg(unix)]
pub(crate) fn bounded_child(test: &str) -> bool {
    const MARKER: &str = "TGREP_ROOTED_READ_TEST_CHILD";
    if std::env::var(MARKER).as_deref() == Ok(test) {
        return true;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(MARKER, test)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{test}: {status}");
            return false;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{test} blocked past its deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn contents(mut file: File) -> String {
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    text
}

#[test]
fn regular_opens_and_literal_path_validation() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("dir")).unwrap();
    fs::write(temp.path().join("dir/file"), "inside").unwrap();
    let root = RootedDir::open(temp.path()).unwrap();
    assert_eq!(
        contents(root.open_file(Path::new("dir/file")).unwrap()),
        "inside"
    );
    for path in ["", ".", "./dir/file", "../file", "dir/../file", "dir"] {
        assert!(root.open_file(Path::new(path)).is_err(), "{path}");
    }
    assert!(root.open_file(&temp.path().join("dir/file")).is_err());
    #[cfg(windows)]
    for path in [
        "dir/file:stream",
        "dir/file::$DATA",
        "C:dir/file",
        "\\dir\\file",
    ] {
        assert!(root.open_file(Path::new(path)).is_err(), "{path}");
        assert!(validate_index_path(path).is_err(), "{path}");
    }
}

#[test]
fn query_sibling_batches_preserve_order_and_bound_pin_lifetimes() {
    let mut paths: Vec<_> = (0..130).map(|i| format!("first/file-{i}")).collect();
    paths.extend([
        "second/file".into(),
        "first/last".into(),
        "root-file".into(),
    ]);
    let batches: Vec<_> = sibling_batches(&paths).collect();
    assert_eq!(
        batches.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [64, 64, 2, 1, 1, 1]
    );
    assert_eq!(
        batches.into_iter().flatten().cloned().collect::<Vec<_>>(),
        paths
    );
    assert_eq!(sibling_batches(&[]).count(), 0);
}

#[test]
fn query_sibling_reader_retains_only_its_scoped_parent_pins() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("outer").join("inner");
    fs::create_dir_all(&parent).unwrap();
    fs::write(parent.join("one"), "one").unwrap();
    fs::write(parent.join("two"), "two").unwrap();
    let root = RootedDir::open(temp.path()).unwrap();
    let batch = root.read_batch(Path::new("outer/inner")).unwrap();
    assert_eq!(
        contents(batch.open_file(Path::new("outer/inner/one")).unwrap()),
        "one"
    );
    assert_eq!(
        contents(batch.open_file(Path::new("outer/inner/two")).unwrap()),
        "two"
    );
    for path in ["", "../one", "outer/one", "outer/inner/../one"] {
        assert!(batch.open_file(Path::new(path)).is_err(), "{path}");
    }
    #[cfg(windows)]
    assert!(fs::rename(&parent, temp.path().join("saved")).is_err());
    batch.finish().unwrap();
    fs::rename(&parent, temp.path().join("saved")).unwrap();
}

#[test]
fn owned_query_batch_retains_root_until_verified_finish() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("one"), "one").unwrap();
    let root = Arc::new(RootedDir::open(temp.path()).unwrap());
    let weak = Arc::downgrade(&root);
    let batch = root.owned_batch(Path::new("")).unwrap();
    drop(root);
    assert!(weak.upgrade().is_some());
    assert_eq!(contents(batch.open_file(Path::new("one")).unwrap()), "one");
    batch.finish().unwrap();
    assert!(weak.upgrade().is_none());
}

#[cfg(unix)]
#[test]
fn query_batch_finish_rejects_a_replaced_parent_without_following_it() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("parent");
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join("one"), "original").unwrap();
    let root = RootedDir::open(temp.path()).unwrap();
    let batch = root.read_batch(Path::new("parent")).unwrap();
    fs::rename(&parent, temp.path().join("saved")).unwrap();
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join("one"), "replacement").unwrap();
    assert_eq!(
        contents(batch.open_file(Path::new("parent/one")).unwrap()),
        "original"
    );
    assert!(batch.finish().is_err());
}

#[test]
fn streaming_file_open_does_not_reuse_a_replaced_parent_handle() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("parent");
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join("one"), "original").unwrap();
    let root = RootedDir::open(temp.path()).unwrap();

    assert_eq!(
        contents(root.open_file(Path::new("parent/one")).unwrap()),
        "original"
    );
    fs::rename(&parent, temp.path().join("saved")).unwrap();
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join("one"), "replacement").unwrap();

    assert_eq!(
        contents(root.open_file(Path::new("parent/one")).unwrap()),
        "replacement"
    );
}

#[test]
fn sibling_batches_release_pins_and_recheck_ancestor_identity() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("outer").join("inner");
    fs::create_dir_all(&parent).unwrap();
    fs::write(parent.join("one"), "one").unwrap();
    fs::write(parent.join("two"), "two").unwrap();
    let root = RootedDir::open(temp.path()).unwrap();
    let batch = root.batch(Path::new("outer/inner")).unwrap();
    assert_eq!(
        contents(batch.open_file(Path::new("outer/inner/one")).unwrap()),
        "one"
    );
    assert_eq!(
        contents(batch.open_file(Path::new("outer/inner/two")).unwrap()),
        "two"
    );
    for path in ["outer/one", "outer/inner/../one", "../one", "one"] {
        assert!(batch.open_file(Path::new(path)).is_err(), "{path}");
    }
    #[cfg(windows)]
    assert!(fs::rename(&parent, temp.path().join("saved")).is_err());
    let observation = batch.finish().unwrap();
    observation.check_current_ids(&root).unwrap();
    fs::rename(&parent, temp.path().join("saved")).unwrap();
    fs::create_dir(&parent).unwrap();
    assert!(observation.check_current_ids(&root).is_err());
}

#[test]
fn ancestor_observations_are_point_in_time_not_lifetime_pins() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("parent");
    let saved = temp.path().join("saved");
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join("file"), "original").unwrap();
    let root = RootedDir::open(temp.path()).unwrap();
    let batch = root.batch(Path::new("parent")).unwrap();
    assert_eq!(
        contents(batch.open_file(Path::new("parent/file")).unwrap()),
        "original"
    );
    let observation = batch.finish().unwrap();
    fs::rename(&parent, &saved).unwrap();
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join("file"), "replacement").unwrap();
    assert!(observation.check_current_ids(&root).is_err());
    fs::remove_file(parent.join("file")).unwrap();
    fs::remove_dir(&parent).unwrap();
    fs::rename(&saved, &parent).unwrap();
    // Restoring the original object restores ID equality, not path continuity.
    observation.check_current_ids(&root).unwrap();
    assert_eq!(
        contents(root.open_file(Path::new("parent/file")).unwrap()),
        "original"
    );
}

#[test]
fn completed_batch_never_accepts_a_reparse_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("parent");
    let other = temp.path().join("other");
    fs::create_dir(&parent).unwrap();
    fs::create_dir(&other).unwrap();
    fs::write(parent.join("file"), "original").unwrap();
    fs::write(other.join("file"), "redirected").unwrap();
    let root = RootedDir::open(temp.path()).unwrap();
    let batch = root.batch(Path::new("parent")).unwrap();
    assert_eq!(
        contents(batch.open_file(Path::new("parent/file")).unwrap()),
        "original"
    );
    let observation = batch.finish().unwrap();
    fs::rename(&parent, temp.path().join("saved")).unwrap();
    link_directory(&other, &parent);
    assert!(root.batch(Path::new("parent")).is_err());
    assert!(observation.check_current_ids(&root).is_err());
}

#[cfg(unix)]
#[test]
fn batch_child_fifo_open_is_nonblocking_and_never_follows_symlinks() {
    if !bounded_child(
        "rooted::tests::batch_child_fifo_open_is_nonblocking_and_never_follows_symlinks",
    ) {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = RootedDir::open(temp.path()).unwrap();
    let batch = root.batch(Path::new("")).unwrap();
    make_fifo(&temp.path().join("fifo"));
    assert!(batch.open_file(Path::new("fifo")).is_err());
    fs::write(temp.path().join("target"), "outside").unwrap();
    std::os::unix::fs::symlink("target", temp.path().join("link")).unwrap();
    assert!(batch.open_file(Path::new("link")).is_err());
}

#[test]
fn directory_swap_before_open_rejects_outside_and_inside_links() {
    for inside in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("root");
        let directory = path.join("dir");
        let target = if inside {
            path.join("target")
        } else {
            temp.path().join("target")
        };
        fs::create_dir_all(&directory).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(directory.join("file"), "original").unwrap();
        fs::write(target.join("file"), "redirected").unwrap();
        let root = RootedDir::open(&path).unwrap();
        let opened = root.open_file_with(Path::new("dir/file"), |index| {
            if index == 0 {
                fs::rename(&directory, path.join("saved")).unwrap();
                link_directory(&target, &directory);
            }
        });
        assert_eq!(
            fs::read_to_string(directory.join("file")).unwrap(),
            "redirected"
        );
        assert!(
            opened.is_err(),
            "a substituted directory link must not be followed"
        );
    }
}

#[test]
fn directory_swap_after_open_cannot_redirect_parent_handle() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("root");
    let directory = path.join("dir");
    let target = temp.path().join("outside");
    fs::create_dir_all(&directory).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(directory.join("file"), "original").unwrap();
    fs::write(target.join("file"), "outside").unwrap();
    let root = RootedDir::open(&path).unwrap();
    let file = root
        .open_file_with(Path::new("dir/file"), |index| {
            if index == 1 {
                let renamed = fs::rename(&directory, path.join("saved"));
                #[cfg(unix)]
                {
                    renamed.unwrap();
                    link_directory(&target, &directory);
                }
                #[cfg(windows)]
                assert!(
                    renamed.is_err(),
                    "the checked directory handle must deny replacement"
                );
            }
        })
        .unwrap();
    assert_eq!(contents(file), "original");
    #[cfg(unix)]
    assert!(root.open_file(Path::new("dir/file")).is_err());
}

#[test]
fn pinned_root_cannot_be_retargeted_between_verification_and_open() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("root");
    let target = temp.path().join("outside");
    fs::create_dir(&path).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(path.join("file"), "original").unwrap();
    fs::write(target.join("file"), "outside").unwrap();
    let root = RootedDir::open(&path).unwrap();
    let file = root
        .open_file_with(Path::new("file"), |_| {
            let renamed = fs::rename(&path, temp.path().join("saved"));
            #[cfg(unix)]
            {
                renamed.unwrap();
                link_directory(&target, &path);
            }
            #[cfg(windows)]
            assert!(renamed.is_err(), "the root handle must deny replacement");
        })
        .unwrap();
    assert_eq!(contents(file), "original");
    #[cfg(unix)]
    assert!(root.verify_root().is_err());
}

#[cfg(unix)]
#[test]
fn final_symlink_swap_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file");
    fs::write(&path, "original").unwrap();
    fs::write(temp.path().join("target"), "redirected").unwrap();
    let root = RootedDir::open(temp.path()).unwrap();
    let opened = root.open_file_with(Path::new("file"), |_| {
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(temp.path().join("target"), &path).unwrap();
    });
    assert!(opened.is_err());
}

#[cfg(unix)]
#[test]
fn fifo_swap_before_final_open_is_nonblocking() {
    if !bounded_child("rooted::tests::fifo_swap_before_final_open_is_nonblocking") {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("dir")).unwrap();
    let path = temp.path().join("dir/file");
    fs::write(&path, "original").unwrap();
    let root = RootedDir::open(temp.path()).unwrap();
    let opened = root.open_file_with(Path::new("dir/file"), |index| {
        if index == 1 {
            fs::remove_file(&path).unwrap();
            make_fifo(&path);
        }
    });
    assert_eq!(opened.unwrap_err().kind(), ErrorKind::InvalidInput);
}
