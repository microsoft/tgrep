// Copyright (c) Microsoft Corporation. All rights reserved.

use super::*;

fn native_ready(daemon: &Daemon, view: &Value, after: Option<u64>) -> Value {
    let started = Instant::now();
    let mut quiet = None;
    loop {
        let status = daemon.rpc("views.status", json!({"id":view["view"]}));
        assert_eq!(status["watch_mode"], "native", "{status}");
        assert!(status["watch_fallback"].is_null(), "{status}");
        if status["ready"] == true
            && status["work"].is_null()
            && after.is_none_or(|epoch| status["input_epoch"].as_u64().unwrap() > epoch)
        {
            let current = epoch(&status);
            let (last_epoch, since) = quiet.get_or_insert((current, Instant::now()));
            if *last_epoch != current {
                *last_epoch = current;
                *since = Instant::now();
            }
            assert_eq!(
                status["reconciliation_policy"]["native_watch_budget_per_view"],
                256
            );
            assert_eq!(
                status["reconciliation_policy"]["periodic_full_verification"],
                true
            );
            assert_eq!(
                status["reconciliation_policy"]["full_verification_reason"],
                "periodic-native-verification"
            );
            assert_eq!(status["reconciliation_policy"]["poll_interval_ms"], 120_000);
            if since.elapsed() >= Duration::from_millis(500) {
                return status;
            }
        } else {
            quiet = None;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "native event did not settle: {status}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn epoch(status: &Value) -> u64 {
    status["input_epoch"].as_u64().unwrap()
}

/// Initial attach may end in the documented retryable, not-committed preparation
/// invalidation; only that exact result is resolved and resumed within `deadline`.
pub(super) fn attach_watched_root<E: super::attach_retry::AttachEndpoint>(
    endpoint: &E,
    claim: &OwnerClaim,
    input: Value,
    deadline: Instant,
) -> Value {
    super::attach_retry::retry_attach(
        endpoint,
        claim,
        None,
        input,
        super::attach_retry::Retry {
            point: "native-watch-attach",
            deadline,
            allowance: super::attach_retry::RetryableAttach::PreparationInvalidated,
        },
        |_| {},
    )
    .unwrap_or_else(|failures| {
        panic!(
            "native-watch-attach: exhausted the shared attach bound after {} documented preparation invalidations: {failures:?}",
            failures.len()
        )
    })
}

#[test]
#[cfg(feature = "managed-test-hooks")]
fn watcher_attach_recovers_real_preparation_invalidation_with_one_lease() {
    let fixture = Fixture::new();
    let mut configured = policy();
    configured["work"]["max_views"] = json!(32);
    configured["work"]["workers"] = json!(1);
    let mut daemon = start(&fixture, &configured, &[]);
    let (claim, guard) = owner(&daemon);
    let mut input = attach_input(&fixture, &claim);
    input["request"]["root"] = json!(fs::canonicalize(&fixture.a).unwrap());
    let hook = daemon.rpc(
        "testing.install",
        json!({
            "point":"preparation-checkpoint-ready",
            "operation":null,
            "skip_hits":0,
            "action":{"mode":"invalidate","remaining":3}
        }),
    );
    let started = Instant::now();
    let attached = attach_watched_root(
        &daemon,
        &claim,
        input.clone(),
        started + Duration::from_secs(30),
    );
    assert!(started.elapsed() < Duration::from_secs(30));

    let failed = daemon.rpc("operations.lookup", input["token"].clone());
    assert_eq!(failed["kind"], "attach");
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["cancelled"], false);
    assert_eq!(failed["committed_state"], "not-committed");
    assert_eq!(failed["error"]["category"], "busy");
    assert_eq!(
        failed["error"]["reason_code"],
        "view-preparation-invalidated"
    );
    assert_eq!(failed["error"]["retryable"], true);
    assert_eq!(failed["error"]["committed_state"], "not-committed");
    assert_eq!(
        failed["progress"]["view"],
        attached["result"]["current"]["id"]
    );
    assert_eq!(failed["request"], input["request"]);

    let status = daemon.rpc("testing.status", json!({}));
    assert_eq!(status["ticket"], hook["ticket"]);
    assert_eq!(status["stage"], "fired");
    assert_eq!(status["hits"], 3);
    assert_eq!(status["reached_operation"], failed["id"]);
    assert_eq!(attached["token"], token(&claim, 2));
    assert_ne!(attached["id"], failed["id"]);
    assert_eq!(attached["state"], "completed");
    assert_eq!(attached["result"]["lease"]["operation"], failed["id"]);

    let view = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    assert_eq!(view["view"], failed["progress"]["view"]);
    let ready = native_ready(&daemon, &view, None);
    assert_eq!(ready["leases"], 1);
    assert!(!matches(&daemon, &view, "shared_term").is_empty());
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());

    daemon.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"managed-client"}),
    );
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
    drop(daemon);
    fixture.temp.close().unwrap();
}

