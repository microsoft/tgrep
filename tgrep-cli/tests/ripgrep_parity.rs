//! Regression tests for divergences found by differential-testing tgrep
//! against a real `ripgrep` binary (rg 15.2.0).
//!
//! Each block below corresponds to a behaviour that was empirically confirmed
//! by running the same arguments through `rg` and through `tgrep`. The comment
//! above each group records what ripgrep actually does, so the expectation can
//! be re-verified without re-deriving it from the source.

use assert_cmd::Command;
use std::fs;
use std::path::MAIN_SEPARATOR;
use tempfile::TempDir;

fn tgrep() -> Command {
    Command::cargo_bin("tgrep").unwrap()
}

/// A fixture rooted at the temp dir with a `src/` subtree, so relative path
/// arguments (`src`, `src/`, `./src`) can be exercised from `current_dir`.
fn fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(dir.path().join("alpha.txt"), "needle at top\n").unwrap();
    fs::write(src.join("main.rs"), "fn main() {\n    // needle\n}\n").unwrap();
    fs::write(src.join("lib.rs"), "pub fn add() {}\n").unwrap();
    dir
}

fn sep() -> char {
    MAIN_SEPARATOR
}

fn stdout_of(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

// ---------------------------------------------------------------------------
// 1. Path display
//
// ripgrep pushes results onto the path the user typed: the argument survives
// verbatim and only the appended remainder uses the native separator.
//   (no arg) -> alpha.txt        .    -> .\alpha.txt
//   src      -> src\main.rs      src/ -> src/main.rs
//   ./src    -> ./src\main.rs    abs  -> absolute
// ---------------------------------------------------------------------------

#[test]
fn path_display_without_argument_is_bare() {
    let dir = fixture();
    let out =
        stdout_of(
            tgrep()
                .current_dir(dir.path())
                .args(["--no-index", "--no-heading", "needle"]),
        );
    assert!(
        out.contains("alpha.txt:needle at top"),
        "expected a bare relative path, got: {out:?}"
    );
    assert!(
        !out.contains(&format!(".{}alpha.txt", sep())),
        "no argument must not synthesise a ./ prefix, got: {out:?}"
    );
}

#[test]
fn path_display_dot_argument_keeps_the_dot() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "needle",
        ".",
    ]));
    assert!(
        out.contains(&format!(".{}alpha.txt", sep())),
        "expected a `.`-prefixed path, got: {out:?}"
    );
}

#[test]
fn path_display_directory_argument_is_prefixed() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "needle",
        "src",
    ]));
    assert!(
        out.contains(&format!("src{}main.rs", sep())),
        "expected the typed directory as a prefix, got: {out:?}"
    );
}

#[test]
fn path_display_preserves_a_trailing_slash_argument() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "needle",
        "src/",
    ]));
    // The typed argument survives verbatim: the separator the user wrote is
    // the separator that gets printed.
    assert!(
        out.contains("src/main.rs"),
        "expected `src/main.rs`, got: {out:?}"
    );
}

#[test]
fn path_display_mixes_typed_and_native_separators() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "needle",
        "./src",
    ]));
    assert!(
        out.contains(&format!("./src{}main.rs", sep())),
        "the typed `./src` must survive, only the remainder is native: {out:?}"
    );
}

#[test]
fn path_display_of_an_explicit_file_is_exactly_as_typed() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "-H",
        "needle",
        "src/main.rs",
    ]));
    assert!(
        out.contains("src/main.rs:"),
        "an explicit file argument prints exactly as typed, got: {out:?}"
    );
}

#[test]
fn path_display_gives_each_argument_its_own_prefix() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "needle",
        "src",
        "./alpha.txt",
    ]));
    assert!(
        out.contains(&format!("src{}main.rs", sep())),
        "missing the `src` prefix: {out:?}"
    );
    assert!(
        out.contains("./alpha.txt:"),
        "missing the verbatim file argument: {out:?}"
    );
}

#[test]
fn path_display_keeps_absolute_arguments_absolute() {
    let dir = fixture();
    let root = dir.path().to_str().unwrap().to_string();
    let out = stdout_of(tgrep().args(["--no-index", "--no-heading", "needle", &root]));
    assert!(
        out.contains(&root),
        "an absolute argument must stay absolute, got: {out:?}"
    );
}

#[test]
fn path_separator_overrides_every_separator() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "--path-separator",
        "::",
        "-l",
        "needle",
        "./src",
    ]));
    assert!(
        out.contains(".::src::main.rs"),
        "--path-separator must rewrite typed and appended separators: {out:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Line numbers are TTY-dependent
//
// ripgrep enables -n only when stdout is a terminal. --column, --vimgrep and
// -p also imply it; -b and -A/-B/-C do not. Tests never run on a TTY, so the
// default here is "no line numbers".
// ---------------------------------------------------------------------------

#[test]
fn line_numbers_are_off_when_not_a_tty() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "needle",
        "alpha.txt",
    ]));
    assert_eq!(out, "needle at top\n", "unexpected default decoration");
}

#[test]
fn byte_offset_does_not_imply_line_numbers() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "-b",
        "needle",
        "alpha.txt",
    ]));
    assert_eq!(out, "0:needle at top\n", "-b must not turn on -n");
}

#[test]
fn context_does_not_imply_line_numbers() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "-A",
        "1",
        "needle",
        "src/main.rs",
    ]));
    assert_eq!(out, "    // needle\n}\n", "-A must not turn on -n");
}

#[test]
fn column_flag_implies_line_numbers() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "--column",
        "needle",
        "alpha.txt",
    ]));
    assert_eq!(out, "1:1:needle at top\n", "--column must imply -n");
}

// ---------------------------------------------------------------------------
// 3. Filenames are shown unless exactly one *file* was named
// ---------------------------------------------------------------------------

