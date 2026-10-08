// Copyright (c) Microsoft Corporation. All rights reserved.

use super::*;
use std::sync::Arc;
use tgrep_core::managed::Id;

pub(super) fn manage(root: &Path, method: &str, params: Value) -> Value {
    let params = params.to_string();
    let response: Value = serde_json::from_str(&success(cli(
        root,
        &["shared", "manage", ".", method, "--params", &params],
    )))
    .unwrap();
    assert_eq!(response["ok"], true, "{response}");
    response["result"].clone()
}

fn wait_json(process: &mut runtime::Process, flag: &str) -> Value {
    let started = Instant::now();
    loop {
        for line in String::from_utf8_lossy(&process.stdout()).lines() {
            if let Ok(value) = serde_json::from_str::<Value>(line)
                && value[flag] == true
            {
                return value;
            }
        }
        assert!(
            process.running(),
            "owned helper exited before readiness: {:?}",
            process.finish(Duration::from_secs(1))
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "owned helper readiness deadline"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

pub(super) fn held_owner(fixture: &Fixture, daemon: &Daemon) -> (OwnerClaim, runtime::Process) {
    let prepared = daemon.rpc(
        "owners.prepare",
        json!({
            "token":format!("process-owner-{}", LEASE_SEQUENCE.fetch_add(1, Ordering::Relaxed))
        }),
    );
    let claim: OwnerClaim = serde_json::from_value(prepared["claim"].clone()).unwrap();
    let path = fixture
        .temp
        .path()
        .join(format!("{}.claim.json", claim.owner));
    fs::write(&path, serde_json::to_vec(&claim).unwrap()).unwrap();
    let mut child =
        runtime::Process::start(Command::new(assert_cmd::cargo::cargo_bin("tgrep")).args([
            "shared",
            "owner-hold",
            "--claim",
            path.to_str().unwrap(),
        ]));
    let holding = wait_json(&mut child, "holding");
    assert_eq!(holding["claim"], serde_json::to_value(&claim).unwrap());
    manage(&fixture.a, "owners.register", json!({"claim":claim}));
    (claim, child)
}

#[test]
fn actual_legacy_core_readers_reject_staged_and_published_managed_storage() {
    let reader =
        PathBuf::from(std::env::var_os("TGREP_V1_READER").expect(
            "managed qualification requires the pinned TGREP_V1_READER; see CONTRIBUTING.md",
        ));
    let legacy =
        PathBuf::from(std::env::var_os("TGREP_V1_BINARY").expect(
            "managed qualification requires the pinned TGREP_V1_BINARY; see CONTRIBUTING.md",
        ));
    assert_ne!(
        fs::canonicalize(&legacy).unwrap(),
        fs::canonicalize(assert_cmd::cargo::cargo_bin("tgrep")).unwrap()
    );
    let fixture = Fixture::new();
    let probe = |path: &Path, expected| {
        for kind in ["reader", "shared"] {
            let result = runtime::output(Command::new(&reader).arg(kind).arg(path));
            assert_eq!(
                result.status.code(),
                Some(expected),
                "{kind} {}: {result:?}",
                path.display()
            );
        }
    };
    success(runtime::output(
        Command::new(&legacy)
            .current_dir(&fixture.a)
            .args(["index", "."]),
    ));
    probe(&fixture.a.join(".tgrep"), 0);
    probe(&fixture.temp.path().join("absent"), 1);
    fs::write(fixture.a.join("late.txt"), "managed_compatibility_fresh\n").unwrap();
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    let hook = super::faults::pause(&daemon, "generation-built");
    let operation = daemon.rpc("views.attach", attach_input(&fixture, &claim));
    super::faults::reached(&daemon, &hook);
    let page = daemon.rpc("objects.page", json!({"cursor":null}));
    let stage = page["objects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|object| object["kind"] == "generation" && object["state"] == "preparing")
        .unwrap_or_else(|| panic!("fully built managed stage not present: {page}"));
    let directory = PathBuf::from(daemon.marker["storage"].as_str().unwrap())
        .join("objects")
        .join(stage["id"].as_str().unwrap());
    probe(&directory, 1);
    let renamed = fixture.temp.path().join("renamed-managed-sections");
    fs::create_dir(&renamed).unwrap();
    for (managed, old) in [
        ("paths.tgm", "files.bin"),
        ("lookup.tgm", "lookup.bin"),
        ("postings.tgm", "index.bin"),
        ("meta.tgm", "meta.json"),
    ] {
        fs::copy(directory.join(managed), renamed.join(old)).unwrap();
    }
    probe(&renamed, 1);
    daemon.rpc("testing.release", json!({"id":hook["ticket"]}));
    let attached = completed(&daemon, &operation);
    assert_eq!(
        attached["result"]["current"]["current"]["incarnation"],
        stage["id"]
    );
    probe(&directory, 1);
    let guarded = tgrep_core::managed::open_generation(
        Path::new(daemon.marker["storage"].as_str().unwrap()),
        &Id::parse(stage["id"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert!(
        guarded
            .base()
            .reader()
            .all_paths()
            .contains(&"notes.txt".to_owned())
    );
    drop(guarded);
    let old = runtime::output(Command::new(&legacy).current_dir(&fixture.a).args([
        "--json",
        "-F",
        "--",
        "managed_compatibility_fresh",
        ".",
    ]));
    let scan = cli(
        &fixture.a,
        &[
            "--no-index",
            "--json",
            "-F",
            "--",
            "managed_compatibility_fresh",
            ".",
        ],
    );
    assert_eq!(old.status.code(), Some(0), "{old:?}");
    assert_eq!(canonical_matches(old), canonical_matches(scan));
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
}

#[test]
fn independent_process_leases_survive_detach_and_only_proven_owner_death_reaps_abandonment() {
    let fixture = Fixture::new();
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (claim, mut child) = held_owner(&fixture, &daemon);
    let input = attach_input(&fixture, &claim);
    let accepted = manage(&fixture.a, "views.attach", input.clone());
    let first = completed(&daemon, &accepted);
    assert_eq!(
        manage(&fixture.a, "views.attach", input.clone())["id"],
        accepted["id"]
    );
    let view = &first["result"]["current"];
    let (sibling, sibling_guard) = owner(&daemon);
    let mut join = attach_input(&fixture, &sibling);
    join["request"]["revision"] = Value::Null;
    join["request"]["lease"] = json!("independent-sibling");
    join["request"]["accept_current"] = json!({"view":view["id"],"version":1});
    let second = completed(&daemon, &daemon.rpc("views.attach", join));
    assert_eq!(second["result"]["current"]["id"], view["id"]);
    assert_eq!(
        daemon.rpc("views.status", json!({"id":view["id"]}))["leases"],
        2
    );
    let detached = manage(
        &fixture.a,
        "views.detach",
        json!({"owner":claim.owner,"lease":"managed-client"}),
    );
    assert_eq!(detached["lease_released"], true);
    assert_eq!(detached["remaining_leases"], 1);
    assert_eq!(detached["root_handles_released"], false);
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());

    let mut abandoned = input;
    abandoned["token"] = token(&claim, 2);
    abandoned["request"]["revision"] = Value::Null;
    abandoned["request"]["lease"] = json!("abandoned-client");
    abandoned["request"]["accept_current"] = json!({"view":view["id"],"version":1});
    completed(&daemon, &manage(&fixture.a, "views.attach", abandoned));
    let alive = daemon
        .try_rpc("owners.reap", json!({"id":claim.owner}))
        .unwrap();
    assert_eq!(
        alive["error"]["data"]["reason_code"], "owner-still-alive",
        "{alive}"
    );
    assert_eq!(
        daemon.rpc("views.status", json!({"id":view["id"]}))["leases"],
        2
    );
    child.cancel();
    let ended = retry_busy_response(&daemon, "owners.reap", json!({"id":claim.owner}));
    if ended.get("error").is_some() {
        assert_eq!(
            ended["error"]["data"]["category"], "receipt-expired",
            "{ended}"
        );
    } else {
        assert_eq!(ended["result"]["data"]["owner"]["released"], true);
    }
    assert_eq!(
        daemon.rpc("views.status", json!({"id":view["id"]}))["leases"],
        1
    );
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
    daemon.rpc(
        "views.detach",
        json!({"owner":sibling.owner,"lease":"independent-sibling"}),
    );
    daemon.rpc("owners.release", json!({"claim":sibling}));
    drop(sibling_guard);
    stop(&mut daemon);
}

#[test]
#[ignore = "subprocess entry point; requires a caller-owned TGREP_MANAGED_READER journal"]
fn guarded_reader_process() {
    let journal =
        PathBuf::from(std::env::var_os("TGREP_MANAGED_READER").expect("owned reader journal"));
    let request: Value = serde_json::from_slice(&fs::read(journal).unwrap()).unwrap();
    let generation = tgrep_core::managed::open_generation(
        Path::new(request["namespace"].as_str().unwrap()),
        &Id::parse(request["incarnation"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    if request["candidate"] == true {
        let view = tgrep_core::worktrees::WorktreeView::new(
            Path::new(request["root"].as_str().unwrap()),
            Arc::clone(&generation),
            tgrep_core::worktrees::WorktreeOptions::default(),
        )
        .unwrap();
        view.reconcile_full().unwrap();
        let candidate = view
            .with_snapshot(|snapshot| snapshot.open_candidate("notes.txt").unwrap())
            .unwrap();
        let clone = candidate.try_clone().unwrap();
        drop((candidate, view, generation));
        println!("{}", json!({"ready":true,"kind":"escaped-candidate"}));
        std::io::stdout().flush().unwrap();
        let mut action = String::new();
        std::io::stdin().read_line(&mut action).unwrap();
        assert_eq!(action.trim(), "release");
        drop(clone);
    } else {
        let reader = Arc::clone(generation.base().reader());
        drop(generation);
        assert!(reader.all_paths().contains(&"notes.txt".to_string()));
        println!("{}", json!({"ready":true,"kind":"escaped-index-reader"}));
        std::io::stdout().flush().unwrap();
        let mut action = String::new();
        std::io::stdin().read_line(&mut action).unwrap();
        assert_eq!(action.trim(), "release");
        drop(reader);
    }
}

#[test]
#[ignore = "subprocess entry point; requires a caller-owned TGREP_MANAGED_NAMESPACE path"]
fn namespace_owner_process() {
    let path = PathBuf::from(std::env::var_os("TGREP_MANAGED_NAMESPACE").expect("owned namespace"));
    let namespace = tgrep_core::managed::Namespace::open(&path).unwrap();
    println!(
        "{}",
        json!({"ready":true,"namespace":namespace.header().namespace})
    );
    std::io::stdout().flush().unwrap();
    let mut action = String::new();
    std::io::stdin().read_line(&mut action).unwrap();
    assert_eq!(action.trim(), "release");
    drop(namespace);
}

#[test]
fn a_native_namespace_owner_excludes_startup_and_other_maintenance_until_release() {
    let fixture = Fixture::new();
    let configured = policy();
    let mut daemon = start(&fixture, &configured, &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    let original = completed(
        &daemon,
        &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
    );
    let namespace = PathBuf::from(daemon.marker["storage"].as_str().unwrap());
    let namespace_id = daemon.marker["namespace"].clone();
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
    drop(daemon);

    let mut holder = runtime::Process::start(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "managed::clients::namespace_owner_process",
                "--ignored",
                "--nocapture",
            ])
            .env("TGREP_MANAGED_NAMESPACE", &namespace),
    );
    assert_eq!(wait_json(&mut holder, "ready")["namespace"], namespace_id);
    let policy_path = fixture.storage.join("policy.json");
    let mut starter =
        runtime::Process::start(Command::new(assert_cmd::cargo::cargo_bin("tgrep")).args([
            "serve",
            "--shared",
            fixture.a.to_str().unwrap(),
            "--shared-storage",
            fixture.storage.to_str().unwrap(),
            "--shared-policy",
            policy_path.to_str().unwrap(),
            "--no-watch",
        ]));
    let mut inspector =
        runtime::Process::start(Command::new(assert_cmd::cargo::cargo_bin("tgrep")).args([
            "shared",
            "maintenance",
            namespace.to_str().unwrap(),
            "namespace.status",
        ]));
    let request = json!({"token":{"scope":namespace_id,"sequence":1,"token":"competing-maintainer"},"request":{"cursor":null}});
    let mut maintainer =
        runtime::Process::start(Command::new(assert_cmd::cargo::cargo_bin("tgrep")).args([
            "shared",
            "maintenance",
            namespace.to_str().unwrap(),
            "maintenance.recover",
            "--params",
            &request.to_string(),
            "--apply",
        ]));
    let startup = starter.finish(Duration::from_secs(10)).unwrap();
    assert!(!startup.status.success());
    assert!(
        String::from_utf8_lossy(&startup.stderr).contains("namespace-owned"),
        "{startup:?}"
    );
    for process in [&mut inspector, &mut maintainer] {
        let output = process.finish(Duration::from_secs(10)).unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["error"]["category"], "busy", "{error}");
        assert_eq!(error["error"]["reason_code"], "namespace-owned", "{error}");
        assert_eq!(
            error["error"]["committed_state"], "not-committed",
            "{error}"
        );
    }
    assert!(holder.running());
    holder.send("release");
    assert!(
        holder
            .finish(Duration::from_secs(10))
            .unwrap()
            .status
            .success()
    );
    let mut restarted = start(&fixture, &configured, &["--no-watch"]);
    let (claim, guard) = owner(&restarted);
    let current = completed(
        &restarted,
        &restarted.rpc("views.attach", attach_input(&fixture, &claim)),
    );
    assert_eq!(
        current["result"]["current"]["current"],
        original["result"]["current"]["current"]
    );
    assert_scan_parity(&restarted, &fixture.a, fixture.temp.path());
    restarted.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut restarted);
}

fn reader_process(
    fixture: &Fixture,
    daemon: &Daemon,
    incarnation: &Value,
    candidate: bool,
) -> runtime::Process {
    let path = fixture.temp.path().join("reader.json");
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "namespace":daemon.marker["storage"],"incarnation":incarnation,"candidate":candidate,
            "root":fs::canonicalize(&fixture.a).unwrap(),
        }))
        .unwrap(),
    )
    .unwrap();
    let mut child = runtime::Process::start(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "managed::clients::guarded_reader_process",
                "--ignored",
                "--nocapture",
            ])
            .env("TGREP_MANAGED_READER", path),
    );
    assert_eq!(
        wait_json(&mut child, "ready")["kind"],
        if candidate {
            "escaped-candidate"
        } else {
            "escaped-index-reader"
        }
    );
    child
}

