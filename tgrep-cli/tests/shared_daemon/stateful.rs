use super::*;
use std::collections::BTreeMap;

type Model = BTreeMap<String, String>;

const BASE_TEXT: &str = "pr\u{e9}face\nstate_needle base_only\ncontext\nstate_needle base_only\n";
const FOLDS: &str = "CAF\u{c9} \u{ff29}\u{ff24} \u{17f}hell\u{17f}hoc\u{212a}\n";

struct Replay {
    seed: u64,
    random: u64,
    operations: Vec<String>,
    started: Instant,
    rounds: usize,
}

impl Replay {
    fn new() -> Self {
        let seed = std::env::var("TGREP_SHARED_SEED")
            .map(|value| {
                value
                    .parse()
                    .expect("TGREP_SHARED_SEED must be a decimal u64")
            })
            .unwrap_or(174_2026);
        let rounds = std::env::var("TGREP_SHARED_ROUNDS")
            .map(|value| {
                value
                    .parse()
                    .expect("TGREP_SHARED_ROUNDS must be an integer")
            })
            .unwrap_or(1);
        assert!(
            (1..=16).contains(&rounds),
            "TGREP_SHARED_ROUNDS must be 1..=16"
        );
        Self {
            seed,
            random: seed,
            operations: Vec::new(),
            started: Instant::now(),
            rounds,
        }
    }