#[test]
fn single_file_argument_suppresses_filenames() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "needle",
        "alpha.txt",
    ]));
    assert!(
        !out.contains("alpha.txt"),
        "a lone file argument must not print its own name: {out:?}"
    );
}

#[test]
fn single_directory_argument_still_shows_filenames() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "needle",
        "src",
    ]));
    assert!(
        out.contains("main.rs"),
        "a directory argument keeps filenames: {out:?}"
    );
}

#[test]
fn two_file_arguments_show_filenames() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "needle",
        "alpha.txt",
        "src/main.rs",
    ]));
    assert!(
        out.contains("alpha.txt:") && out.contains("src/main.rs:"),
        "multiple file arguments keep filenames: {out:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. With -e/-f, every positional is a path
//
// `rg -e needle .` searches for `needle` in `.`; it does NOT also search for
// the literal pattern `.`.
// ---------------------------------------------------------------------------

#[test]
fn dash_e_makes_every_positional_a_path() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "-e",
        "needle",
        ".",
    ]));
    for line in out.lines() {
        assert!(
            line.contains("needle"),
            "`.` was treated as a pattern, matching everything: {line:?}"
        );
    }
    assert_eq!(out.lines().count(), 2, "expected exactly 2 hits: {out:?}");
}

#[test]
fn dash_f_makes_every_positional_a_path() {
    let dir = fixture();
    let pats = dir.path().join("pats.txt");
    fs::write(&pats, "needle\n").unwrap();

    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "-f",
        pats.to_str().unwrap(),
        "src",
    ]));
    assert_eq!(out.lines().count(), 1, "expected exactly 1 hit: {out:?}");
    assert!(out.contains("needle"));
}

// ---------------------------------------------------------------------------
// 5. --index-path with a subdirectory or file argument
//
// The index stores paths relative to the *index* root. Searching a subtree of
// an indexed root previously joined those onto the search root and silently
// produced nothing.
// ---------------------------------------------------------------------------

/// Build an index rooted at the temp dir (not at `src/`), so a `src` argument
/// is a strict subtree of the index root.
fn indexed_fixture() -> (TempDir, String) {
    let dir = fixture();
    let idx = dir.path().join("idx").to_str().unwrap().to_string();
    tgrep()
        .args(["index", dir.path().to_str().unwrap(), "--index-path", &idx])
        .assert()
        .success();
    (dir, idx)
}

#[test]
fn indexed_search_of_a_subdirectory_finds_matches() {
    let (dir, idx) = indexed_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-heading",
        "--index-path",
        &idx,
        "needle",
        "src",
    ]));
    assert!(
        out.contains(&format!("src{}main.rs", sep())),
        "indexed subtree search returned nothing: {out:?}"
    );
    assert!(
        !out.contains("alpha.txt"),
        "results must be scoped to the search subtree: {out:?}"
    );
}

#[test]
fn indexed_search_of_a_single_file_finds_matches() {
    let (dir, idx) = indexed_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-heading",
        "--index-path",
        &idx,
        "-H",
        "needle",
        "src/main.rs",
    ]));
    assert!(
        out.contains("src/main.rs:"),
        "indexed single-file search returned nothing: {out:?}"
    );
    assert!(!out.contains("alpha.txt"), "scope leaked: {out:?}");
}

#[test]
fn indexed_and_brute_force_agree_on_a_subtree() {
    let (dir, idx) = indexed_fixture();
    let run = |extra: &[&str]| -> Vec<String> {
        let mut args: Vec<&str> = vec!["--no-heading"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["needle", "src"]);
        let mut v: Vec<String> = stdout_of(tgrep().current_dir(dir.path()).args(&args))
            .lines()
            .map(str::to_string)
            .collect();
        v.sort();
        v
    };
    assert_eq!(run(&["--no-index"]), run(&["--index-path", &idx]));
}

// ---------------------------------------------------------------------------
// 6. --max-depth applies to the indexed path too
// ---------------------------------------------------------------------------

#[test]
fn indexed_search_honors_max_depth() {
    let (dir, idx) = indexed_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-heading",
        "--index-path",
        &idx,
        "--max-depth",
        "1",
        "needle",
        ".",
    ]));
    assert!(
        out.contains("alpha.txt"),
        "depth-1 file should still match: {out:?}"
    );
    assert!(
        !out.contains("main.rs"),
        "--max-depth must be enforced on the indexed path: {out:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. --hidden / --no-ignore must bypass the index
//
// The index is built skipping hidden and ignored files, so answering these
// flags from the index would silently under-report.
// ---------------------------------------------------------------------------

#[test]
fn hidden_flag_bypasses_the_index() {
    let (dir, idx) = indexed_fixture();
    // Written after indexing, and hidden, so only a bypassing search sees it.
    fs::write(dir.path().join(".secret.txt"), "needle hidden\n").unwrap();

    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-heading",
        "--index-path",
        &idx,
        "--hidden",
        "needle",
        ".",
    ]));
    assert!(
        out.contains("needle hidden"),
        "--hidden must bypass the index: {out:?}"
    );
}

#[test]
fn no_ignore_flag_bypasses_the_index() {
    let dir = fixture();
    fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(dir.path().join("ignored.txt"), "needle ignored\n").unwrap();
    let idx = dir.path().join("idx").to_str().unwrap().to_string();
    tgrep()
        .args(["index", dir.path().to_str().unwrap(), "--index-path", &idx])
        .assert()
        .success();

    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-heading",
        "--index-path",
        &idx,
        "--no-ignore",
        "needle",
        ".",
    ]));
    assert!(
        out.contains("needle ignored"),
        "--no-ignore must bypass the index: {out:?}"
    );
}

// ---------------------------------------------------------------------------
// 8. Binary handling
//
// Verified against rg 15.2.0:
//   rg needle .          -> binary files found by traversal are invisible
//                           (no note, no -l entry, no -c entry)
//   rg needle bin.txt    -> "binary file matches (found "\0" byte around ...)"
//   rg --binary needle . -> traversal files behave like explicit ones
//   rg -a needle .       -> binary detection off entirely
// ---------------------------------------------------------------------------

