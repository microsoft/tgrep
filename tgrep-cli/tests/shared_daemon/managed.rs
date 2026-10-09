// Copyright (c) Microsoft Corporation. All rights reserved.

use super::*;
use tgrep_core::managed::{OwnerClaim, OwnerGuard};

#[cfg(feature = "managed-test-hooks")]
#[path = "managed_faults.rs"]
mod faults;

#[cfg(feature = "managed-test-hooks")]
#[path = "managed_clients.rs"]
mod clients;

#[cfg(feature = "managed-test-hooks")]
#[path = "managed_live.rs"]
mod live;

#[cfg(feature = "managed-test-hooks")]
#[path = "managed_performance.rs"]
mod performance;

fn policy() -> Value {
    json!({
        "schema":2,"storage":"managed","retention":{"mode":"retain-all"},"advancement":{"mode":"fixed"},
        "work":{
            "max_views":8,"max_leases":32,"workers":2,"queue_items":16,
            "staging_bytes":67108864,"private_work_bytes":67108864,
            "sort_buffer_bytes":1048576,"blob_bytes":1048576,"operation_timeout_ms":30000,
            "page_objects":16,"max_cursors":8,"cursor_lifetime_ms":30000,"max_receipts":1024,
            "metadata_bytes":16777216
        },
        "collection":{
            "schedule":{"mode":"disabled"},"on_pressure":false,
            "checkpoint_grace_ms":0,"generation_grace_ms":0,
            "max_duration_ms":1000,"max_examined":64,"max_removed":16,"max_delete_bytes":1048576,
            "chunk_bytes":65536,"max_pages":4,"retry_ms":100
        }
    })
}

fn start(fixture: &Fixture, policy: &Value, watch: &[&str]) -> Daemon {
    let path = fixture.storage.join("policy.json");
    fs::write(&path, serde_json::to_vec(policy).unwrap()).unwrap();
    let mut args = vec!["--shared-policy", path.to_str().unwrap()];
    args.extend_from_slice(watch);
    fixture.start(&args)
}

fn owner(daemon: &Daemon) -> (OwnerClaim, OwnerGuard) {
    let prepared = daemon.rpc(
        "owners.prepare",
        json!({"token":format!("bootstrap-{}",LEASE_SEQUENCE.fetch_add(1,Ordering::Relaxed))}),
    );
    let claim: OwnerClaim = serde_json::from_value(prepared["claim"].clone()).unwrap();
    let guard = OwnerGuard::claim(claim.clone()).unwrap();
    daemon.rpc("owners.register", json!({"claim":claim}));
    (claim, guard)
}

fn token(claim: &OwnerClaim, sequence: u64) -> Value {
    json!({"scope":claim.owner,"sequence":sequence,"token":format!("operation-{sequence}")})
}

fn attach_input(fixture: &Fixture, claim: &OwnerClaim) -> Value {
    json!({
        "token":token(claim,1),
        "request":{"root":fs::canonicalize(&fixture.a).unwrap(),"revision":fixture.revision,
            "profile":serde_json::from_str::<Value>(PROFILE).unwrap(),
            "lease":"managed-client","owner":claim.owner,"accept_current":null,"migratable":true,"allocation_version":1}
    })
}

fn completed(daemon: &Daemon, operation: &Value) -> Value {
    let record = terminal(daemon, operation);
    assert_eq!(
        record["state"], "completed",
        "operation did not succeed: {record}"
    );
    record
}

fn terminal(daemon: &Daemon, operation: &Value) -> Value {
    terminal_before(daemon, operation, Instant::now() + Duration::from_secs(30))
}