fn collect_all(daemon: &Daemon, claim: &OwnerClaim, sequence: &mut u64) {
    let mut cursor = Value::Null;
    for _ in 0..32 {
        *sequence += 1;
        let request = json!({"token":token(claim,*sequence),"request":{
            "policy_version":1,"allocation_version":1,"cursor":cursor,
            "bounds":{"max_duration_ms":1000,"max_examined":64,"max_removed":16,
                "max_delete_bytes":1048576,"max_pages":64}
        }});
        let done = completed(daemon, &daemon.rpc("collections.start", request));
        assert_eq!(done["result"]["errors"], 0, "{done}");
        cursor = done["result"]["next"].clone();
        if cursor.is_null() {
            return;
        }
    }
    panic!("bounded collection did not finish its traversal");
}

fn removed(daemon: &Daemon, incarnation: &Value) -> bool {
    let response = daemon
        .try_rpc("objects.inspect", json!({"id":incarnation}))
        .unwrap();
    if response.get("error").is_some() {
        assert_eq!(
            response["error"]["data"]["category"], "cache-evicted",
            "{response}"
        );
        true
    } else {
        response["result"]["data"]["state"] == "removed"
    }
}

fn idle_status(daemon: &Daemon) -> Value {
    retry_busy(daemon, "stop-if-idle", idle_request(daemon))
}