fn binary_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("bin.txt"), b"needle\n\x00more\n".as_slice()).unwrap();
    fs::write(dir.path().join("plain.txt"), "needle plain\n").unwrap();
    dir
}

#[test]
fn traversal_hides_binary_files() {
    let dir = binary_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "needle",
        ".",
    ]));
    assert_eq!(
        out,
        format!(".{}plain.txt:needle plain\n", sep()),
        "a binary file reached by traversal must be invisible: {out:?}"
    );
}

#[test]
fn traversal_hides_binary_files_from_list_and_count() {
    let dir = binary_fixture();
    let listed =
        stdout_of(
            tgrep()
                .current_dir(dir.path())
                .args(["--no-index", "-l", "needle", "."]),
        );
    assert!(
        !listed.contains("bin.txt"),
        "-l leaked a binary file: {listed:?}"
    );

    let counted =
        stdout_of(
            tgrep()
                .current_dir(dir.path())
                .args(["--no-index", "-c", "needle", "."]),
        );
    assert!(
        !counted.contains("bin.txt"),
        "-c leaked a binary file: {counted:?}"
    );
}

#[test]
fn explicit_binary_file_reports_a_note() {
    let dir = binary_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "needle",
        "bin.txt",
    ]));
    assert_eq!(
        out, "binary file matches (found \"\\0\" byte around offset 7)\n",
        "unexpected note for an explicitly named binary file"
    );
}

#[test]
fn explicit_binary_file_is_listed_and_counted() {
    let dir = binary_fixture();
    let listed =
        stdout_of(
            tgrep()
                .current_dir(dir.path())
                .args(["--no-index", "-l", "needle", "bin.txt"]),
        );
    assert_eq!(listed, "bin.txt\n");

    let counted =
        stdout_of(
            tgrep()
                .current_dir(dir.path())
                .args(["--no-index", "-c", "needle", "bin.txt"]),
        );
    assert_eq!(counted, "1\n");
}

#[test]
fn binary_flag_promotes_traversal_files_to_explicit() {
    let dir = binary_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "--binary",
        "needle",
        ".",
    ]));
    assert!(
        out.contains(&format!(
            ".{}bin.txt: binary file matches (found \"\\0\" byte around offset 7)",
            sep()
        )),
        "--binary must surface traversal binaries with a note: {out:?}"
    );
    assert!(
        out.contains("plain.txt:needle plain"),
        "lost a text hit: {out:?}"
    );
}

#[test]
fn text_flag_disables_binary_detection() {
    let dir = binary_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "-a",
        "needle",
        ".",
    ]));
    assert!(
        out.contains(&format!(".{}bin.txt:needle", sep())),
        "-a must print binary matches as text: {out:?}"
    );
    assert!(
        !out.contains("binary file matches"),
        "-a must not emit a note: {out:?}"
    );
}

#[test]
fn traversal_hides_binary_files_from_files_without_match() {
    let dir = binary_fixture();
    // bin.txt has no match for this pattern either way; the point is that a
    // skipped binary file must not be reported as "no match".
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--files-without-match",
        "nosuchpattern",
        ".",
    ]));
    assert!(
        !out.contains("bin.txt"),
        "-L must not report skipped binary files: {out:?}"
    );
    assert!(out.contains("plain.txt"), "-L lost a text file: {out:?}");
}

#[test]
fn files_lists_binary_extensions_too() {
    let dir = fixture();
    fs::write(
        dir.path().join("blob.bin"),
        b"needle\x00\x01\x02".as_slice(),
    )
    .unwrap();

    let out = stdout_of(
        tgrep()
            .current_dir(dir.path())
            .args(["--no-index", "--files", "."]),
    );
    assert!(
        out.contains("blob.bin"),
        "--files must not sniff content or reject by extension: {out:?}"
    );
}

#[test]
fn binary_flag_includes_extension_rejected_files() {
    let dir = fixture();
    // Text content behind a binary extension. tgrep skips these by extension to
    // keep indexing cheap; --binary is the escape hatch back to rg's coverage.
    fs::write(dir.path().join("blob.dat"), "needle plain\n").unwrap();

    let without = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "needle",
        ".",
    ]));
    assert!(
        !without.contains("blob.dat"),
        "binary extensions are skipped by default: {without:?}"
    );

    let with = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "--binary",
        "needle",
        ".",
    ]));
    assert!(
        with.contains("blob.dat"),
        "--binary must include extension-rejected files: {with:?}"
    );
}

#[test]
fn indexed_and_brute_force_agree_on_binary_visibility() {
    let dir = binary_fixture();
    let idx = dir.path().join("idx").to_str().unwrap().to_string();
    tgrep()
        .args(["index", dir.path().to_str().unwrap(), "--index-path", &idx])
        .assert()
        .success();

    let run = |mode: &[&str], target: &str| -> String {
        let mut args: Vec<&str> = mode.to_vec();
        args.extend_from_slice(&["--no-heading", "needle", target]);
        stdout_of(tgrep().current_dir(dir.path()).args(&args))
    };

    assert_eq!(
        run(&["--no-index"], "."),
        run(&["--index-path", &idx], "."),
        "traversal binary suppression must match across search paths"
    );
    assert_eq!(
        run(&["--no-index"], "bin.txt"),
        run(&["--index-path", &idx], "bin.txt"),
        "explicit binary notes must match across search paths"
    );
}

// ---------------------------------------------------------------------------
// 9. -M/--max-columns wording, and context lines must not be dropped
// ---------------------------------------------------------------------------