fn terminal_before(daemon: &Daemon, operation: &Value, deadline: Instant) -> Value {
    loop {
        let record = daemon.rpc("operations.inspect", json!({"id":operation["id"]}));
        match record["state"].as_str().unwrap() {
            "completed" | "failed" | "cancelled" => return record,
            _ => assert!(
                Instant::now() < deadline,
                "operation did not complete: {record}"
            ),
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn search(daemon: &Daemon, root: &Path) -> Value {
    let view = daemon.rpc("lookup", json!({"root":fs::canonicalize(root).unwrap()}));
    let result = retry_busy(
        daemon,
        "search",
        json!({
            "root":view["root"],"view":view["view"],"expected_version":view["version"],
            "query":{"pattern":"shared_term","hidden":true}
        }),
    );
    assert_eq!(result["backend"], "shared-v2");
    assert_eq!(result["ready"], true);
    result
}

fn stop(daemon: &mut Daemon) {
    let started = Instant::now();
    let mut request = idle_request(daemon);
    loop {
        let response = daemon.try_rpc("stop-if-idle", request.clone()).unwrap();
        if let Some(error) = response.get("error") {
            assert_eq!(error["data"]["category"], "busy", "{response}");
            assert_eq!(error["data"]["retryable"], true, "{response}");
            assert_eq!(
                error["data"]["committed_state"], "not-committed",
                "{response}"
            );
        } else if response["result"]["data"]["stopping"] == true {
            break;
        } else {
            request = idle_request(daemon);
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "idle shutdown is still busy: {response}"
        );
        thread::sleep(Duration::from_millis(20));
    }
    while daemon.child.try_wait().unwrap().is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "stopped daemon did not exit"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn idle_request(daemon: &Daemon) -> Value {
    let sequence = LEASE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    json!({"token":{"scope":daemon.marker["namespace"],"sequence":sequence,"token":format!("idle-{sequence}")}})
}

fn retry_busy(daemon: &Daemon, method: &str, params: Value) -> Value {
    let response = retry_busy_response(daemon, method, params);
    assert!(response.get("error").is_none(), "{method}: {response}");
    assert_eq!(response["result"]["instance"], daemon.marker["instance"]);
    assert_eq!(response["result"]["namespace"], daemon.marker["namespace"]);
    response["result"]["data"].clone()
}

fn retry_busy_response(daemon: &Daemon, method: &str, params: Value) -> Value {
    let started = Instant::now();
    loop {
        let response = daemon.try_rpc(method, params.clone()).unwrap();
        if let Some(error) = response.get("error")
            && error["data"]["category"] == "busy"
        {
            assert_eq!(error["data"]["retryable"], true, "{response}");
            assert_eq!(
                error["data"]["committed_state"], "not-committed",
                "{response}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{method} remains busy: {response}"
            );
            thread::sleep(Duration::from_millis(20));
        } else {
            return response;
        }
    }
}

fn legacy_cli(root: &Path, args: &[&str]) -> Output {
    let binary = std::env::var_os("TGREP_V1_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| assert_cmd::cargo::cargo_bin("tgrep"));
    runtime::output(Command::new(binary).current_dir(root).args(args))
}

fn offline(fixture: &Fixture, namespace: &str, method: &str, params: Value, apply: bool) -> Value {
    let params = params.to_string();
    let mut args = vec![
        "shared",
        "maintenance",
        namespace,
        method,
        "--params",
        &params,
    ];
    if apply {
        args.push("--apply");
    }
    let response: Value = serde_json::from_str(&success(cli(fixture.temp.path(), &args))).unwrap();
    assert_eq!(response["ok"], true, "{response}");
    response["result"].clone()
}

fn canonical_matches(output: Output) -> Vec<Value> {
    let mut rows: Vec<Value> = success(output)
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["type"] == "match")
        .map(|row| row["data"].clone())
        .collect();
    rows.sort_by_key(Value::to_string);
    rows
}

fn assert_scan_parity(daemon: &Daemon, root: &Path, home: &Path) {
    let path_key = |path: &str| {
        Path::new(path)
            .components()
            .filter(|component| *component != std::path::Component::CurDir)
            .collect::<PathBuf>()
    };
    let invoke = |args: &[&str]| {
        runtime::output(
            Command::new(assert_cmd::cargo::cargo_bin("tgrep"))
                .current_dir(root)
                .env("HOME", home)
                .env("USERPROFILE", home)
                .env("XDG_CONFIG_HOME", home)
                .args(args),
        )
    };
    let view = daemon.rpc("lookup", json!({"root":fs::canonicalize(root).unwrap()}));
    for hidden in [false, true] {
        let mut shared_files = Vec::new();
        for method in ["search", "files"] {
            let query = if method == "search" {
                json!({"pattern":"shared_term","hidden":hidden})
            } else {
                json!({"hidden":hidden})
            };
            let result = retry_busy(
                daemon,
                method,
                json!({
                    "root":view["root"],"view":view["view"],"expected_version":view["version"],"query":query
                }),
            );
            assert_eq!(result["backend"], "shared-v2", "{result}");
            assert_eq!(result["ready"], true, "{result}");
            if method == "files" {
                shared_files = result["files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|path| path_key(path.as_str().unwrap()))
                    .collect();
                shared_files.sort();
            }
        }
        // Indexed corpora exclude Git metadata directories; explicit raw scans do not.
        let mut args = vec!["--stats", "--json", "-F", "--glob", "!**/.git/**"];
        if hidden {
            args.push("--hidden");
        }
        args.extend(["--", "shared_term", "."]);
        let indexed = invoke(&args);
        args.insert(0, "--no-index");
        let scan = invoke(&args);
        assert!(
            String::from_utf8_lossy(&indexed.stderr).contains("(via shared daemon v2)"),
            "CLI did not prove shared-v2 service: {}",
            String::from_utf8_lossy(&indexed.stderr)
        );
        assert_eq!(canonical_matches(indexed), canonical_matches(scan));
        let mut args = vec!["--files", "--glob", "!**/.git/**"];
        if hidden {
            args.push("--hidden");
        }
        args.push(".");
        let indexed = invoke(&args);
        args.insert(0, "--no-index");
        let scan = invoke(&args);
        let paths = |output: Output| {
            let mut paths: Vec<String> = success(output).lines().map(str::to_owned).collect();
            paths.sort();
            paths
        };
        assert!(
            indexed.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&indexed.stderr)
        );
        let scanned = paths(scan);
        assert_eq!(paths(indexed), scanned);
        let mut scanned: Vec<PathBuf> = scanned.iter().map(|path| path_key(path)).collect();
        scanned.sort();
        assert_eq!(shared_files, scanned);
    }
}

#[test]
fn bounded_ignore_inputs_preserve_scan_parity_and_leave_sibling_views_serviceable() {
    let fixture = Fixture::new();
    let home = fixture.temp.path().join("home");
    fs::create_dir(&home).unwrap();
    fs::write(
        home.join(".gitconfig"),
        "[core]\nexcludesFile = ~/global-ignore\n",
    )
    .unwrap();
    fs::write(home.join("global-ignore"), "global.skip\n").unwrap();
    let policy_path = fixture.storage.join("policy.json");
    fs::write(&policy_path, serde_json::to_vec(&policy()).unwrap()).unwrap();
    let mut daemon = Daemon::start_with_home(
        &fixture.a,
        &fixture.storage,
        &[
            "--shared-policy",
            policy_path.to_str().unwrap(),
            "--no-watch",
        ],
        Some(&home),
    );
    let (claim, guard) = owner(&daemon);
    completed(
        &daemon,
        &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
    );
    let mut sibling = attach_input(&fixture, &claim);
    sibling["token"] = token(&claim, 2);
    sibling["request"]["root"] = json!(fs::canonicalize(&fixture.b).unwrap());
    sibling["request"]["lease"] = json!("sibling-client");
    completed(&daemon, &daemon.rpc("views.attach", sibling));
    let view = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    let other = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.b).unwrap()}),
    );
    assert_eq!(view["incarnation"], other["incarnation"]);

    git(&fixture.a, &["config", "core.ignorecase", "true"]);
    fs::write(
        fixture.a.join("Mixed.Cs"),
        "shared_term tracked exemption\n",
    )
    .unwrap();
    git(&fixture.a, &["add", "Mixed.Cs"]);
    git(
        &fixture.a,
        &["commit", "-qm", "new tracked path on fixed pin"],
    );
    git(&fixture.a, &["mv", "notes.txt", "renamed.txt"]);
    fs::write(
        fixture.a.join("src").join("main.rs"),
        "shared_term staged\n",
    )
    .unwrap();
    git(&fixture.a, &["add", "src/main.rs"]);
    fs::write(
        fixture.a.join("src").join("main.rs"),
        "shared_term dirty after staging\n",
    )
    .unwrap();
    fs::write(fixture.a.join(".gitignore"), "*.CS\nignored-dir/\n").unwrap();
    fs::write(fixture.a.join(".ignore"), "!.hidden\n").unwrap();
    fs::write(fixture.temp.path().join(".ignore"), "parent.skip\n").unwrap();
    fs::write(fixture.a.join("p4ignore.ini"), "p4\\*.txt\n").unwrap();
    let repository = tgrep_core::generations::Repository::discover(&fixture.a).unwrap();
    fs::write(
        repository.common_dir().join("info").join("exclude"),
        "excluded.local\n",
    )
    .unwrap();
    for path in [
        "untracked.txt",
        "untracked.cs",
        "global.skip",
        "parent.skip",
        "excluded.local",
    ] {
        fs::write(fixture.a.join(path), "shared_term private file\n").unwrap();
    }
    for directory in ["nested", "p4", "ignored-dir"] {
        fs::create_dir(fixture.a.join(directory)).unwrap();
    }
    fs::write(fixture.a.join("nested").join(".gitignore"), "nested.skip\n").unwrap();
    fs::write(fixture.a.join("nested").join(".ignore"), "dot.skip\n").unwrap();
    for path in [
        PathBuf::from("nested").join("nested.skip"),
        PathBuf::from("nested").join("dot.skip"),
        PathBuf::from("nested").join("visible.txt"),
        PathBuf::from("p4").join("private.txt"),
        PathBuf::from("ignored-dir").join("private.txt"),
    ] {
        fs::write(fixture.a.join(path), "shared_term nested file\n").unwrap();
    }

    let refresh =
        |sequence: u64| {
            daemon.rpc(
                "views.invalidate",
                json!({"view":view["view"],"expected_version":1,
            "owner":claim.owner,"changed":[],"full":true}),
            );
            daemon.rpc("views.refresh", json!({"token":token(&claim,sequence),"request":{
            "view":view["view"],"expected_version":1,"owner":claim.owner,"allocation_version":1
        }}))
        };
    completed(&daemon, &refresh(3));
    assert!(
        search(&daemon, &fixture.a)
            .to_string()
            .contains("tracked exemption")
    );
    assert_scan_parity(&daemon, &fixture.a, &home);
    assert_scan_parity(&daemon, &fixture.b, &home);
    fs::write(
        fixture.a.join(".ignore"),
        format!("#{}\n", "x".repeat(1024 * 1024)),
    )
    .unwrap();
    let failed = terminal(&daemon, &refresh(4));
    assert_eq!(failed["error"]["category"], "resource-pressure", "{failed}");
    assert_eq!(
        failed["error"]["reason_code"], "ignore-input-byte-limit",
        "{failed}"
    );
    assert_eq!(failed["committed_state"], "not-committed", "{failed}");
    assert_eq!(
        daemon.rpc("views.status", json!({"id":view["view"]}))["ready"],
        false
    );
    assert_scan_parity(&daemon, &fixture.b, &home);
    fs::write(fixture.a.join(".ignore"), "!.hidden\n").unwrap();
    completed(&daemon, &refresh(5));
    assert_scan_parity(&daemon, &fixture.a, &home);
    daemon.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"managed-client"}),
    );
    daemon.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"sibling-client"}),
    );
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
}