fn matches(daemon: &Daemon, view: &Value, pattern: &str) -> Vec<Value> {
    let result = retry_busy(
        daemon,
        "search",
        json!({"root":view["root"],"view":view["view"],"expected_version":view["version"],
            "query":{"pattern":pattern}}),
    );
    assert_eq!(result["backend"], "shared-v2", "{result}");
    assert_eq!(result["ready"], true, "{result}");
    result["matches"].as_array().unwrap().clone()
}

#[test]
#[cfg(feature = "managed-test-hooks")]
fn lagging_view_serves_its_last_publication_then_converges() {
    for files in [true, false] {
        let fixture = Fixture::new();
        let pattern = "native_ready_snapshot_only";
        fs::create_dir(fixture.b.join("ignored")).unwrap();
        fs::write(
            fixture.b.join("ignored").join("marker.txt"),
            format!("shared_term {pattern}\n"),
        )
        .unwrap();
        let mut configured = policy();
        configured["work"]["max_views"] = json!(32);
        configured["work"]["workers"] = json!(1);
        let mut daemon = start(&fixture, &configured, &[]);
        let (claim, guard) = owner(&daemon);
        let mut input = attach_input(&fixture, &claim);
        input["request"]["root"] = json!(fs::canonicalize(&fixture.b).unwrap());
        verified_attach(&daemon, &daemon.rpc("views.attach", input));
        let view = daemon.rpc(
            "lookup",
            json!({"root":fs::canonicalize(&fixture.b).unwrap()}),
        );
        let ready = native_ready(&daemon, &view, None);
        assert_eq!(matches(&daemon, &view, pattern).len(), 1);
        assert_scan_parity(&daemon, &fixture.b, fixture.temp.path());

        let (_, common) = tgrep_core::git_index::read_repository_dirs(&fixture.b).unwrap();
        let hook = faults::pause(&daemon, "view-before-commit");
        let args = if files {
            vec!["--files", "--glob", "!**/.git/**", "."]
        } else {
            vec!["--stats", "--json", "-F", "--", pattern, "."]
        };
        let run = || {
            runtime::output(
                Command::new(assert_cmd::cargo::cargo_bin("tgrep"))
                    .current_dir(&fixture.b)
                    .env("HOME", fixture.temp.path())
                    .env("USERPROFILE", fixture.temp.path())
                    .env("XDG_CONFIG_HOME", fixture.temp.path())
                    .args(&args),
            )
        };
        // Deliver a real ignore event after the caller's ready snapshot.
        fs::write(common.join("info").join("exclude"), "ignored/\n").unwrap();
        faults::reached(&daemon, &hook);
        let pending = daemon.rpc("views.status", json!({"id":view["view"]}));
        assert_eq!(pending["ready"], false, "{pending}");
        assert_eq!(pending["serving"], true, "{pending}");
        assert_eq!(pending["watch_mode"], "native", "{pending}");
        assert!(pending["watch_fallback"].is_null(), "{pending}");
        assert!(epoch(&pending) > epoch(&ready), "{pending}");
        // Eventual consistency: a lagging view keeps answering from its last
        // publication instead of sending the client to a filesystem scan.
        let lagging = run();
        let stderr = String::from_utf8_lossy(&lagging.stderr);
        assert!(matches!(lagging.status.code(), Some(0 | 1)), "{lagging:?}");
        assert!(!stderr.contains("scanning filesystem"), "{lagging:?}");
        assert!(!stderr.contains("view-input-pending"), "{lagging:?}");
        // The paused pass has already swapped its reconciled state in memory;
        // only its durable commit waits, so either membership may be served.
        if files {
            assert!(lagging.stderr.is_empty(), "{lagging:?}");
        } else {
            assert!(stderr.contains("(via shared daemon v2)"), "{lagging:?}");
        }
        daemon.rpc("testing.release", json!({"id":hook["ticket"]}));
        native_ready(&daemon, &view, Some(epoch(&ready)));
        let indexed = managed_cli(&daemon, &view, run);
        if files {
            assert!(indexed.stderr.is_empty(), "{indexed:?}");
            let paths = success(indexed);
            assert!(!paths.contains("marker.txt"), "{paths}");
        } else {
            assert!(
                String::from_utf8_lossy(&indexed.stderr).contains("(via shared daemon v2)"),
                "{indexed:?}"
            );
            assert_eq!(indexed.status.code(), Some(1), "{indexed:?}");
            let rows: Vec<Value> = std::str::from_utf8(&indexed.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert!(rows.iter().all(|row| row["type"] != "match"), "{rows:?}");
            let summary = rows.iter().find(|row| row["type"] == "summary").unwrap();
            assert_eq!(summary["data"]["stats"]["matches"], 0, "{summary}");
        }
        assert!(matches(&daemon, &view, pattern).is_empty());
        assert_scan_parity(&daemon, &fixture.b, fixture.temp.path());
        native_ready(&daemon, &view, Some(epoch(&ready)));
        daemon.rpc(
            "views.detach",
            json!({"owner":claim.owner,"lease":"managed-client"}),
        );
        daemon.rpc("owners.release", json!({"claim":claim}));
        drop(guard);
        stop(&mut daemon);
        drop(daemon);
        fixture.temp.close().unwrap();
    }
}

#[test]
fn native_subscriptions_cover_nested_worktree_changes_with_default_budgets() {
    let mut fixture = Fixture::new();
    let recursive = cfg!(any(windows, target_os = "macos"));
    let branches = if recursive { 320 } else { 4 };
    for index in 0..branches {
        let directory = fixture
            .a
            .join("tree")
            .join(format!("branch-{index}"))
            .join("one")
            .join("two")
            .join("three");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("source.txt"), "shared_term original\n").unwrap();
    }
    git(&fixture.a, &["add", "tree"]);
    git(&fixture.a, &["commit", "-qm", "nested native fixture"]);
    fixture.revision = git(&fixture.a, &["rev-parse", "HEAD"]);
    git(&fixture.b, &["checkout", "--detach", &fixture.revision]);
    let third = fixture.third(&fixture.revision);
    let roots = [&fixture.a, &fixture.b, &third];
    let mut configured = policy();
    configured["work"]["max_views"] = json!(32);
    configured["work"]["workers"] = json!(1);
    configured["work"]["private_work_bytes"] = json!(8_u64 * 1024 * 1024 * 1024);
    configured["work"]["staging_bytes"] = json!(8_u64 * 1024 * 1024 * 1024);
    let mut daemon = start(&fixture, &configured, &[]);
    let mut owners = Vec::new();
    let mut views: Vec<Value> = Vec::new();
    let mut statuses = Vec::new();
    let mut attach_ms = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        let (claim, guard) = owner(&daemon);
        let mut input = attach_input(&fixture, &claim);
        input["request"]["root"] = json!(fs::canonicalize(root).unwrap());
        let started = Instant::now();
        attach_watched_root(&daemon, &claim, input, started + Duration::from_secs(30));
        let view = daemon.rpc("lookup", json!({"root":fs::canonicalize(root).unwrap()}));
        let status = native_ready(&daemon, &view, None);
        attach_ms.push(started.elapsed().as_millis());
        if recursive {
            assert_eq!(status["watch_count"], if index == 0 { 1 } else { 2 });
            assert_eq!(
                status["reconciliation_policy"]["native_registration_mode"],
                "recursive-roots"
            );
        } else {
            assert!(status["watch_count"].as_u64().unwrap() <= 256);
            assert_eq!(
                status["reconciliation_policy"]["native_registration_mode"],
                "per-directory"
            );
        }
        if let Some(first) = views.first() {
            assert_eq!(view["generation"], first["generation"]);
            assert_eq!(view["incarnation"], first["incarnation"]);
        }
        assert_scan_parity(&daemon, root, fixture.temp.path());
        owners.push((claim, guard));
        views.push(view);
        statuses.push(status);
    }
    let resources = daemon.rpc("namespace.status", json!({}));
    assert_eq!(resources["reconciliation_policy"]["native_views"], 3);
    assert_eq!(resources["reconciliation_policy"]["polling_views"], 0);
    assert_eq!(
        resources["reconciliation_policy"]["watch_count"],
        statuses
            .iter()
            .map(|status| status["watch_count"].as_u64().unwrap())
            .sum::<u64>()
    );
    eprintln!(
        "native fixture: {}",
        json!({"branches":branches,"directories_per_tree":1 + branches * 4,
            "worktrees":3,"attach_ms":attach_ms,
            "watch_counts":statuses.iter().map(|status| &status["watch_count"]).collect::<Vec<_>>()})
    );

    let view = &views[1];
    let root = &fixture.b;
    let nested = root.join("new").join("nested").join("leaf");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("event.txt"), "shared_term nested_created\n").unwrap();
    let created = native_ready(&daemon, view, Some(epoch(&statuses[1])));
    assert_eq!(matches(&daemon, view, "nested_created").len(), 1);

    // A later file-only event proves coverage survived directory creation.
    fs::write(nested.join("event.txt"), "shared_term nested_updated\n").unwrap();
    let updated = native_ready(&daemon, view, Some(epoch(&created)));
    assert_eq!(matches(&daemon, view, "nested_updated").len(), 1);
    assert!(matches(&daemon, view, "nested_created").is_empty());
    assert!(matches(&daemon, &views[0], "nested_updated").is_empty());
    assert!(matches(&daemon, &views[2], "nested_updated").is_empty());
    assert_scan_parity(&daemon, root, fixture.temp.path());

    let renamed = root.join("renamed");
    fs::rename(root.join("new"), &renamed).unwrap();
    let moved = native_ready(&daemon, view, Some(epoch(&updated)));
    assert_eq!(matches(&daemon, view, "nested_updated").len(), 1);
    assert_scan_parity(&daemon, root, fixture.temp.path());
    fs::remove_dir_all(&renamed).unwrap();
    let removed = native_ready(&daemon, view, Some(epoch(&moved)));
    assert!(matches(&daemon, view, "nested_updated").is_empty());
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("event.txt"), "shared_term recreated\n").unwrap();
    let recreated = native_ready(&daemon, view, Some(epoch(&removed)));
    assert_eq!(matches(&daemon, view, "recreated").len(), 1);
    fs::write(nested.join("event.txt"), "shared_term recreated_later\n").unwrap();
    let later = native_ready(&daemon, view, Some(epoch(&recreated)));
    assert_eq!(matches(&daemon, view, "recreated_later").len(), 1);

    fs::write(root.join(".gitignore"), "new/\n").unwrap();
    let ignored = native_ready(&daemon, view, Some(epoch(&later)));
    assert_eq!(ignored["last_reconcile"]["full"], true);
    assert!(matches(&daemon, view, "recreated_later").is_empty());
    assert_scan_parity(&daemon, root, fixture.temp.path());
    fs::remove_file(root.join(".gitignore")).unwrap();
    let unignored = native_ready(&daemon, view, Some(epoch(&ignored)));
    assert_eq!(unignored["last_reconcile"]["full"], true);
    assert_eq!(matches(&daemon, view, "recreated_later").len(), 1);

    let (private, common) = tgrep_core::git_index::read_repository_dirs(root).unwrap();
    fs::write(
        private.join("config.worktree"),
        "[nativeFixture]\nvalue = true\n",
    )
    .unwrap();
    let private_change = native_ready(&daemon, view, Some(epoch(&unignored)));
    assert_eq!(private_change["last_reconcile"]["full"], true);
    statuses = views
        .iter()
        .map(|view| native_ready(&daemon, view, None))
        .collect();
    git(
        &fixture.a,
        &[
            "update-ref",
            "refs/heads/native/nested/branch",
            &fixture.revision,
        ],
    );
    for (view, before) in views.iter().zip(&mut statuses) {
        *before = native_ready(&daemon, view, Some(epoch(before)));
        assert_eq!(before["last_reconcile"]["full"], true);
    }
    fs::write(common.join("info").join("exclude"), "new/\n").unwrap();
    for ((view, before), root) in views.iter().zip(&statuses).zip(roots) {
        let after = native_ready(&daemon, view, Some(epoch(before)));
        assert_eq!(after["last_reconcile"]["full"], true);
        assert!(matches(&daemon, view, "recreated_later").is_empty());
        assert_scan_parity(&daemon, root, fixture.temp.path());
    }

    daemon.rpc(
        "views.detach",
        json!({"owner":owners[1].0.owner,"lease":"managed-client"}),
    );
    daemon.rpc("owners.release", json!({"claim":owners[1].0}));
    let remaining = daemon.rpc("namespace.status", json!({}));
    assert_eq!(remaining["reconciliation_policy"]["native_views"], 2);
    if recursive {
        assert_eq!(remaining["reconciliation_policy"]["watch_count"], 3);
    }
    fs::remove_dir_all(&fixture.b).unwrap();
    assert!(!fixture.b.exists());
    for index in [0, 2] {
        native_ready(&daemon, &views[index], None);
        daemon.rpc(
            "views.detach",
            json!({"owner":owners[index].0.owner,"lease":"managed-client"}),
        );
        daemon.rpc("owners.release", json!({"claim":owners[index].0}));
    }
    drop(owners);
    let drained = daemon.rpc("namespace.status", json!({}));
    assert_eq!(drained["reconciliation_policy"]["native_views"], 0);
    assert_eq!(drained["reconciliation_policy"]["polling_views"], 0);
    assert_eq!(drained["reconciliation_policy"]["watch_count"], 0);
    stop(&mut daemon);
    assert!(daemon.child.wait().unwrap().success());
    drop(daemon);
    fixture.temp.close().unwrap();
}