#[test]
fn max_columns_distinguishes_matching_and_context_lines() {
    let dir = TempDir::new().unwrap();
    let long = "x".repeat(200);
    fs::write(
        dir.path().join("long.txt"),
        format!("needle {long}\ncontext {long}\n"),
    )
    .unwrap();

    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "-M",
        "20",
        "-A",
        "1",
        "needle",
        "long.txt",
    ]));
    assert!(
        out.contains("[Omitted long matching line]"),
        "wrong matching-line message: {out:?}"
    );
    assert!(
        out.contains("[Omitted long context line]"),
        "over-long context lines must be reported, not dropped: {out:?}"
    );
}

// ---------------------------------------------------------------------------
// 10. --column / -b / --vimgrep report *source* byte offsets
//
// tgrep decodes invalid UTF-8 lossily, where each bad byte becomes a 3-byte
// U+FFFD. Offsets must be mapped back to the original bytes.
//   printf 'caf\xe9 needle\n' -> rg --column reports column 6, not 8.
// ---------------------------------------------------------------------------

fn invalid_utf8_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("bad.txt"), b"caf\xe9 needle\n".as_slice()).unwrap();
    dir
}

#[test]
fn column_maps_back_to_source_bytes() {
    let dir = invalid_utf8_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "--column",
        "needle",
        "bad.txt",
    ]));
    assert!(
        out.starts_with("1:6:"),
        "expected line 1 column 6 (source bytes), got: {out:?}"
    );
}

#[test]
fn byte_offset_maps_back_to_source_bytes() {
    let dir = invalid_utf8_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "-b",
        "-o",
        "needle",
        "bad.txt",
    ]));
    assert!(
        out.starts_with("5:"),
        "expected source byte offset 5, got: {out:?}"
    );
}

#[test]
fn vimgrep_maps_back_to_source_bytes() {
    let dir = invalid_utf8_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--vimgrep",
        "needle",
        "bad.txt",
    ]));
    assert!(
        out.contains("bad.txt:1:6:"),
        "expected vimgrep column 6, got: {out:?}"
    );
}

#[test]
fn valid_utf8_columns_are_unaffected() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("ok.txt"), "café needle\n").unwrap();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-heading",
        "--column",
        "needle",
        "ok.txt",
    ]));
    // `café ` is 6 bytes (é is 2), so the match starts at column 7.
    assert!(
        out.starts_with("1:7:"),
        "valid UTF-8 must report real byte columns, got: {out:?}"
    );
}

#[test]
fn vimgrep_reports_every_match_on_a_line() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("multi.txt"), "needle and needle\n").unwrap();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--vimgrep",
        "needle",
        "multi.txt",
    ]));
    assert!(
        out.contains("multi.txt:1:1:"),
        "missing first match: {out:?}"
    );
    assert!(
        out.contains("multi.txt:1:12:"),
        "missing second match: {out:?}"
    );
}

// ---------------------------------------------------------------------------
// 11. --index-path pointing outside the search root falls back to brute force
// ---------------------------------------------------------------------------

#[test]
fn search_root_outside_the_index_falls_back_to_brute_force() {
    let (indexed, idx) = indexed_fixture();
    let other = fixture();
    // `other` was never indexed by `idx`; the search must still work.
    let out = stdout_of(tgrep().args([
        "--no-heading",
        "--index-path",
        &idx,
        "needle",
        other.path().to_str().unwrap(),
    ]));
    assert!(
        out.contains("alpha.txt"),
        "an unrelated search root must fall back to brute force: {out:?}"
    );
    assert!(
        !out.contains(indexed.path().to_str().unwrap()),
        "results must not come from the unrelated index: {out:?}"
    );
}

// ---------------------------------------------------------------------------
// 12. `--replace` coordinates
//
// Verified against rg 15.2.0 on `aa X bb X cc` with `-r YYYY`:
//   rg --vimgrep -r YYYY X  ->  1:4 and 1:12
//   rg -b -o     -r YYYY X  ->  3 and 11
// Both are offsets into the *replaced* line, so the second match shifts by the
// length delta of the first. They are therefore not offsets into the file and
// must never be routed through the lossy-UTF-8 fixup table.
// ---------------------------------------------------------------------------

fn replace_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("two.txt"), "aa X bb X cc\n").unwrap();
    // 0xFF is not valid UTF-8; lossy decoding turns it into a 3-byte U+FFFD.
    fs::write(dir.path().join("bad.txt"), [0xFF, b'a', b'\n']).unwrap();
    dir
}

#[test]
fn replace_vimgrep_columns_index_the_replaced_line() {
    let dir = replace_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--vimgrep",
        "-r",
        "YYYY",
        "X",
        "two.txt",
    ]));
    assert_eq!(
        out, "two.txt:1:4:aa YYYY bb YYYY cc\ntwo.txt:1:12:aa YYYY bb YYYY cc\n",
        "columns must be 4 and 12, as ripgrep reports"
    );
}

#[test]
fn replace_only_matching_byte_offsets_accumulate_the_shift() {
    let dir = replace_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "-b",
        "-o",
        "-r",
        "YYYY",
        "X",
        "two.txt",
    ]));
    assert_eq!(
        out, "3:YYYY\n11:YYYY\n",
        "the second replacement starts at 11, not at the original 8"
    );
}

#[test]
fn replace_on_invalid_utf8_does_not_panic() {
    // Regression: replaced-text columns were mapped through `LossyFixups`,
    // which underflowed when an offset landed inside a U+FFFD.
    let dir = replace_fixture();
    for args in [
        vec!["--no-index", "-r", "Z", ".", "bad.txt"],
        vec!["--no-index", "--vimgrep", "-r", "Z", ".", "bad.txt"],
        vec!["--no-index", "-b", "-o", "-r", "Z", ".", "bad.txt"],
        vec!["--no-index", "--column", "-r", "Z", ".", "bad.txt"],
    ] {
        tgrep()
            .current_dir(dir.path())
            .args(&args)
            .assert()
            .success();
    }
}

