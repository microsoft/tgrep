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
