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

#[test]
fn local_index_rejects_directory_links_in_whole_and_subtree_searches() {
    let fixture = Fixture::new();
    let outside = fixture.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("file.txt"), "needle OUTSIDE_SENTINEL\n").unwrap();
    let link = fixture.root.join("nested").join("dir");
    fs::rename(&link, fixture.temp.path().join("saved")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    #[cfg(windows)]
    {
        let output = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/j"])
            .arg(&link)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
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