#[test]
fn replace_columns_on_invalid_utf8_are_source_columns() {
    // `caf\xe9 needle` in latin-1. `needle` starts at source byte 5, so rg
    // 15.2.0 reports column 6 -- not 8, which is where it sits once the
    // undecodable 0xE9 has been repaired into a 3-byte U+FFFD.
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("latin1.txt"),
        [
            0x63, 0x61, 0x66, 0xE9, 0x20, 0x6E, 0x65, 0x65, 0x64, 0x6C, 0x65, 0x0A,
        ],
    )
    .unwrap();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--vimgrep",
        "-r",
        "Z",
        "needle",
        "latin1.txt",
    ]));
    assert!(
        out.starts_with("latin1.txt:1:6:"),
        "expected the source column 6, got: {out:?}"
    );
}

#[test]
fn replace_columns_on_invalid_utf8_stay_positive() {
    let dir = replace_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--vimgrep",
        "-r",
        "Z",
        ".",
        "bad.txt",
    ]));
    // Here the pattern matches the U+FFFD that repairing 0xFF produced, which
    // ripgrep never does -- it searches raw bytes. Parity is out of reach, but
    // the column must still be a valid 1-based one rather than wrapping to 0.
    for line in out.lines() {
        let col: usize = line.split(':').nth(2).unwrap().parse().unwrap();
        assert!(col >= 1, "column must be 1-based, got {col} in {out:?}");
    }
}

// ---------------------------------------------------------------------------
// 13. `-M/--max-columns` counts the line terminator
//
// Verified against rg 15.2.0 with `-M 10` on lines of 8/9/10/11 bytes:
//   LF file   -> 8 and 9 print; 10 and 11 are omitted
//   CRLF file -> only 8 prints
//   final line with no terminator -> 10 bytes still prints
// So the test is `text.len() + terminator.len() > limit`. Under `-o` ripgrep
// measures the matched text alone, with no terminator.
// ---------------------------------------------------------------------------

fn max_columns_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("lf.txt"),
        "nbcdefgh\nnbcdefghi\nnbcdefghij\nnbcdefghijk\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("crlf.txt"),
        "nbcdefgh\r\nnbcdefghi\r\nnbcdefghij\r\n",
    )
    .unwrap();
    fs::write(dir.path().join("notrail.txt"), "nbcdefgh\nnbcdefghij").unwrap();
    fs::write(dir.path().join("om.txt"), "xx nnnnnnnnnn yy\n").unwrap();
    dir
}

#[test]
fn max_columns_counts_the_newline() {
    let dir = max_columns_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "-M",
        "10",
        "-n",
        "n",
        "lf.txt",
    ]));
    assert_eq!(
        out,
        "1:nbcdefgh\n2:nbcdefghi\n3:[Omitted long matching line]\n4:[Omitted long matching line]\n",
        "a 10-byte line plus its newline exceeds a limit of 10"
    );
}

#[test]
fn max_columns_counts_a_crlf_as_two_bytes() {
    let dir = max_columns_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "-M",
        "10",
        "-n",
        "n",
        "crlf.txt",
    ]));
    assert_eq!(
        out, "1:nbcdefgh\n2:[Omitted long matching line]\n3:[Omitted long matching line]\n",
        "9 bytes of text plus CRLF exceeds a limit of 10"
    );
}

#[test]
fn max_columns_ignores_a_missing_final_terminator() {
    let dir = max_columns_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "-M",
        "10",
        "-n",
        "n",
        "notrail.txt",
    ]));
    assert_eq!(
        out, "1:nbcdefgh\n2:nbcdefghij\n",
        "an unterminated final line of exactly the limit still prints"
    );
}

#[test]
fn max_columns_under_only_matching_measures_the_match_alone() {
    let dir = max_columns_fixture();
    let printed = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "-o",
        "-M",
        "10",
        "-n",
        "n+",
        "om.txt",
    ]));
    assert_eq!(
        printed, "1:nnnnnnnnnn\n",
        "a 10-byte match fits a limit of 10"
    );
    let omitted = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "-o",
        "-M",
        "9",
        "-n",
        "n+",
        "om.txt",
    ]));
    assert_eq!(
        omitted, "1:[Omitted long matching line]\n",
        "the same match exceeds a limit of 9, with no terminator counted"
    );
}

// ---------------------------------------------------------------------------
// 14. Unparseable ignore files
//
// Verified against rg 15.2.0 using an ignore file containing `a[z-a]`:
//   rg --ignore-file bad.ignore needle .
//     -> rg: bad.ignore: line 1: error parsing glob 'a[z-a]': invalid range
//     -> exit 0 (a match was still found)
// Both `--no-ignore-messages` and `--no-messages` suppress it: ripgrep's
// `ignore_message!` requires messages AND ignore-messages to be enabled.
// Crucially it does *not* set the "errored" flag the way `err_message!` does,
// so a malformed ignore file never turns a successful search into exit 2.
// The same message is emitted for ignore files discovered during the walk.
// ---------------------------------------------------------------------------

fn stderr_of(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// A fixture whose ignore files contain a glob the parser rejects.
fn bad_ignore_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "needle here\n").unwrap();
    fs::write(dir.path().join("bad.ignore"), "a[z-a]\n").unwrap();
    dir
}

#[test]
fn unparseable_ignore_file_is_reported() {
    let dir = bad_ignore_fixture();
    let err = stderr_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--ignore-file",
        "bad.ignore",
        "needle",
        ".",
    ]));
    assert!(
        err.contains("bad.ignore: line 1: error parsing glob"),
        "expected a parse error naming the ignore file, got: {err:?}"
    );
    // The path must appear exactly once: the error already renders as
    // `<path>: line N: ...`, so prefixing it again would repeat it.
    assert_eq!(
        err.matches("bad.ignore").count(),
        1,
        "the ignore file path should not be printed twice: {err:?}"
    );
}

