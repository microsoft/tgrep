//! Hidden-file coverage must not change ignore rules or ordinary query visibility.
//! Indexed assertions also check the route, so a full scan cannot hide an
//! incomplete index or a watcher that never delivered an update. Positive glob
//! overrides are compared by results because they can widen the indexed corpus.

use assert_cmd::Command;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::{NamedTempFile, TempDir};
use tgrep_core::meta::IndexMeta;

const NEEDLE: &str = "needle";
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const INNER_IGNORE: &str = "private.txt\n.private.txt\n";
const MATCHING_FILES: &[(&str, &str)] = &[
    (
        "visible.txt",
        "needle visible first\nneedle visible second\n",
    ),
    ("nested/visible.txt", "needle nested visible\n"),
    (".secret.txt", "needle secret first\nneedle secret second\n"),
    (".github/settings.txt", "needle github settings\n"),
    (".github/.nested/deep.txt", "needle github nested\n"),
    ("nested/.hidden/deep.txt", "needle nested hidden\n"),
];

fn path_under(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = path_under(root, relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, contents).unwrap();
}

#[cfg(windows)]
fn set_hidden_attribute(root: &Path, relative: &str, hidden: bool) {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};

    let path = path_under(root, relative);
    let attributes = fs::metadata(&path).unwrap().file_attributes();
    let attributes = if hidden {
        attributes | FILE_ATTRIBUTE_HIDDEN
    } else {
        attributes & !FILE_ATTRIBUTE_HIDDEN
    };
    let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // The NUL-terminated path remains allocated throughout the call.
    let result = unsafe { SetFileAttributesW(wide.as_ptr(), attributes) };
    assert_ne!(
        result,
        0,
        "cannot set hidden={hidden} on {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

struct Fixture {
    temp: TempDir,
    root: PathBuf,
    index: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let target = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target");
        fs::create_dir_all(&target).unwrap();
        let temp = tempfile::Builder::new()
            .prefix("indexed-hidden-")
            .tempdir_in(target)
            .unwrap();
        let root = temp.path().join("repo");
        // The ignore crate recognizes this repository boundary without git init.
        fs::create_dir_all(root.join(".git")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let index = temp.path().join("index");
        let fixture = Self { temp, root, index };

        for &(path, contents) in MATCHING_FILES {
            fixture.write(path, contents);
        }
        fixture.write("notes.txt", "no matching token here\n");
        fixture.write(".notes.txt", "no matching token here either\n");
        fixture.write(
            ".gitignore",
            "ignored.txt\n.ignored.txt\nignored-tree/\n.ignored-tree/\n",
        );
        fixture.write(".github/.gitignore", INNER_IGNORE);
        for path in [
            "ignored.txt",
            ".ignored.txt",
            "ignored-tree/visible.txt",
            "ignored-tree/.secret.txt",
            ".ignored-tree/visible.txt",
            ".github/private.txt",
            ".github/.private.txt",
            ".git/private.txt",
        ] {
            fixture.write(path, "needle ignored content must not leak\n");
        }
        fixture
    }

    fn write(&self, relative: &str, contents: &str) {
        write_file(&self.root, relative, contents);
    }

    fn remove(&self, relative: &str) {
        fs::remove_file(path_under(&self.root, relative)).unwrap();
    }

    fn rename(&self, old: &str, new: &str) {
        fs::rename(path_under(&self.root, old), path_under(&self.root, new)).unwrap();
    }

    fn build(&self, force: bool) {
        let mut command = Command::cargo_bin("tgrep").unwrap();
        command
            .current_dir(&self.root)
            .timeout(WAIT_TIMEOUT)
            .arg("index")
            .arg(&self.root)
            .arg("--index-path")
            .arg(&self.index);
        if force {
            command.arg("--force");
        }
        // Deliberately never pass index --hidden.
        command.assert().success();
        self.assert_complete_coverage();
    }

    fn assert_complete_coverage(&self) {
        let meta = IndexMeta::load(&self.index).unwrap();
        assert!(meta.complete, "index publication is incomplete: {meta:?}");
        assert!(
            meta.hidden_complete,
            "hidden coverage is unproven: {meta:?}"
        );
    }

    fn query(
        &self,
        scope: &str,
        pattern: &str,
        mode: OutputMode,
        hidden: bool,
        no_index: bool,
    ) -> Output {
        let mut command = Command::cargo_bin("tgrep").unwrap();
        command
            .current_dir(&self.root)
            .timeout(WAIT_TIMEOUT)
            .arg("--index-path")
            .arg(&self.index);
        if let Some(flag) = mode.flag() {
            command.arg(flag);
        }
        if hidden {
            command.arg("--hidden");
        }
        // Preserve the Copilot argv shape, including the negative-only glob.
        command.args(["--glob", "!.git", "--with-filename", "--stats"]);
        if no_index {
            command.arg("--no-index");
        }
        command.arg("--");
        if mode != OutputMode::Files {
            command.arg(pattern);
        }
        command
            .arg(path_under(&self.root, scope))
            .output()
            .expect("failed to run tgrep query")
    }

    fn relative_output_path(&self, printed: &str) -> String {
        let printed = printed.replace('\\', "/");
        let root = self.root.to_string_lossy().replace('\\', "/");
        let printed = printed.strip_prefix("//?/").unwrap_or(&printed);
        let root = root.strip_prefix("//?/").unwrap_or(&root);
        printed
            .strip_prefix(&format!("{root}/"))
            .unwrap_or_else(|| panic!("output path {printed:?} is not under {root:?}"))
            .to_owned()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMode {
    Content,
    FilesWithMatches,
    FilesWithMatchesShort,
    Count,
    CountShort,
    Json,
    Files,
}

impl OutputMode {
    fn flag(self) -> Option<&'static str> {
        match self {
            Self::Content => None,
            Self::FilesWithMatches => Some("--files-with-matches"),
            Self::FilesWithMatchesShort => Some("-l"),
            Self::Count => Some("--count"),
            Self::CountShort => Some("-c"),
            Self::Json => Some("--json"),
            Self::Files => Some("--files"),
        }
    }

    fn paths_only(self) -> bool {
        matches!(
            self,
            Self::FilesWithMatches | Self::FilesWithMatchesShort | Self::Files
        )
    }
}

const OUTPUT_MODES: &[OutputMode] = &[
    OutputMode::Content,
    OutputMode::FilesWithMatches,
    OutputMode::FilesWithMatchesShort,
    OutputMode::Count,
    OutputMode::CountShort,
    OutputMode::Json,
    OutputMode::Files,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Local,
    Server,
    BruteForce,
    // Expect a scan without requesting --no-index.
    Fallback,
}

impl Backend {
    fn matches(self, mode: OutputMode, output: &Output) -> bool {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if mode == OutputMode::Files {
            let route = match self {
                Self::Local => "(via local index)",
                Self::Server => "(via server)",
                Self::BruteForce | Self::Fallback => "(via filesystem walk)",
            };
            return stderr.contains("Filename search completed")
                && stderr.contains(route)
                && !stderr.contains("Brute-force search completed");
        }
        match self {
            Self::Local => {
                stderr.contains("Search completed")
                    && stderr.contains("Query plan:")
                    && !stderr.contains("(via server)")
                    && !stderr.contains("Brute-force search completed")
            }
            Self::Server => {
                stderr.contains("(via server)") && !stderr.contains("Brute-force search completed")
            }
            Self::BruteForce | Self::Fallback => {
                stderr.contains("Brute-force search completed")
                    && !stderr.contains("(via server)")
                    && !stderr.contains("Query plan:")
            }
        }
    }
}

#[derive(Clone)]
struct Corpus {
    matches: BTreeMap<String, usize>,
    files: BTreeSet<String>,
    hidden_attributes: BTreeSet<String>,
}

impl Corpus {
    fn initial() -> Self {
        let matches: BTreeMap<_, _> = MATCHING_FILES
            .iter()
            .map(|&(path, contents)| {
                (
                    path.to_owned(),
                    contents
                        .lines()
                        .filter(|line| line.contains(NEEDLE))
                        .count(),
                )
            })
            .collect();
        let mut files: BTreeSet<_> = matches.keys().cloned().collect();
        files.extend(
            [
                "notes.txt",
                ".notes.txt",
                ".gitignore",
                ".github/.gitignore",
            ]
            .map(str::to_owned),
        );
        Self {
            matches,
            files,
            hidden_attributes: BTreeSet::new(),
        }
    }

    fn add(&mut self, path: &str, count: usize) {
        self.matches.insert(path.to_owned(), count);
        self.files.insert(path.to_owned());
    }

    fn remove(&mut self, path: &str) {
        self.matches.remove(path);
        self.files.remove(path);
        self.hidden_attributes.remove(path);
    }

    fn expected(&self, scope: &str, hidden: bool, mode: OutputMode) -> BTreeMap<String, usize> {
        let paths = if mode == OutputMode::Files {
            self.files.iter().map(|path| (path.clone(), 1)).collect()
        } else {
            self.matches.clone()
        };
        let prefix = if scope.is_empty() {
            String::new()
        } else {
            format!("{scope}/")
        };
        let hidden_attributes: Vec<_> = self
            .hidden_attributes
            .iter()
            .filter_map(|entry| entry.strip_prefix(&prefix))
            .collect();
        paths
            .into_iter()
            .filter(|(path, _)| {
                path.strip_prefix(&prefix).is_some_and(|relative| {
                    hidden
                        || (!relative.split('/').any(|part| part.starts_with('.'))
                            && !hidden_attributes.iter().any(|entry| {
                                relative == *entry || relative.starts_with(&format!("{entry}/"))
                            }))
                })
            })
            .map(|(path, count)| (path, if mode.paths_only() { 1 } else { count }))
            .collect()
    }
}

fn parsed_output(fixture: &Fixture, mode: OutputMode, output: &Output) -> BTreeMap<String, usize> {
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let mut paths = BTreeMap::new();
    let mut summaries = 0;
    for line in stdout.lines() {
        let (printed, count) = match mode {
            OutputMode::Json => {
                let record: Value = serde_json::from_str(line).expect("invalid JSON output");
                match record["type"].as_str().expect("missing JSON record type") {
                    "match" => {
                        let text = record["data"]["lines"]["text"]
                            .as_str()
                            .expect("missing JSON match text");
                        assert!(text.contains(NEEDLE), "unexpected match: {record}");
                        let path = record["data"]["path"]["text"]
                            .as_str()
                            .expect("missing JSON match path");
                        let path = fixture.relative_output_path(path);
                        *paths.entry(path).or_insert(0) += 1;
                    }
                    "summary" => summaries += 1,
                    "begin" | "end" => {}
                    kind => panic!("unexpected JSON record type {kind:?}: {record}"),
                }
                continue;
            }
            OutputMode::Content => {
                let (path, text) = line.rsplit_once(':').expect("missing content separator");
                assert!(text.contains(NEEDLE), "unexpected content line: {line}");
                (path, 1)
            }
            OutputMode::Count | OutputMode::CountShort => {
                let (path, count) = line.rsplit_once(':').expect("missing count separator");
                (path, count.parse::<usize>().expect("invalid match count"))
            }
            _ => (line, 1),
        };
        let path = fixture.relative_output_path(printed);
        if mode == OutputMode::Content {
            *paths.entry(path).or_insert(0) += count;
        } else {
            assert!(
                paths.insert(path, count).is_none(),
                "duplicate file in {mode:?} output: {stdout}"
            );
        }
    }
    if mode == OutputMode::Json {
        assert_eq!(summaries, 1, "missing or duplicate JSON summary: {stdout}");
    }
    paths
}

fn expected_exit(mode: OutputMode, expected: &BTreeMap<String, usize>) -> i32 {
    if mode == OutputMode::Files || !expected.is_empty() {
        0
    } else {
        1
    }
}

fn assert_query(
    fixture: &Fixture,
    corpus: &Corpus,
    scope: &str,
    hidden: bool,
    mode: OutputMode,
    backend: Backend,
) {
    let output = fixture.query(scope, NEEDLE, mode, hidden, backend == Backend::BruteForce);
    let expected = corpus.expected(scope, hidden, mode);
    let diagnostics =
        format!("{backend:?}, {mode:?}, scope={scope:?}, hidden={hidden}\n{output:?}");
    assert_eq!(
        output.status.code(),
        Some(expected_exit(mode, &expected)),
        "{diagnostics}"
    );
    assert!(backend.matches(mode, &output), "{diagnostics}");
    assert_eq!(
        parsed_output(fixture, mode, &output),
        expected,
        "{diagnostics}"
    );
}

fn assert_modes(fixture: &Fixture, corpus: &Corpus, scope: &str, backend: Backend) {
    for hidden in [false, true] {
        for &mode in OUTPUT_MODES {
            assert_query(fixture, corpus, scope, hidden, mode, backend);
        }
    }
}

fn assert_snapshot(fixture: &Fixture, corpus: &Corpus, backend: Backend) {
    for hidden in [false, true] {
        for mode in [OutputMode::FilesWithMatches, OutputMode::Files] {
            assert_query(fixture, corpus, "", hidden, mode, backend);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatchMode {
    Disabled,
    Native,
    Poll,
}

impl WatchMode {
    fn active_name(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Native => "native",
            Self::Poll => "poll",
        }
    }
}

struct ServerGuard {
    child: Option<Child>,
    log: NamedTempFile,
    port: Option<u16>,
    mode: WatchMode,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl ServerGuard {
    fn start(fixture: &Fixture, corpus: &Corpus, mode: WatchMode, hidden: bool) -> Self {
        let warm_start = fixture.index.join("meta.json").exists()
            && IndexMeta::load(&fixture.index).unwrap().complete;
        let log = NamedTempFile::new_in(fixture.temp.path()).unwrap();
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("tgrep"));
        command
            .current_dir(&fixture.root)
            .arg("serve")
            .arg("--index-path")
            .arg(&fixture.index);
        match mode {
            WatchMode::Disabled => command.arg("--no-watch"),
            WatchMode::Native => command.args(["--watch-mode", "auto"]),
            WatchMode::Poll => command.args(["--watch-mode", "poll", "--poll-interval", "1"]),
        };
        if hidden {
            command.arg("--hidden");
        }
        let child = command
            .arg(&fixture.root)
            .stdout(Stdio::null())
            .stderr(log.as_file().try_clone().unwrap())
            .spawn()
            .expect("failed to start tgrep serve");
        let pid = u64::from(child.id());
        let mut server = Self {
            child: Some(child),
            log,
            port: None,
            mode,
        };
        let started = Instant::now();
        let serve_json = fixture.index.join("serve.json");
        loop {
            server.assert_running();
            let observation = match fs::read_to_string(&serve_json) {
                Ok(data) => {
                    if let Ok(info) = serde_json::from_str::<Value>(&data)
                        && info["pid"].as_u64() == Some(pid)
                        && let Some(port) = info["port"].as_u64()
                        && let Ok(port) = u16::try_from(port)
                        && TcpStream::connect(("127.0.0.1", port)).is_ok()
                    {
                        server.port = Some(port);
                        break;
                    }
                    data
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => error.to_string(),
                Err(error) => panic!("cannot read {}: {error}", serve_json.display()),
            };
            assert!(
                started.elapsed() < WAIT_TIMEOUT,
                "server discovery timed out: {observation}\n{}",
                server.logs()
            );
            thread::sleep(Duration::from_millis(100));
        }

        loop {
            server.assert_running();
            let status = server.rpc("status");
            let refresh_ready = (!warm_start && mode == WatchMode::Disabled)
                || (status["last_reconcile_at"].is_u64() && status["reconcile_running"] == false);
            if has_complete_coverage(&status)
                && status["watch_mode_active"] == mode.active_name()
                && refresh_ready
            {
                server.assert_mode(&status);
                break;
            }
            assert!(
                started.elapsed() < WAIT_TIMEOUT,
                "server bootstrap timed out: {status}\n{}",
                server.logs()
            );
            thread::sleep(Duration::from_millis(100));
        }
        wait_for_snapshot(fixture, &mut server, corpus);
        server
    }

    fn logs(&self) -> String {
        fs::read_to_string(self.log.path())
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with("[trace] search:"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_running(&mut self) {
        let status = self.child.as_mut().unwrap().try_wait().unwrap();
        assert!(
            status.is_none(),
            "server exited unexpectedly: {status:?}\n{}",
            self.logs()
        );
    }

    fn rpc(&self, method: &str) -> Value {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port.unwrap())).unwrap();
        stream.set_read_timeout(Some(WAIT_TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(WAIT_TIMEOUT)).unwrap();
        let request = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method});
        writeln!(stream, "{request}").unwrap();
        stream.flush().unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        assert!(
            response.get("error").is_none(),
            "{method} RPC failed: {response}\n{}",
            self.logs()
        );
        response.get("result").expect("missing RPC result").clone()
    }

    fn assert_mode(&self, status: &Value) {
        assert_eq!(
            status["watch_mode_active"],
            self.mode.active_name(),
            "{status}"
        );
        assert_eq!(
            status["watcher_active"],
            self.mode == WatchMode::Native,
            "{status}"
        );
        assert!(status["last_reconcile_error"].is_null(), "{status}");
    }

    fn stop(mut self) {
        let child = self.child.as_mut().unwrap();
        child.kill().expect("failed to stop owned server process");
        child.wait().expect("failed to reap owned server process");
        self.child = None;
    }
}

fn has_complete_coverage(status: &Value) -> bool {
    let indexing = status["indexing"]
        .as_bool()
        .expect("missing indexing status");
    let hidden_complete = status["hidden_complete"]
        .as_bool()
        .expect("missing hidden coverage status");
    assert!(
        !indexing || !hidden_complete,
        "partial server advertised complete hidden coverage: {status}"
    );
    !indexing && hidden_complete
}

fn wait_for_snapshot(fixture: &Fixture, server: &mut ServerGuard, corpus: &Corpus) {
    let started = Instant::now();
    loop {
        server.assert_running();
        let mut failures = Vec::new();
        for hidden in [false, true] {
            for mode in [OutputMode::FilesWithMatches, OutputMode::Files] {
                let output = fixture.query("", NEEDLE, mode, hidden, false);
                let actual = parsed_output(fixture, mode, &output);
                let expected = corpus.expected("", hidden, mode);
                if output.status.code() != Some(expected_exit(mode, &expected))
                    || !Backend::Server.matches(mode, &output)
                    || actual != expected
                {
                    failures.push(format!(
                        "{mode:?}, hidden={hidden}: expected {expected:?}, got {actual:?}\n\
                         status={:?}, stderr={}",
                        output.status.code(),
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
            }
        }
        let status = server.rpc("status");
        if !has_complete_coverage(&status) {
            failures.push(format!("server coverage is not ready: {status}"));
        }
        if failures.is_empty() {
            server.assert_mode(&status);
            return;
        }
        assert!(
            started.elapsed() < WAIT_TIMEOUT,
            "server did not reach expected snapshot:\n{}\n{}",
            failures.join("\n"),
            server.logs()
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_pattern(
    fixture: &Fixture,
    server: &mut ServerGuard,
    pattern: &str,
    expected_paths: &[&str],
) {
    let expected: BTreeMap<_, _> = expected_paths
        .iter()
        .map(|path| ((*path).to_owned(), 1))
        .collect();
    let mode = OutputMode::FilesWithMatches;
    let started = Instant::now();
    loop {
        server.assert_running();
        let output = fixture.query("", pattern, mode, true, false);
        if output.status.code() == Some(expected_exit(mode, &expected))
            && Backend::Server.matches(mode, &output)
            && parsed_output(fixture, mode, &output) == expected
        {
            return;
        }
        assert!(
            started.elapsed() < WAIT_TIMEOUT,
            "pattern {pattern:?} did not reach {expected:?} via server:\n{output:?}\n{}",
            server.logs()
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn default_index_serves_hidden_copilot_formats_without_disabling_ignores() {
    let fixture = Fixture::new();
    let corpus = Corpus::initial();
    fixture.build(false);
    assert_modes(&fixture, &corpus, "", Backend::Local);
    assert_modes(&fixture, &corpus, "", Backend::BruteForce);
}

#[test]
fn ready_server_serves_hidden_formats_and_accepts_redundant_hidden_flag() {
    for hidden in [false, true] {
        let fixture = Fixture::new();
        let corpus = Corpus::initial();
        // Start without an index: serve, not a preceding index command, owns
        // the initial hidden-inclusive build in both variants.
        let server = ServerGuard::start(&fixture, &corpus, WatchMode::Disabled, hidden);
        assert_modes(&fixture, &corpus, "", Backend::Server);
        for mode in [OutputMode::Content, OutputMode::Files] {
            assert_query(&fixture, &corpus, "", true, mode, Backend::BruteForce);
        }
        server.stop();
        fixture.assert_complete_coverage();
        assert_snapshot(&fixture, &corpus, Backend::Local);
    }
}

#[test]
fn unproven_indexes_scan_scoped_hidden_roots_until_server_upgrade() {
    let fixture = Fixture::new();
    let mut corpus = Corpus::initial();
    fixture.build(false);
    let meta_path = fixture.index.join("meta.json");
    let original: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();

    let late_paths = [
        ".github/late-visible.txt",
        ".github/.late-hidden.txt",
        ".github/.late-directory/child.txt",
        "late-outside.txt",
        ".late-outside.txt",
    ];
    for path in late_paths {
        fixture.write(path, "needle added_after_the_index\n");
    }
    for mode in [OutputMode::FilesWithMatches, OutputMode::Files] {
        assert_query(&fixture, &corpus, ".github", true, mode, Backend::Local);
    }
    for path in late_paths {
        corpus.add(path, 1);
    }

    for (coverage, complete, hidden_complete) in [
        ("incomplete", false, Some(true)),
        ("false", true, Some(false)),
        ("missing", true, None),
    ] {
        let mut meta = original.clone();
        meta["complete"] = Value::Bool(complete);
        if let Some(hidden_complete) = hidden_complete {
            meta["hidden_complete"] = Value::Bool(hidden_complete);
        } else {
            let fields = meta.as_object_mut().unwrap();
            fields.remove("hidden_complete");
            fields.remove("visibility");
        }
        fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        let loaded = IndexMeta::load(&fixture.index).unwrap();
        assert!(
            !loaded.complete || !loaded.hidden_complete,
            "{coverage}: capability must remain unproven"
        );
        eprintln!("scoped fallback coverage: {coverage}");
        for hidden in [false, true] {
            for mode in [
                OutputMode::Content,
                OutputMode::FilesWithMatches,
                OutputMode::Files,
            ] {
                assert_query(
                    &fixture,
                    &corpus,
                    ".github",
                    hidden,
                    mode,
                    Backend::Fallback,
                );
            }
        }
    }

    // The final variant is complete but lacks both hidden capability and
    // visibility evidence. Startup must reconcile, not merely flip a flag.
    let assert_scope = |backend| {
        for hidden in [false, true] {
            for mode in [
                OutputMode::Content,
                OutputMode::FilesWithMatches,
                OutputMode::Files,
            ] {
                assert_query(&fixture, &corpus, ".github", hidden, mode, backend);
            }
        }
    };
    let server = ServerGuard::start(&fixture, &corpus, WatchMode::Disabled, false);
    assert_scope(Backend::Server);
    server.stop();
    fixture.assert_complete_coverage();
    assert_scope(Backend::Local);
}

#[test]
fn explicitly_named_hidden_root_uses_the_parent_custom_index() {
    let fixture = Fixture::new();
    let corpus = Corpus::initial();
    fixture.build(false);
    for backend in [Backend::Local, Backend::Server] {
        let _server = (backend == Backend::Server)
            .then(|| ServerGuard::start(&fixture, &corpus, WatchMode::Disabled, false));
        // settings.txt is visible relative to .github; .nested/deep.txt and
        // .gitignore still require --hidden. Siblings must never escape scope.
        assert_modes(&fixture, &corpus, ".github", backend);
        for mode in [OutputMode::FilesWithMatches, OutputMode::Files] {
            assert_query(&fixture, &corpus, "nested/.hidden", false, mode, backend);
        }
        assert_snapshot(&fixture, &corpus, backend);
    }
}

#[cfg(windows)]
#[test]
fn windows_hidden_attributes_retain_visibility_and_respect_explicit_roots() {
    let fixture = Fixture::new();
    let mut corpus = Corpus::initial();
    for path in [
        "attribute-secret.txt",
        "attribute-directory/visible.txt",
        "attribute-directory/secret.txt",
        "attribute-directory/.dot.txt",
    ] {
        fixture.write(path, "needle native_hidden_attribute\n");
        corpus.add(path, 1);
    }
    let hidden_entries = [
        "attribute-secret.txt",
        "attribute-directory",
        "attribute-directory/secret.txt",
    ];
    for path in hidden_entries {
        set_hidden_attribute(&fixture.root, path, true);
        corpus.hidden_attributes.insert(path.to_owned());
    }
    fixture.build(false);

    let assert_scopes = |corpus: &Corpus, backend| {
        for scope in ["", "attribute-directory"] {
            for hidden in [false, true] {
                for mode in [OutputMode::FilesWithMatches, OutputMode::Files] {
                    assert_query(&fixture, corpus, scope, hidden, mode, backend);
                }
            }
        }
    };
    assert_scopes(&corpus, Backend::Local);
    assert_scopes(&corpus, Backend::BruteForce);
    let server = ServerGuard::start(&fixture, &corpus, WatchMode::Disabled, false);
    assert_scopes(&corpus, Backend::Server);

    for path in hidden_entries {
        set_hidden_attribute(&fixture.root, path, false);
    }
    let mut live_corpus = corpus.clone();
    live_corpus.hidden_attributes.clear();
    // Attribute changes after a no-watch startup must not cause per-query
    // stat-based visibility changes. Only a fresh scan sees the live attributes.
    assert_scopes(&corpus, Backend::Server);
    assert_scopes(&live_corpus, Backend::BruteForce);
    server.stop();
    assert_scopes(&corpus, Backend::Local);

    fixture.build(true);
    assert_scopes(&live_corpus, Backend::Local);
}

fn exercise_hidden_updates(mode: WatchMode) {
    let fixture = Fixture::new();
    let mut corpus = Corpus::initial();
    fixture.build(false);
    let mut server = ServerGuard::start(&fixture, &corpus, mode, false);

    for (path, contents) in [
        (".created.txt", "needle hidden_create_before\n"),
        (".github/live.txt", "needle github_live\n"),
        (".github/.live.txt", "needle github_dot_live\n"),
        ("nested/.moving/child.txt", "needle moving_visible_child\n"),
        ("nested/.moving/.child.txt", "needle moving_hidden_child\n"),
    ] {
        fixture.write(path, contents);
        corpus.add(path, 1);
    }
    fixture.write(".github/private.txt", "needle ignored_edit_must_not_leak\n");
    fixture.write(
        ".ignored-tree/new.txt",
        "needle ignored_create_must_not_leak\n",
    );
    wait_for_snapshot(&fixture, &mut server, &corpus);
    wait_for_pattern(
        &fixture,
        &mut server,
        "hidden_create_before",
        &[".created.txt"],
    );

    fixture.write(
        ".created.txt",
        "needle hidden_edit_after_with_a_different_size\n",
    );
    fixture.remove(".secret.txt");
    corpus.remove(".secret.txt");
    fixture.rename("nested/.moving", "nested/moved");
    for name in ["child.txt", ".child.txt"] {
        corpus.remove(&format!("nested/.moving/{name}"));
        corpus.add(&format!("nested/moved/{name}"), 1);
    }
    // Filename membership proves deletion and rename, not merely that search
    // skipped a path which no longer exists on disk.
    wait_for_snapshot(&fixture, &mut server, &corpus);
    wait_for_pattern(
        &fixture,
        &mut server,
        "hidden_edit_after_with_a_different_size",
        &[".created.txt"],
    );
    wait_for_pattern(&fixture, &mut server, "hidden_create_before", &[]);

    fixture.write(
        ".github/.gitignore",
        &format!("{INNER_IGNORE}live.txt\n.pending/\n"),
    );
    fixture.write(".github/.ignore", ".live.txt\n");
    corpus.remove(".github/live.txt");
    corpus.remove(".github/.live.txt");
    corpus.files.insert(".github/.ignore".to_owned());
    wait_for_snapshot(&fixture, &mut server, &corpus);

    fixture.write(".github/live.txt", "needle changed_while_gitignored\n");
    fixture.write(".github/.live.txt", "needle changed_while_dot_ignored\n");
    fixture.write(
        ".github/.pending/new.txt",
        "needle hidden_created_while_ignored\n",
    );
    fixture.write(
        ".created.txt",
        "needle ignore_event_positive_control_longer\n",
    );
    wait_for_pattern(
        &fixture,
        &mut server,
        "ignore_event_positive_control_longer",
        &[".created.txt"],
    );
    wait_for_snapshot(&fixture, &mut server, &corpus);

    fixture.write(".github/.gitignore", INNER_IGNORE);
    fixture.remove(".github/.ignore");
    corpus.files.remove(".github/.ignore");
    for path in [
        ".github/live.txt",
        ".github/.live.txt",
        ".github/.pending/new.txt",
    ] {
        corpus.add(path, 1);
    }
    wait_for_snapshot(&fixture, &mut server, &corpus);
    wait_for_pattern(
        &fixture,
        &mut server,
        "hidden_created_while_ignored",
        &[".github/.pending/new.txt"],
    );

    // A synchronous reload establishes a durable pre-stop snapshot. Offline
    // mutations below can only be learned by the restarted server.
    server.rpc("reload");
    wait_for_snapshot(&fixture, &mut server, &corpus);
    server.stop();

    fixture.remove(".created.txt");
    corpus.remove(".created.txt");
    fixture.write(".offline.txt", "needle offline_hidden_create\n");
    corpus.add(".offline.txt", 1);
    fixture.write(
        ".github/settings.txt",
        "needle offline_hidden_edit_with_different_size\n",
    );
    fixture.rename("nested/moved", "nested/.returned");
    for name in ["child.txt", ".child.txt"] {
        corpus.remove(&format!("nested/moved/{name}"));
        corpus.add(&format!("nested/.returned/{name}"), 1);
    }
    fixture.write(".github/.gitignore", &format!("{INNER_IGNORE}live.txt\n"));
    corpus.remove(".github/live.txt");

    let mut server = ServerGuard::start(&fixture, &corpus, mode, false);
    wait_for_pattern(
        &fixture,
        &mut server,
        "offline_hidden_edit_with_different_size",
        &[".github/settings.txt"],
    );
    wait_for_pattern(
        &fixture,
        &mut server,
        "ignore_event_positive_control_longer",
        &[],
    );
}

#[test]
fn native_watcher_tracks_hidden_updates_ignore_transitions_and_restart() {
    exercise_hidden_updates(WatchMode::Native);
}

#[test]
fn polling_tracks_hidden_updates_ignore_transitions_and_restart() {
    exercise_hidden_updates(WatchMode::Poll);
}

#[test]
fn custom_index_inside_source_is_excluded_from_build_rebuild_and_reload() {
    for name in ["custom-index", ".custom-index"] {
        let mut fixture = Fixture::new();
        fixture.index = fixture.root.join(name);
        let corpus = Corpus::initial();

        for force in [false, true] {
            for path in ["visible.txt", ".secret.txt", ".nested/child.txt"] {
                write_file(&fixture.index, path, "needle index_output_must_not_leak\n");
            }
            fixture.build(force);
            assert_snapshot(&fixture, &corpus, Backend::Local);
        }

        for path in ["visible.txt", ".secret.txt", ".nested/child.txt"] {
            write_file(&fixture.index, path, "needle index_output_must_not_leak\n");
        }
        let mut server = ServerGuard::start(&fixture, &corpus, WatchMode::Disabled, false);
        server.rpc("reload");
        wait_for_snapshot(&fixture, &mut server, &corpus);
        server.stop();
        assert_snapshot(&fixture, &corpus, Backend::Local);
    }
}