#[test]
fn offline_collection_after_repository_deletion_has_exact_replay_and_accounting() {
    let fixture = Fixture::new();
    let mut configured = policy();
    configured["retention"] = json!({"mode":"bounded","target_bytes":1});
    let mut daemon = start(&fixture, &configured, &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    completed(
        &daemon,
        &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
    );
    search(&daemon, &fixture.a);
    let namespace = daemon.marker["storage"].as_str().unwrap().to_owned();
    let scope = daemon.marker["namespace"].clone();
    daemon.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"managed-client"}),
    );
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
    fs::remove_dir_all(&fixture.b).unwrap();
    fs::remove_dir_all(&fixture.a).unwrap();
    let foreign = fixture.storage.join("foreign.txt");
    fs::write(&foreign, "not a catalog-owned object").unwrap();

    let preview = offline(
        &fixture,
        &namespace,
        "collections.preview",
        json!({}),
        false,
    );
    assert!(
        preview["objects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|object| object["eligible"] == true),
        "{preview}"
    );
    let status = offline(&fixture, &namespace, "namespace.status", json!({}), false);
    let before = status["usage"]["object_logical_bytes"].as_u64().unwrap();
    assert!(before > 0);
    let mut reclaimed = 0;
    let mut next = Value::Null;
    for pass in 0..64 {
        let request = json!({
            "token":{"scope":scope,"sequence":10000+pass,"token":format!("offline-{pass}")},
            "request":{"policy_version":1,"allocation_version":1,
                "bounds":{"max_duration_ms":1000,"max_examined":64,"max_removed":16,
                    "max_delete_bytes":1048576,"max_pages":4},"cursor":next}
        });
        let result = offline(
            &fixture,
            &namespace,
            "collections.run",
            request.clone(),
            true,
        );
        assert_eq!(
            offline(&fixture, &namespace, "collections.run", request, true),
            result
        );
        let progress = &result["progress"];
        assert_eq!(progress["omitted_skip_details"], 0);
        assert!(
            progress["details"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["error"].is_null()),
            "{result}"
        );
        reclaimed += progress["logical_bytes_reclaimed"].as_u64().unwrap();
        next = progress["next"].clone();
        let page = offline(&fixture, &namespace, "objects.page", json!({}), false);
        if page["objects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|object| object["state"] == "removed")
        {
            break;
        }
        assert!(pass < 63, "collection did not converge: {page}");
    }
    assert_eq!(reclaimed, before);
    let page = offline(&fixture, &namespace, "objects.page", json!({}), false);
    assert!(
        page["objects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|object| object["state"] == "removed"),
        "{page}"
    );
    assert_eq!(
        fs::read_to_string(foreign).unwrap(),
        "not a catalog-owned object"
    );
}

#[test]
fn compatibility_v1_cli_lease_blocks_v2_migration_until_explicit_release() {
    let fixture = Fixture::new();
    let mut policy = policy();
    policy["storage"] = json!("compatibility-retain-all");
    let mut daemon = start(&fixture, &policy, &["--no-watch"]);
    let legacy: Value = serde_json::from_str(&success(legacy_cli(
        &fixture.a,
        &[
            "shared",
            "attach",
            ".",
            "--revision",
            &fixture.revision,
            "--lease",
            "actual-v1",
        ],
    )))
    .unwrap();
    assert_eq!(legacy["protocol"], 1);
    assert_eq!(legacy["ready"], true);
    assert_eq!(legacy["generation"]["schema"], 1);
    let legacy_search = legacy_cli(&fixture.a, &["--json", "-F", "--", "shared_term", "."]);
    assert!(
        legacy_search.status.success(),
        "{}",
        String::from_utf8_lossy(&legacy_search.stderr)
    );
    assert!(String::from_utf8_lossy(&legacy_search.stdout).contains("shared_term"));
    assert!(!String::from_utf8_lossy(&legacy_search.stderr).contains("falling back"));
    search(&daemon, &fixture.a);

    let (claim, guard) = owner(&daemon);
    let mut input = attach_input(&fixture, &claim);
    input["request"]["revision"] = Value::Null;
    input["request"]["accept_current"] = json!({"view":legacy["view"],"version":1});
    completed(&daemon, &daemon.rpc("views.attach", input));
    fs::write(fixture.a.join("notes.txt"), "shared_term newer commit\n").unwrap();
    git(&fixture.a, &["add", "notes.txt"]);
    git(&fixture.a, &["commit", "-qm", "new pin"]);
    let target = git(&fixture.a, &["rev-parse", "HEAD"]);
    let mut migration = json!({"token":token(&claim,2),"request":{
        "view":legacy["view"],"root":legacy["root"],"expected_version":1,"target_commit":target,
        "profile":serde_json::from_str::<Value>(PROFILE).unwrap(),"owner":claim.owner,"allocation_version":1
    }});
    let blocked = terminal(&daemon, &daemon.rpc("views.advance", migration.clone()));
    assert_eq!(blocked["error"]["reason_code"], "fixed-pin-participant");
    let mut recovery = idle_request(&daemon);
    recovery["request"] = json!({"cursor":null});
    let recovered = retry_busy(&daemon, "maintenance.recover", recovery);
    assert_eq!(recovered["state"], "completed");
    assert_eq!(recovered["result"]["owners_reaped"], 0);
    assert_eq!(
        daemon.rpc("views.status", json!({"id":legacy["view"]}))["leases"],
        2
    );
    let detached: Value = serde_json::from_str(&success(legacy_cli(
        &fixture.a,
        &["shared", "detach", ".", "--lease", "actual-v1"],
    )))
    .unwrap();
    assert_eq!(detached["remaining_leases"], 1);
    migration["token"] = token(&claim, 3);
    completed(&daemon, &daemon.rpc("views.advance", migration));
    assert_eq!(search(&daemon, &fixture.a)["version"], 2);
    daemon.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"managed-client"}),
    );
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
}