#[test]
fn no_ignore_messages_suppresses_ignore_file_errors() {
    let dir = bad_ignore_fixture();
    let err = stderr_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-ignore-messages",
        "--ignore-file",
        "bad.ignore",
        "needle",
        ".",
    ]));
    assert!(
        !err.contains("error parsing glob"),
        "--no-ignore-messages must suppress the parse error, got: {err:?}"
    );
}

#[test]
fn no_messages_also_suppresses_ignore_file_errors() {
    let dir = bad_ignore_fixture();
    let err = stderr_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--no-messages",
        "--ignore-file",
        "bad.ignore",
        "needle",
        ".",
    ]));
    assert!(
        !err.contains("error parsing glob"),
        "--no-messages suppresses ignore messages too, got: {err:?}"
    );
}

#[test]
fn unparseable_ignore_file_does_not_change_the_exit_code() {
    let dir = bad_ignore_fixture();
    let out = tgrep()
        .current_dir(dir.path())
        .args(["--no-index", "--ignore-file", "bad.ignore", "needle", "."])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "ripgrep reports the error but still exits 0 when a match was found"
    );
}

#[test]
fn unparseable_gitignore_found_during_the_walk_is_reported() {
    let dir = bad_ignore_fixture();
    fs::create_dir_all(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub").join("b.txt"), "needle deep\n").unwrap();
    fs::write(dir.path().join("sub").join(".gitignore"), "a[z-a]\n").unwrap();
    let err = stderr_of(
        tgrep()
            .current_dir(dir.path())
            .args(["--no-index", "needle", "."]),
    );
    assert!(
        err.contains("error parsing glob"),
        "a malformed .gitignore met during the walk must be reported, got: {err:?}"
    );
    // Reported relative to the search root, not as the extended-length path
    // that `canonicalize` hands the walker on Windows.
    assert!(
        err.contains(&format!("sub{}.gitignore", sep())) && !err.contains(r"\\?\"),
        "the path should be root-relative and free of a verbatim prefix, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 15. `--regex-size-limit` / `--dfa-size-limit`
//
// ripgrep stores both as `usize` and rejects a value that does not fit
// ("size is too big") rather than truncating it, so the search either uses the
// limit that was asked for or fails outright. A malformed value exits 2:
//   rg --regex-size-limit abc  -> rg: error parsing flag --regex-size-limit: ...
// ---------------------------------------------------------------------------

#[test]
fn regex_size_limit_error_names_its_own_flag() {
    let dir = fixture();
    let out = tgrep()
        .current_dir(dir.path())
        .args(["--no-index", "--regex-size-limit", "abc", "needle", "."])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(2), "a bad flag value exits 2");
    assert!(
        err.contains("--regex-size-limit"),
        "the error must name the flag that was actually wrong, got: {err:?}"
    );
}

#[test]
fn dfa_size_limit_error_names_its_own_flag() {
    let dir = fixture();
    let out = tgrep()
        .current_dir(dir.path())
        .args(["--no-index", "--dfa-size-limit", "abc", "needle", "."])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(2), "a bad flag value exits 2");
    assert!(
        err.contains("--dfa-size-limit"),
        "the error must name the flag that was actually wrong, got: {err:?}"
    );
}

#[test]
fn oversized_size_limit_is_rejected_rather_than_truncated() {
    let dir = fixture();
    let out = tgrep()
        .current_dir(dir.path())
        .args([
            "--no-index",
            "--regex-size-limit",
            "99999999999999999G",
            "needle",
            ".",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "a limit too large to represent must fail, not wrap around"
    );
}

#[test]
fn a_large_but_representable_size_limit_is_accepted() {
    let dir = fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--regex-size-limit",
        "1G",
        "--dfa-size-limit",
        "1G",
        "needle",
        "alpha.txt",
    ]));
    assert_eq!(out, "needle at top\n", "a 1G limit is well within usize");
}

// ---------------------------------------------------------------------------
// 16. Binary files in `--json` output
//
// The human-readable printer collapses a matching binary file into a
// "binary file matches" note, but ripgrep's JSON printer has no such note: it
// emits the matches as ordinary `match` events and records the offset of the
// first NUL byte on the file's `end` message.
//
//   $ rg --json needle bin.dat        # "needle here\n\0\x01\ntail\n"
//   {"type":"begin",...}
//   {"type":"match",...,"line_number":1,...}
//   {"type":"end","data":{...,"binary_offset":12,
//                         "stats":{...,"bytes_searched":12,"matches":1}}}
//
// ripgrep also stops counting bytes at that offset rather than at the end of
// the file, and still hides binary files that were only reached by traversal.
// ---------------------------------------------------------------------------

/// A directory holding one binary file (NUL after the matching line) and one
/// ordinary text file, both matching `needle`.
fn binary_json_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let mut bin = b"needle here\n".to_vec();
    bin.extend_from_slice(&[0x00, 0x01]);
    bin.extend_from_slice(b"\ntail\n");
    fs::write(dir.path().join("bin.dat"), bin).unwrap();
    fs::write(dir.path().join("plain.txt"), "needle in text\n").unwrap();
    dir
}

fn json_events(out: &str) -> Vec<serde_json::Value> {
    out.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

#[test]
fn json_emits_match_events_for_an_explicit_binary_file() {
    let dir = binary_json_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--json",
        "needle",
        "bin.dat",
    ]));
    let events = json_events(&out);
    let matches: Vec<_> = events.iter().filter(|e| e["type"] == "match").collect();
    assert_eq!(
        matches.len(),
        1,
        "ripgrep reports binary matches in JSON rather than swallowing them: {out}"
    );
    assert_eq!(matches[0]["data"]["lines"]["text"], "needle here\n");
    assert_eq!(matches[0]["data"]["line_number"], 1);
}