    fn next(&mut self) -> u64 {
        self.random = self
            .random
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1);
        self.random
    }

    fn record(&mut self, operation: impl Into<String>) {
        self.operations.push(operation.into());
        assert!(
            self.started.elapsed() < Duration::from_secs(300 * self.rounds as u64),
            "sequence exceeded its overall deadline"
        );
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!(
                "Replay: TGREP_SHARED_SEED={} TGREP_SHARED_ROUNDS={}\n{}",
                self.seed,
                self.rounds,
                self.operations
                    .iter()
                    .enumerate()
                    .map(|(i, op)| format!("{i}: {op}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
}

fn write(root: &Path, model: &mut Model, path: &str, text: &str) {
    fs::write(root.join(path), text).unwrap();
    model.insert(path.to_owned(), text.to_owned());
}

fn remove(root: &Path, model: &mut Model, path: &str) {
    fs::remove_file(root.join(path)).unwrap();
    assert!(model.remove(path).is_some());
}

fn transient(response: &Value) -> bool {
    let message = response["error"]["message"].as_str().unwrap_or("");
    response["error"]["code"] == -32001
        && [
            "worktree invalidated during reconciliation",
            "worktree view is not ready",
            "shared reconciliation in progress",
            "shared reconciliation or checkpoint publication incomplete",
            "view changed during query",
        ]
        .iter()
        .any(|part| message.contains(part))
}

fn rpc(d: &Daemon, method: &str, params: Value) -> Value {
    let started = Instant::now();
    loop {
        let response = d.try_rpc(method, params.clone()).unwrap();
        if let Some(result) = response.get("result") {
            assert_eq!(result["instance"], d.marker["instance"]);
            return result.clone();
        }
        assert!(transient(&response), "{method}: {response}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{method}: {response}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn barrier(d: &Daemon, root: &Path, lease: &Value, changed: &[&str], full: bool) {
    let result = rpc(
        d,
        "refresh",
        json!({
            "root":root,"view":lease["view"],"lease":lease["lease"],"changed":changed,"full":full
        }),
    );
    assert!(result["processed_epoch"].as_u64().unwrap() >= lease["epoch"].as_u64().unwrap());
    assert_eq!(result["generation"], lease["generation"]);
}

fn visible(model: &Model, path: &str, hidden: bool) -> bool {
    (hidden || !path.split('/').any(|component| component.starts_with('.')))
        && !model
            .get(".gitignore")
            .is_some_and(|rules| rules.lines().any(|rule| rule == path))
}

fn json_semantics(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .map(|line| {
            let mut row: Value = serde_json::from_str(line).unwrap();
            // Timings and search-work counters differ by backend, not match semantics.
            if row["type"] == "end" || row["type"] == "summary" {
                let stats = &row["data"]["stats"];
                row["data"] = json!({
                    "path":row["data"]["path"], "binary_offset":row["data"]["binary_offset"],
                    "matches":stats["matches"], "matched_lines":stats["matched_lines"]
                });
            }
            row
        })
        .collect()
}

fn cli_parity(d: &Daemon, root: &Path, args: &[&str]) -> Output {
    let mut shared = vec!["--sort", "path", "--color", "never", "--stats"];
    shared.extend_from_slice(args);
    let started = Instant::now();
    let indexed = loop {
        let before = d.lookup(root)["queries"].as_u64().unwrap();
        let output = cli(root, &shared);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let used = d.lookup(root)["queries"].as_u64().unwrap();
        if stderr.contains("via shared daemon v1") && used == before + 1 {
            break output;
        }
        // An event arriving after the acknowledged barrier may close the gate.
        // Never compare fallback output; only a successful shared attempt counts.
        assert!(
            stderr.contains("reconcil")
                || stderr.contains("not ready")
                || stderr.contains("changed during query"),
            "{args:?}: {output:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{args:?}: {output:?}"
        );
        d.ready(root);
    };
    let mut scan = vec!["--no-index"];
    // Explicit hidden scans can inspect Git internals; shared indexes exclude
    // metadata directories but intentionally include linked-worktree gitfiles.
    if root.join(".git").is_dir() {
        scan.extend(["-g", "!.git"]);
    }
    scan.extend(shared);
    let scanned = cli(root, &scan);
    assert!(matches!(indexed.status.code(), Some(0 | 1)), "{indexed:?}");
    assert_eq!(indexed.status.code(), scanned.status.code(), "{args:?}");
    if args.contains(&"--json") {
        assert_eq!(
            json_semantics(&indexed.stdout),
            json_semantics(&scanned.stdout),
            "{args:?}"
        );
    } else {
        assert_eq!(indexed.stdout, scanned.stdout, "{args:?}");
    }
    indexed
}

fn check(d: &Daemon, roots: &[PathBuf], leases: &[Value], models: &[Model], step: usize) {
    for (i, root) in roots.iter().enumerate() {
        let lease = &leases[i];
        let model = &models[i];
        for hidden in [false, true] {
            let result = rpc(
                d,
                "files",
                json!({"root":root,"view":lease["view"],"query":{"hidden":hidden}}),
            );
            assert_eq!(result["backend"], "shared-v1");
            assert_eq!(result["generation"], lease["generation"]);
            let mut expected: Vec<_> = model
                .keys()
                .filter(|path| visible(model, path, hidden))
                .cloned()
                .collect();
            if hidden && i != 0 {
                expected.push(".git".into());
            }
            expected.sort();
            assert_eq!(
                result["files"],
                json!(expected),
                "worktree {i}, step {step}"
            );
        }
        for pattern in [
            "state_needle",
            "base_only",
            "owner_0",
            "owner_1",
            "owner_2",
            "never_present",
        ] {
            let result = rpc(
                d,
                "search",
                json!({
                    "root":root,"view":lease["view"],"query":{"pattern":pattern,"fixed_string":true,"detail":true}
                }),
            );
            assert_eq!(result["backend"], "shared-v1");
            let expected: Vec<_> = model
                .iter()
                .filter(|(path, text)| {
                    visible(model, path, false) && !path.ends_with(".bin") && !text.contains('\0')
                })
                .flat_map(|(path, text)| {
                    text.lines()
                        .enumerate()
                        .filter(move |(_, text)| text.contains(pattern))
                        .map(move |(line, _)| (path.clone(), (line + 1) as u64))
                })
                .collect();
            let mut actual: Vec<_> = result["matches"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["type"] == "match")
                .map(|row| {
                    (
                        row["file"].as_str().unwrap().to_owned(),
                        row["line"].as_u64().unwrap(),
                    )
                })
                .collect();
            actual.sort();
            assert_eq!(actual, expected, "worktree {i}, step {step}, {pattern}");
        }
        let surfaces: &[&[&str]] = &[
            &["-n", "--", "state_needle", "."],
            &["--count-matches", "--", "state_needle", "."],
            &["--files", "--hidden", "."],
            &["--json", "-C", "1", "--", "state_needle", "."],
            &["-l", "--", "base_only", "."],
            &["--vimgrep", "--", "state_needle", "."],
            &["-c", "--", "state_needle", "."],
        ];
        cli_parity(d, root, surfaces[step % surfaces.len()]);
        // #174 folds must survive base reuse, whole-file overrides, ignores and restart.
        let pattern = ["shellshock", "caf\u{e9}", "\u{ff49}\u{ff44}"][step % 3];
        let result = cli_parity(d, root, &["--json", "-F", "-i", "--", pattern, "."]);
        let rows = json_semantics(&result.stdout);
        let matches: Vec<_> = rows.iter().filter(|row| row["type"] == "match").collect();
        let expected =
            usize::from(model.contains_key("unicode.txt") && visible(model, "unicode.txt", false));
        assert_eq!(matches.len(), expected, "worktree {i}, step {step}");
        if expected == 1 {
            assert!(
                matches[0]["data"]["path"]["text"]
                    .as_str()
                    .unwrap()
                    .ends_with("unicode.txt")
            );
            assert_eq!(matches[0]["data"]["line_number"], 1);
            assert_eq!(
                matches[0]["data"]["submatches"].as_array().unwrap().len(),
                1
            );
        }
        let absent = cli_parity(d, root, &["-l", "--", "never_present", "."]);
        assert_eq!(absent.status.code(), Some(1));
        assert!(absent.stdout.is_empty());
    }
}

fn sequence(watch: bool) {
    let mut replay = Replay::new();
    replay.record(format!("start watch={watch}"));
    let mut f = Fixture::new();
    let mut base = Model::from([
        (
            "src/main.rs".into(),
            "fn shared_term() {}\ncontext line\nshared_term again\n".into(),
        ),
        ("notes.txt".into(), "shared_term notes\n".into()),
        (".hidden".into(), "shared_term hidden\n".into()),
    ]);
    for (path, text) in [
        ("model.txt", BASE_TEXT),
        ("unicode.txt", FOLDS),
        ("empty.txt", ""),
        ("asset.bin", "state_needle filename only\n"),
        ("binary.txt", "state_needle\0binary\n"),
        (".gitattributes", "eol.txt text eol=crlf\n"),
        ("eol.txt", "state_needle transformed\n"),
    ] {
        write(&f.a, &mut base, path, text);
    }
    git(&f.a, &["add", "."]);
    git(&f.a, &["commit", "-qm", "stateful base"]);
    f.revision = git(&f.a, &["rev-parse", "HEAD"]);
    git(&f.b, &["reset", "--hard", &f.revision]);
    let roots = vec![f.a.clone(), f.b.clone(), f.third(&f.revision)];
    assert!(
        fs::read(roots[2].join("eol.txt"))
            .unwrap()
            .windows(2)
            .any(|bytes| bytes == b"\r\n")
    );
    let options = if watch {
        vec![
            "--watch-mode",
            "auto",
            "--poll-interval",
            "120",
            "--shared-max-views",
            "3",
        ]
    } else {
        vec!["--no-watch"]
    };
    let mut d = f.start(&options);
    let mut leases: Vec<_> = roots
        .iter()
        .map(|root| d.attach(root, &f.revision))
        .collect();
    let generation = leases[0]["generation"].clone();
    for root in &roots {
        let status = d.lookup(root);
        assert_eq!(status["generation"], generation);
        assert_eq!(status["base_sharing_views"], 3);
        assert_eq!(
            status["watch_mode"],
            if watch { "native" } else { "disabled" }
        );
    }
    let mut models = vec![base.clone(); 3];
    let mut step = 0;
    for round in 0..replay.rounds {
        let p = (replay.next() % 3) as usize;
        let s = (p + 1) % 3;
        let upstream = (p + 2) % 3;
        replay.record(format!(
            "round {round}: primary={p}, sibling={s}, upstream={upstream}"
        ));
        if round > 0 {
            for i in 0..3 {
                git(&roots[i], &["reset", "--hard", &f.revision]);
                models[i] = base.clone();
                barrier(&d, &roots[i], &leases[i], &[], true);
            }
        }
        check(&d, &roots, &leases, &models, step);
        for i in [p, s] {
            let nonce = replay.next();
            replay.record(format!(
                "edit worktree {i}: owner_{i} nonce={nonce}; overlay Unicode"
            ));
            let before = d.lookup(&roots[i])["published_epoch"].as_u64().unwrap();
            write(
                &roots[i],
                &mut models[i],
                "model.txt",
                &format!("state_needle owner_{i} {nonce}\ncontext\nstate_needle owner_{i}\n"),
            );
            write(
                &roots[i],
                &mut models[i],
                "unicode.txt",
                "caf\u{e9} \u{ff49}\u{ff44} SHELLSHOCK\n",
            );
            if watch {
                let started = Instant::now();
                loop {
                    let result = rpc(
                        &d,
                        "search",
                        json!({
                            "root":roots[i],"view":leases[i]["view"],"query":{"pattern":format!("owner_{i}")}
                        }),
                    );
                    assert_eq!(result["backend"], "shared-v1");
                    if result["epoch"].as_u64().unwrap() > before
                        && !result["matches"].as_array().unwrap().is_empty()
                    {
                        break;
                    }
                    assert!(
                        started.elapsed() < Duration::from_secs(20),
                        "native events did not publish {result}"
                    );
                    thread::sleep(Duration::from_millis(20));
                }
            }
            barrier(
                &d,
                &roots[i],
                &leases[i],
                &["model.txt", "unicode.txt"],
                false,
            );
        }
        step += 1;
        check(&d, &roots, &leases, &models, step);

        replay.record("stage addition and edit; then unstaged rewrite and untracked addition");
        write(
            &roots[p],
            &mut models[p],
            "added.txt",
            "state_needle staged\n",
        );
        git(&roots[p], &["add", "model.txt", "added.txt"]);
        write(
            &roots[p],
            &mut models[p],
            "added.txt",
            "state_needle unstaged state_needle\n",
        );
        write(
            &roots[p],
            &mut models[p],
            "untracked.txt",
            "state_needle untracked\n",
        );
        barrier(
            &d,
            &roots[p],
            &leases[p],
            &["added.txt", "untracked.txt"],
            false,
        );
        step += 1;
        check(&d, &roots, &leases, &models, step);

        replay.record("rename notes, delete base model, commit divergence");
        fs::rename(roots[p].join("notes.txt"), roots[p].join("renamed.txt")).unwrap();
        let notes = models[p].remove("notes.txt").unwrap();
        models[p].insert("renamed.txt".into(), notes);
        remove(&roots[p], &mut models[p], "model.txt");
        barrier(
            &d,
            &roots[p],
            &leases[p],
            &["notes.txt", "renamed.txt", "model.txt"],
            false,
        );
        step += 1;
        check(&d, &roots, &leases, &models, step);
        git(&roots[p], &["add", "-A"]);
        git(&roots[p], &["commit", "-qm", "stateful divergence"]);
        let divergent_commit = git(&roots[p], &["rev-parse", "HEAD"]);
        let divergent = models[p].clone();
        barrier(&d, &roots[p], &leases[p], &[], true);
        step += 1;
        check(&d, &roots, &leases, &models, step);

        replay.record("ignore tracked overlay and base files");
        write(
            &roots[p],
            &mut models[p],
            ".gitignore",
            "added.txt\nunicode.txt\neol.txt\n",
        );
        barrier(&d, &roots[p], &leases[p], &[".gitignore"], false);
        step += 1;
        check(&d, &roots, &leases, &models, step);
        replay.record("remove ignores; restore pinned file while HEAD remains divergent");
        remove(&roots[p], &mut models[p], ".gitignore");
        git(
            &roots[p],
            &[
                "restore",
                "--source",
                &f.revision,
                "--worktree",
                "--",
                "model.txt",
            ],
        );
        models[p].insert("model.txt".into(), BASE_TEXT.into());
        barrier(
            &d,
            &roots[p],
            &leases[p],
            &[".gitignore", "model.txt"],
            false,
        );
        step += 1;
        check(&d, &roots, &leases, &models, step);

        replay.record("reset to pinned base, then checkout divergent commit");
        git(&roots[p], &["reset", "--hard", &f.revision]);
        models[p] = base.clone();
        barrier(&d, &roots[p], &leases[p], &[], true);
        step += 1;
        check(&d, &roots, &leases, &models, step);
        git(&roots[p], &["checkout", "--detach", &divergent_commit]);
        models[p] = divergent;
        barrier(&d, &roots[p], &leases[p], &[], true);
        step += 1;
        check(&d, &roots, &leases, &models, step);

        replay.record("advance independent upstream and rebase; keep exact old generation pinned");
        write(
            &roots[upstream],
            &mut models[upstream],
            "upstream.txt",
            "state_needle upstream\n",
        );
        git(&roots[upstream], &["add", "upstream.txt"]);
        git(&roots[upstream], &["commit", "-qm", "independent upstream"]);
        let upstream_commit = git(&roots[upstream], &["rev-parse", "HEAD"]);
        git(
            &roots[p],
            &["rebase", "--onto", &upstream_commit, &f.revision],
        );
        models[p].insert("upstream.txt".into(), "state_needle upstream\n".into());
        for i in [p, upstream] {
            barrier(&d, &roots[i], &leases[i], &[], true);
        }
        step += 1;
        check(&d, &roots, &leases, &models, step);
        replay.record("sparse checkout then rematerialize transformed checkout");
        git(
            &roots[upstream],
            &["sparse-checkout", "set", "--no-cone", "src/"],
        );
        let complete = models[upstream].clone();
        models[upstream].retain(|path, _| path.starts_with("src/"));
        barrier(&d, &roots[upstream], &leases[upstream], &[], true);
        step += 1;
        check(&d, &roots, &leases, &models, step);
        git(&roots[upstream], &["sparse-checkout", "disable"]);
        models[upstream] = complete;
        barrier(&d, &roots[upstream], &leases[upstream], &[], true);
        step += 1;
        check(&d, &roots, &leases, &models, step);

        replay
            .record("crash/restart; fresh caller tokens, checkpoint revalidation, unchanged pins");
        let old_instance = d.marker["instance"].clone();
        d.stop();
        d = f.start(&options);
        assert_ne!(d.marker["instance"], old_instance);
        for i in 0..3 {
            let previous = leases[i].clone();
            leases[i] = d.attach(&roots[i], &f.revision);
            assert_ne!(leases[i]["lease"], previous["lease"]);
            assert_eq!(leases[i]["generation"], generation);
            assert_eq!(leases[i]["checkpoint_restored"], true);
            barrier(&d, &roots[i], &leases[i], &[], true);
        }
        step += 1;
        check(&d, &roots, &leases, &models, step);
    }
}

#[test]
fn seeded_shared_search_parity_explicit_refresh() {
    sequence(false);
}

#[test]
fn seeded_shared_search_parity_native_watcher() {
    sequence(true);
}