#[test]
fn escaped_cross_process_readers_block_collection_and_idle_stop_then_allow_exact_rebuild() {
    for candidate in [false, true] {
        let fixture = Fixture::new();
        let mut configured = policy();
        configured["retention"] = json!({"mode":"bounded","target_bytes":1});
        configured["collection"]["max_pages"] = json!(64);
        let mut daemon = start(&fixture, &configured, &["--no-watch"]);
        let (claim, guard) = owner(&daemon);
        completed(
            &daemon,
            &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
        );
        let before = daemon.rpc(
            "lookup",
            json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
        );
        assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
        let mut reader = reader_process(&fixture, &daemon, &before["incarnation"], candidate);
        let detached = daemon.rpc(
            "views.detach",
            json!({"owner":claim.owner,"lease":"managed-client"}),
        );
        assert_eq!(detached["lease_released"], true);
        assert_eq!(detached["root_handles_released"], !candidate);
        let mut sequence = 1;
        collect_all(&daemon, &claim, &mut sequence);
        assert!(!removed(&daemon, &before["incarnation"]));
        assert!(
            daemon
                .rpc("objects.references", json!({"id":before["incarnation"]}))
                .as_array()
                .unwrap()
                .is_empty()
        );
        let eligibility = daemon.rpc("objects.eligibility", json!({"id":before["incarnation"]}));
        assert_eq!(eligibility["eligible"], false);
        assert!(
            eligibility["reasons"]
                .as_array()
                .unwrap()
                .contains(&json!("object-readers-active"))
        );
        daemon.rpc("owners.release", json!({"claim":claim}));
        drop(guard);
        let idle = idle_status(&daemon);
        assert_eq!(idle["stopping"], false);
        assert_eq!(idle["namespace_readers_or_work"], true);
        assert_eq!(idle["leases"], 0);
        reader.send("release");
        success(reader.finish(Duration::from_secs(30)).unwrap());

        let (claim, guard) = owner(&daemon);
        let mut sequence = 0;
        collect_all(&daemon, &claim, &mut sequence);
        assert!(removed(&daemon, &before["incarnation"]));
        fs::write(
            fixture.a.join("notes.txt"),
            "shared_term newer than the evicted exact pin\n",
        )
        .unwrap();
        git(&fixture.a, &["commit", "-qam", "newer checkout"]);
        sequence += 1;
        let mut input = attach_input(&fixture, &claim);
        input["token"] = token(&claim, sequence);
        completed(&daemon, &daemon.rpc("views.attach", input));
        let rebuilt = daemon.rpc(
            "lookup",
            json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
        );
        assert_eq!(rebuilt["commit"], fixture.revision);
        assert_ne!(rebuilt["incarnation"], before["incarnation"]);
        assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
        daemon.rpc(
            "views.detach",
            json!({"owner":claim.owner,"lease":"managed-client"}),
        );
        daemon.rpc("owners.release", json!({"claim":claim}));
        drop(guard);
        stop(&mut daemon);
    }
}