#[test]
fn json_end_message_carries_the_binary_offset() {
    let dir = binary_json_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--json",
        "needle",
        "bin.dat",
    ]));
    let events = json_events(&out);
    let end = events
        .iter()
        .find(|e| e["type"] == "end")
        .unwrap_or_else(|| panic!("expected an end message: {out}"));
    // "needle here\n" is 12 bytes, so the first NUL sits at offset 12.
    assert_eq!(
        end["data"]["binary_offset"], 12,
        "binary_offset is what tells a JSON consumer the hit was binary"
    );
    assert_eq!(
        end["data"]["stats"]["matches"], 1,
        "the match must be counted, not dropped"
    );
    assert_eq!(
        end["data"]["stats"]["searches_with_match"], 1,
        "a binary hit is still a file with a match"
    );
}

#[test]
fn json_binary_offset_is_null_for_a_text_file() {
    let dir = binary_json_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--json",
        "needle",
        "plain.txt",
    ]));
    let events = json_events(&out);
    let end = events.iter().find(|e| e["type"] == "end").unwrap();
    assert!(
        end["data"]["binary_offset"].is_null(),
        "a text file must not claim a binary offset: {out}"
    );
}

#[test]
fn json_binary_offset_is_null_under_dash_a() {
    let dir = binary_json_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--json",
        "-a",
        "needle",
        "bin.dat",
    ]));
    let events = json_events(&out);
    let end = events.iter().find(|e| e["type"] == "end").unwrap();
    assert!(
        end["data"]["binary_offset"].is_null(),
        "-a searches the file as text, so there is no binary offset: {out}"
    );
    assert_eq!(
        end["data"]["stats"]["bytes_searched"], 20,
        "as text, the whole file is searched"
    );
}

#[test]
fn json_bytes_searched_stops_at_the_binary_offset() {
    let dir = binary_json_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--json",
        "needle",
        "bin.dat",
    ]));
    let events = json_events(&out);
    let end = events.iter().find(|e| e["type"] == "end").unwrap();
    assert_eq!(
        end["data"]["stats"]["bytes_searched"], 12,
        "ripgrep quits at the NUL byte instead of reading the rest: {out}"
    );
}

#[test]
fn json_bytes_searched_stops_at_the_binary_offset_without_a_match() {
    let dir = TempDir::new().unwrap();
    let mut bin = b"nothing\n".to_vec();
    bin.extend_from_slice(&[0x00]);
    bin.extend_from_slice(b"here\n");
    fs::write(dir.path().join("nomatch.dat"), bin).unwrap();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "--json",
        "needle",
        "nomatch.dat",
    ]));
    let summary = json_events(&out)
        .into_iter()
        .find(|e| e["type"] == "summary")
        .unwrap_or_else(|| panic!("expected a summary: {out}"));
    assert_eq!(
        summary["data"]["stats"]["bytes_searched"], 8,
        "the byte count stops at the NUL even when nothing matched: {out}"
    );
}

#[test]
fn json_hides_a_binary_file_reached_by_traversal() {
    let dir = binary_json_fixture();
    let out =
        stdout_of(
            tgrep()
                .current_dir(dir.path())
                .args(["--no-index", "--json", "needle", "."]),
        );
    assert!(
        !out.contains("bin.dat"),
        "ripgrep only surfaces a binary file that was named explicitly: {out}"
    );
    assert!(
        out.contains("plain.txt"),
        "the text file must still be reported: {out}"
    );
}

#[test]
fn text_output_still_collapses_a_binary_file_to_a_note() {
    let dir = binary_json_fixture();
    let out = stdout_of(
        tgrep()
            .current_dir(dir.path())
            .args(["--no-index", "needle", "bin.dat"]),
    );
    assert_eq!(
        out, "binary file matches (found \"\\0\" byte around offset 12)\n",
        "only JSON reports the lines; the human printer keeps the note"
    );
}

#[test]
fn count_of_a_binary_file_is_not_double_counted() {
    let dir = binary_json_fixture();
    let out =
        stdout_of(
            tgrep()
                .current_dir(dir.path())
                .args(["--no-index", "-c", "needle", "bin.dat"]),
        );
    assert_eq!(out, "1\n", "one matching line, counted once");
}

// ---------------------------------------------------------------------------
// 17. `-u/--unrestricted` levels
//
// ripgrep's manual defines the three levels as:
//
//   -u    == --no-ignore
//   -uu   == --no-ignore --hidden
//   -uuu  == --no-ignore --hidden --binary
//
// The third level is `--binary`, *not* `-a/--text`: a binary file becomes
// visible and is summarised with a note, rather than having its lines printed.
//
//   $ rg -uuu needle .
//   ./bin.dat: binary file matches (found "\0" byte around offset 11)
// ---------------------------------------------------------------------------

/// Four files, each reachable only at a different `-u` level.
fn unrestricted_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(dir.path().join("plain.txt"), "needle plain\n").unwrap();
    fs::write(dir.path().join("ignored.txt"), "needle ignored\n").unwrap();
    fs::write(dir.path().join(".hidden.txt"), "needle hidden\n").unwrap();
    let mut bin = b"needle bin\n".to_vec();
    bin.extend_from_slice(&[0x00, 0x01]);
    fs::write(dir.path().join("bin.dat"), bin).unwrap();
    dir
}

fn sorted_lines(out: &str) -> Vec<String> {
    let mut v: Vec<String> = out.lines().map(|l| l.to_string()).collect();
    v.sort();
    v
}

#[test]
fn single_u_lifts_only_gitignore() {
    let dir = unrestricted_fixture();
    let out = stdout_of(
        tgrep()
            .current_dir(dir.path())
            .args(["--no-index", "-u", "needle", "."]),
    );
    assert_eq!(
        sorted_lines(&out),
        vec![
            format!(".{}ignored.txt:needle ignored", sep()),
            format!(".{}plain.txt:needle plain", sep()),
        ],
        "-u is --no-ignore and nothing more"
    );
}