#[test]
fn v2_cli_real_backend_migration_replay_and_idle_shutdown() {
    let fixture = Fixture::new();
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let hello = daemon.rpc("hello", json!({}));
    assert_eq!(hello["storage"], "managed");
    let (claim, guard) = owner(&daemon);
    let input = attach_input(&fixture, &claim);
    let accepted = daemon.rpc("views.attach", input.clone());
    let original = completed(&daemon, &accepted);
    let before = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    assert_eq!(before["version"], 1);
    search(&daemon, &fixture.a);

    let indexed = cli(
        &fixture.a,
        &[
            "--stats",
            "--hidden",
            "--json",
            "-F",
            "--",
            "shared_term",
            ".",
        ],
    );
    let scan = cli(
        &fixture.a,
        &[
            "--no-index",
            "--hidden",
            "--json",
            "-F",
            "--",
            "shared_term",
            ".",
        ],
    );
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let matches = |output: &Output| {
        let mut rows: Vec<Value> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["type"] == "match")
            .map(|row| row["data"].clone())
            .collect();
        rows.sort_by_key(Value::to_string);
        rows
    };
    assert_eq!(matches(&indexed), matches(&scan));
    assert!(
        String::from_utf8_lossy(&indexed.stderr).contains("(via shared daemon v2)"),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let indexed_files = cli(&fixture.a, &["--files", "--stats", "--sort", "path", "."]);
    let scanned_files = cli(
        &fixture.a,
        &["--files", "--no-index", "--sort", "path", "."],
    );
    assert!(
        String::from_utf8_lossy(&indexed_files.stderr).contains("(via shared daemon v2)"),
        "{}",
        String::from_utf8_lossy(&indexed_files.stderr)
    );
    assert_eq!(success(indexed_files), success(scanned_files));

    fs::write(
        fixture.a.join("notes.txt"),
        "shared_term committed change\n",
    )
    .unwrap();
    git(&fixture.a, &["add", "notes.txt"]);
    git(&fixture.a, &["commit", "-qm", "advance"]);
    let target = git(&fixture.a, &["rev-parse", "HEAD"]);
    fs::write(
        fixture.a.join("notes.txt"),
        "shared_term private edit after commit\n",
    )
    .unwrap();
    let migration = json!({"token":token(&claim,2),"request":{
        "view":before["view"],"root":before["root"],"expected_version":1,"target_commit":target,
        "profile":serde_json::from_str::<Value>(PROFILE).unwrap(),"owner":claim.owner,"allocation_version":1
    }});
    let accepted = daemon.rpc("views.advance", migration.clone());
    let advanced = completed(&daemon, &accepted);
    assert_eq!(advanced["committed_state"], "committed");
    assert_eq!(daemon.rpc("views.advance", migration), advanced);
    assert_eq!(daemon.rpc("views.attach", input), original);
    let after = daemon.rpc("lookup", json!({"root":before["root"]}));
    assert_eq!(after["version"], 2);
    assert_eq!(after["commit"], target);
    let result = search(&daemon, &fixture.a);
    assert!(
        result.to_string().contains("private edit after commit"),
        "{result}"
    );
    let stale = daemon.try_rpc("search", json!({
        "root":before["root"],"view":before["view"],"expected_version":1,"query":{"pattern":"shared_term"}
    })).unwrap();
    assert_eq!(stale["error"]["data"]["category"], "stale-version");
    let idle = retry_busy(&daemon, "stop-if-idle", idle_request(&daemon));
    assert_eq!(idle["stopping"], false, "{idle}");
    assert!(idle["leases"].as_u64().unwrap() > 0, "{idle}");
    let detached = daemon.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"managed-client"}),
    );
    assert_eq!(detached["lease_released"], true);
    assert_eq!(detached["root_handles_released"], true);
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
}