#[test]
#[cfg(any(windows, target_os = "macos"))]
fn recursive_root_budget_fallback_is_per_view_and_releases_subscriptions() {
    let fixture = Fixture::new();
    let mut configured = policy();
    configured["work"]["max_views"] = json!(32);
    let mut daemon = start(&fixture, &configured, &["--watch-budget", "32"]);
    let (claim, guard) = owner(&daemon);
    let mut views = Vec::new();
    for (index, root) in [&fixture.a, &fixture.b].into_iter().enumerate() {
        let mut input = attach_input(&fixture, &claim);
        input["token"] = token(&claim, index as u64 + 1);
        input["request"]["root"] = json!(fs::canonicalize(root).unwrap());
        input["request"]["lease"] = json!(format!("budget-client-{index}"));
        verified_attach(&daemon, &daemon.rpc("views.attach", input));
        views.push(daemon.rpc("lookup", json!({"root":fs::canonicalize(root).unwrap()})));
    }
    let native = daemon.rpc("views.status", json!({"id":views[0]["view"]}));
    let fallback = daemon.rpc("views.status", json!({"id":views[1]["view"]}));
    assert_eq!(native["watch_mode"], "native", "{native}");
    assert!(native["watch_fallback"].is_null());
    assert_eq!(native["watch_count"], 1);
    assert_eq!(fallback["watch_mode"], "poll", "{fallback}");
    assert_eq!(fallback["watch_count"], 0);
    assert_eq!(
        fallback["watch_fallback"],
        "native watch count budget; polling"
    );
    assert_eq!(fallback["reconciliation_policy"]["interval_ms"], 120_000);
    assert_eq!(
        fallback["reconciliation_policy"]["full_verification_reason"],
        "polling-update-mechanism"
    );
    let resources = daemon.rpc("namespace.status", json!({}));
    assert_eq!(resources["reconciliation_policy"]["native_views"], 1);
    assert_eq!(resources["reconciliation_policy"]["polling_views"], 1);
    assert_eq!(resources["reconciliation_policy"]["watch_count"], 1);
    for index in 0..2 {
        daemon.rpc(
            "views.detach",
            json!({"owner":claim.owner,"lease":format!("budget-client-{index}")}),
        );
    }
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    let drained = daemon.rpc("namespace.status", json!({}));
    assert_eq!(drained["reconciliation_policy"]["native_views"], 0);
    assert_eq!(drained["reconciliation_policy"]["polling_views"], 0);
    assert_eq!(drained["reconciliation_policy"]["watch_count"], 0);
    stop(&mut daemon);
    drop(daemon);
    fixture.temp.close().unwrap();
}