#[test]
fn double_u_adds_hidden_files() {
    let dir = unrestricted_fixture();
    let out = stdout_of(
        tgrep()
            .current_dir(dir.path())
            .args(["--no-index", "-uu", "needle", "."]),
    );
    assert_eq!(
        sorted_lines(&out),
        vec![
            format!(".{}.hidden.txt:needle hidden", sep()),
            format!(".{}ignored.txt:needle ignored", sep()),
            format!(".{}plain.txt:needle plain", sep()),
        ],
        "-uu adds --hidden"
    );
}

#[test]
fn triple_u_surfaces_binary_files_as_a_note() {
    let dir = unrestricted_fixture();
    let out =
        stdout_of(
            tgrep()
                .current_dir(dir.path())
                .args(["--no-index", "-uuu", "needle", "."]),
        );
    let bin_line = format!(
        ".{}bin.dat: binary file matches (found \"\\0\" byte around offset 11)",
        sep()
    );
    assert!(
        out.lines().any(|l| l == bin_line),
        "-uuu is --binary, so the file is summarised with a note: {out}"
    );
    assert!(
        !out.contains("bin.dat:needle bin"),
        "-uuu must not print binary lines as text; that is -a: {out}"
    );
}

#[test]
fn triple_u_still_needs_dash_a_to_print_binary_lines() {
    let dir = unrestricted_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "-uuu",
        "-a",
        "needle",
        "bin.dat",
    ]));
    assert_eq!(
        out, "needle bin\n",
        "-a is what turns a binary file into text, even alongside -uuu"
    );
}

// ---------------------------------------------------------------------------
// 18. `--max-count` counts matching *lines*, not matches
//
// Verified against rg 15.2.0. `-m` limits matching lines, so a line holding
// several matches spends a single unit of the budget and every match on it is
// still reported:
//
//   $ printf 'foo foo foo\nfoo bar\nfoo baz\n' > m.txt
//   $ rg -m1 --vimgrep foo m.txt
//   m.txt:1:1:foo foo foo
//   m.txt:1:5:foo foo foo
//   m.txt:1:9:foo foo foo
//
// Under `-U` the unit is the contiguous *block* of lines matches cover, which
// keeps a match that straddles a line boundary whole:
//
//   $ printf 'a foo\nbar foo\nbaz foo\nqux foo\n' > ml.txt
//   $ rg -U -m1 '(?s)foo.*?foo' ml.txt
//   a foo
//   bar foo
//
// Known divergence: rg stops reading a file once the limit is reached, so
// `--stats` reports a smaller `bytes_searched` than tgrep, which searches from
// a whole-file buffer. Match counts themselves agree.
// ---------------------------------------------------------------------------

/// Three matches on line 1, then one match on each of lines 2 and 3.
fn max_count_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("m.txt"), "foo foo foo\nfoo bar\nfoo baz\n").unwrap();
    fs::write(
        dir.path().join("ml.txt"),
        "a foo\nbar foo\nbaz foo\nqux foo\n",
    )
    .unwrap();
    dir
}

#[test]
fn max_count_one_keeps_every_match_on_the_matching_line() {
    let dir = max_count_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "-m",
        "1",
        "--vimgrep",
        "foo",
        "m.txt",
    ]));
    assert_eq!(
        out, "m.txt:1:1:foo foo foo\nm.txt:1:5:foo foo foo\nm.txt:1:9:foo foo foo\n",
        "-m limits matching lines, so all three matches on line 1 are reported"
    );
}

#[test]
fn multiline_max_count_one_keeps_every_match_on_the_matching_line() {
    let dir = max_count_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "-U",
        "-m",
        "1",
        "--vimgrep",
        "foo",
        "m.txt",
    ]));
    assert_eq!(
        out, "m.txt:1:1:foo foo foo\nm.txt:1:5:foo foo foo\nm.txt:1:9:foo foo foo\n",
        "-U must count lines too; limiting match spans would print only the first"
    );
}

#[test]
fn multiline_max_count_keeps_a_match_that_spans_lines_whole() {
    let dir = max_count_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "-U",
        "-m",
        "1",
        "(?s)foo.*?foo",
        "ml.txt",
    ]));
    assert_eq!(
        out, "a foo\nbar foo\n",
        "the single match covers both lines, so both print rather than a partial match"
    );
}

#[test]
fn multiline_max_count_two_takes_two_line_blocks() {
    let dir = max_count_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "-U",
        "-m",
        "2",
        "(?s)foo.*?foo",
        "ml.txt",
    ]));
    assert_eq!(
        out, "a foo\nbar foo\nbaz foo\nqux foo\n",
        "two matches of two lines each are two units of the budget"
    );
}

#[test]
fn multiline_max_count_counts_separate_lines_separately() {
    let dir = max_count_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "-U",
        "-m",
        "2",
        "foo",
        "ml.txt",
    ]));
    assert_eq!(
        out, "a foo\nbar foo\n",
        "one match per line means each line spends its own unit"
    );
}

#[test]
fn multiline_max_count_zero_reports_nothing() {
    let dir = max_count_fixture();
    let out = tgrep()
        .current_dir(dir.path())
        .args(["--no-index", "-U", "-m", "0", "foo", "ml.txt"])
        .output()
        .expect("run tgrep");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(out.status.code(), Some(1), "-m 0 finds nothing, so exit 1");
}

#[test]
fn multiline_max_count_counts_lines_for_dash_c() {
    let dir = max_count_fixture();
    let out = stdout_of(tgrep().current_dir(dir.path()).args([
        "--no-index",
        "-U",
        "-m",
        "1",
        "-c",
        "foo",
        "m.txt",
    ]));
    assert_eq!(
        out, "1\n",
        "-c counts the one matching line, not its 3 matches"
    );
}