#[test]
fn owner_holder_graceful_eof_releases_its_os_proof_without_guessing_process_identity() {
    let fixture = Fixture::new();
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (claim, mut child) = held_owner(&fixture, &daemon);
    let alive = daemon
        .try_rpc("owners.reap", json!({"id":claim.owner}))
        .unwrap();
    assert_eq!(alive["error"]["data"]["reason_code"], "owner-still-alive");
    child.close_input();
    success(child.finish(Duration::from_secs(30)).unwrap());
    let ended = retry_busy_response(&daemon, "owners.reap", json!({"id":claim.owner}));
    if ended.get("error").is_some() {
        assert_eq!(
            ended["error"]["data"]["category"], "receipt-expired",
            "{ended}"
        );
    } else {
        assert_eq!(ended["result"]["data"]["owner"]["released"], true);
    }
    stop(&mut daemon);
}

#[test]
fn a_missing_evicted_exact_commit_never_substitutes_a_newer_head_and_scans_explicitly() {
    let fixture = Fixture::new();
    let mut configured = policy();
    configured["retention"] = json!({"mode":"bounded","target_bytes":1});
    configured["collection"]["max_pages"] = json!(64);
    let mut daemon = start(&fixture, &configured, &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    completed(
        &daemon,
        &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
    );
    let original = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    daemon.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"managed-client"}),
    );
    let mut sequence = 1;
    collect_all(&daemon, &claim, &mut sequence);
    assert!(removed(&daemon, &original["incarnation"]));
    fs::write(
        fixture.a.join("notes.txt"),
        "shared_term newer valid worktree\n",
    )
    .unwrap();
    git(&fixture.a, &["commit", "-qam", "newer exact target"]);
    let newer = git(&fixture.a, &["rev-parse", "HEAD"]);
    let repository = tgrep_core::generations::Repository::discover(&fixture.a).unwrap();
    let object = repository
        .common_dir()
        .join("objects")
        .join(&fixture.revision[..2])
        .join(&fixture.revision[2..]);
    assert!(
        object.is_file(),
        "the owned fixture must have a loose initial commit"
    );
    #[cfg(windows)]
    #[allow(
        clippy::permissions_set_readonly_false,
        reason = "Windows-only removal of an owned fixture's read-only Git object."
    )]
    {
        let mut permissions = fs::metadata(&object).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(&object, permissions).unwrap();
    }
    fs::remove_file(object).unwrap();
    let mut request = attach_input(&fixture, &claim);
    sequence += 1;
    request["token"] = token(&claim, sequence);
    request["request"]["lease"] = json!("unavailable-target");
    let failed = terminal(&daemon, &manage(&fixture.a, "views.attach", request));
    assert_eq!(
        failed["error"]["category"], "git-objects-unavailable",
        "{failed}"
    );
    assert_eq!(failed["committed_state"], "not-committed");
    let scan = cli(&fixture.a, &["--json", "-F", "--", "shared_term", "."]);
    assert!(
        String::from_utf8_lossy(&scan.stderr).contains("scanning"),
        "{scan:?}"
    );
    let forced = cli(
        &fixture.a,
        &["--no-index", "--json", "-F", "--", "shared_term", "."],
    );
    let rows = |output: Output| {
        let mut rows: Vec<_> = success(output)
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["type"] == "match")
            .map(|row| row["data"].clone())
            .collect();
        rows.sort_by_key(Value::to_string);
        rows
    };
    assert_eq!(rows(scan), rows(forced));
    sequence += 1;
    let mut request = attach_input(&fixture, &claim);
    request["token"] = token(&claim, sequence);
    request["request"]["revision"] = json!(newer);
    request["request"]["lease"] = json!("available-target");
    completed(&daemon, &manage(&fixture.a, "views.attach", request));
    let available = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    assert_eq!(available["commit"], newer);
    assert_ne!(available["incarnation"], original["incarnation"]);
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
    daemon.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"available-target"}),
    );
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
}
