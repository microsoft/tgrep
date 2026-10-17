// Copyright (c) Microsoft Corporation. All rights reserved.

use super::*;
use tgrep_core::managed::{CommitState, ErrorCategory, OwnerClaim, OwnerGuard};

#[path = "managed_diagnostics.rs"]
mod diagnostics;

#[path = "managed_watch.rs"]
mod watch;

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

#[test]
fn startup_deprecation_is_absent_for_managed_v2() {
    for storage in ["managed", "compatibility-retain-all"] {
        let fixture = Fixture::new();
        let mut policy = policy();
        policy["storage"] = json!(storage);
        let mut daemon = start(&fixture, &policy, &["--no-watch"]);
        assert_eq!(daemon.marker["protocol"], 2);
        let output = cli(&fixture.a, &["shared", "manage", ".", "hello"]);
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let hello: Value = serde_json::from_str(&success(output)).unwrap();
        assert_eq!(hello["ok"], true);
        assert_eq!(hello["result"]["storage"], storage);
        assert_eq!(daemon.rpc("hello", json!({}))["storage"], storage);

        stop(&mut daemon);
        assert!(daemon.child.wait().unwrap().success());
        let stderr = fs::read_to_string(&daemon.log).unwrap();
        assert!(!stderr.contains(V1_DEPRECATION), "{stderr}");
        assert!(fs::read(&daemon.stdout_log).unwrap().is_empty());
        drop(daemon);
        fixture.temp.close().unwrap();
    }
}

fn owner(daemon: &Daemon) -> (OwnerClaim, OwnerGuard) {
    let prepared = daemon.rpc(
        "owners.prepare",
        json!({"token":format!("bootstrap-{}",LEASE_SEQUENCE.fetch_add(1,Ordering::Relaxed))}),
    );
    let claim: OwnerClaim = serde_json::from_value(prepared["claim"].clone()).unwrap();
    let guard = retry_owner_claim(
        || OwnerGuard::claim(claim.clone()),
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap();
    daemon.rpc("owners.register", json!({"claim":claim}));
    (claim, guard)
}

fn retry_owner_claim(
    mut claim: impl FnMut() -> tgrep_core::managed::Result<OwnerGuard>,
    deadline: Instant,
) -> tgrep_core::managed::Result<OwnerGuard> {
    loop {
        match claim() {
            Err(error)
                if error.category == ErrorCategory::Busy
                    && error.reason_code == "catalog-io"
                    && error.retryable
                    && error.committed_state == CommitState::NotCommitted
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

#[test]
fn owner_fixture_retries_catalog_contention_without_replacing_the_claim() {
    let fixture = Fixture::new();
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let prepared = daemon.rpc("owners.prepare", json!({"token":"contended-owner"}));
    let claim: OwnerClaim = serde_json::from_value(prepared["claim"].clone()).unwrap();
    let guard = thread::scope(|scope| {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let catalog = claim.storage.join("catalog.sqlite");
        let holder = scope.spawn(move || {
            let mut connection = rusqlite::Connection::open(catalog).unwrap();
            connection.busy_timeout(Duration::from_secs(10)).unwrap();
            let transaction = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            transaction.rollback().unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let mut attempts = 0;
        let guard = retry_owner_claim(
            || {
                attempts += 1;
                let result = OwnerGuard::claim(claim.clone());
                if attempts == 1 {
                    let error = result
                        .as_ref()
                        .err()
                        .expect("writer lock must block the claim");
                    assert_eq!(error.category, ErrorCategory::Busy);
                    assert_eq!(error.reason_code, "catalog-io");
                    assert!(error.retryable);
                    assert_eq!(error.committed_state, CommitState::NotCommitted);
                    release_tx.send(()).unwrap();
                }
                result
            },
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        holder.join().unwrap();
        assert!(attempts >= 2);
        guard
    });
    assert_eq!(
        serde_json::to_value(guard.registration()).unwrap(),
        prepared["claim"]
    );
    daemon.rpc("owners.register", json!({"claim":claim}));
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
    drop(daemon);
    fixture.temp.close().unwrap();
}

#[test]
fn owner_fixture_does_not_retry_unsafe_or_expired_claim_failures() {
    for (category, reason, retryable, committed, expired) in [
        (
            ErrorCategory::Busy,
            "catalog-io",
            true,
            CommitState::NotCommitted,
            true,
        ),
        (
            ErrorCategory::Busy,
            "catalog-io",
            false,
            CommitState::NotCommitted,
            false,
        ),
        (
            ErrorCategory::Busy,
            "catalog-io",
            true,
            CommitState::Committed,
            false,
        ),
        (
            ErrorCategory::Busy,
            "catalog-io",
            true,
            CommitState::Unknown,
            false,
        ),
        (
            ErrorCategory::Busy,
            "owner-guard-already-held",
            true,
            CommitState::NotCommitted,
            false,
        ),
        (
            ErrorCategory::Io,
            "catalog-io",
            true,
            CommitState::NotCommitted,
            false,
        ),
    ] {
        let deadline = Instant::now()
            + if expired {
                Duration::ZERO
            } else {
                Duration::from_secs(10)
            };
        let mut attempts = 0;
        let result = retry_owner_claim(
            || {
                attempts += 1;
                assert_eq!(attempts, 1, "unsafe claim retry: {reason}");
                let mut error = tgrep_core::managed::Error::new(category, reason, "claim failed");
                error.retryable = retryable;
                error.committed_state = committed;
                Err(error)
            },
            deadline,
        );
        let error = result.err().unwrap();
        assert_eq!(error.category, category);
        assert_eq!(error.reason_code, reason);
        assert_eq!(error.committed_state, committed);
    }
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

#[test]
fn reconciliation_metrics_and_unproved_coverage_are_visible_with_one_operation_worker() {
    for automatic in [false, true] {
        let fixture = Fixture::new();
        let mut configured = policy();
        configured["work"]["workers"] = json!(1);
        let mut daemon = start(
            &fixture,
            &configured,
            if automatic {
                &["--watch-mode", "poll", "--poll-interval", "120"]
            } else {
                &["--no-watch"]
            },
        );
        let hello = daemon.rpc("hello", json!({}));
        assert!(
            hello["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("reconciliation-phase-metrics"))
        );
        assert!(
            hello["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("event-driven-native-reconciliation"))
        );
        assert!(
            hello["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("serving-native-periodic-verification"))
        );
        for method in ["namespace.status", "maintenance.status"] {
            let report = daemon.rpc(method, json!({}));
            let repair = &report["reconciliation_policy"];
            assert_eq!(
                repair["configured_watch_mode"],
                if automatic { "poll" } else { "disabled" }
            );
            assert_eq!(repair["periodic_full_verification"], automatic);
            assert_eq!(
                repair["interval_ms"],
                if automatic {
                    json!(120_000)
                } else {
                    Value::Null
                }
            );
            assert_eq!(repair["poll_interval_ms"], 120_000);
            assert_eq!(repair["native_views"], 0);
            assert_eq!(repair["polling_views"], 0);
            assert_eq!(repair["disabled_views"], 0);
        }
        let (claim, guard) = owner(&daemon);
        let receipt = completed(
            &daemon,
            &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
        );
        let stats = &receipt["result"]["reconcile"];
        assert_eq!(stats["full"], true);
        assert_eq!(stats["files_read"], 3);
        assert_eq!(stats["base_reused"], 3);
        assert_eq!(stats["files_extracted"], 0);
        assert_eq!(stats["batch_retries"], 0);
        let timing = &stats["timings"];
        for field in [
            "queue_us",
            "base_materialization_us",
            "discovery_us",
            "planning_us",
            "content_verification_us",
            "worker_content_us",
            "worker_parent_us",
            "worker_open_us",
            "worker_read_verify_us",
            "worker_decode_hash_us",
            "worker_compare_extract_us",
            "parent_verify_us",
            "assembly_us",
            "checkpoint_us",
            "publication_prepare_us",
            "catalog_control_us",
            "namespace_catalog_us",
            "worker_wait_us",
            "parent_wait_us",
            "read_wait_us",
            "admitted_us",
        ] {
            assert!(timing[field].is_u64(), "{field}: {timing}");
        }
        for field in ["discovery_us", "content_verification_us", "checkpoint_us"] {
            assert!(timing[field].as_u64().unwrap() > 0, "{field}: {timing}");
        }
        let worker_parts: u64 = [
            "worker_parent_us",
            "worker_open_us",
            "worker_read_verify_us",
            "worker_decode_hash_us",
            "worker_compare_extract_us",
        ]
        .into_iter()
        .map(|field| timing[field].as_u64().unwrap())
        .sum();
        assert!(
            worker_parts <= timing["worker_content_us"].as_u64().unwrap(),
            "{timing}"
        );
        let wall_phases: u64 = [
            "base_materialization_us",
            "discovery_us",
            "planning_us",
            "content_verification_us",
            "assembly_us",
            "checkpoint_us",
            "publication_prepare_us",
        ]
        .into_iter()
        .map(|field| timing[field].as_u64().unwrap())
        .sum();
        assert!(
            wall_phases <= timing["admitted_us"].as_u64().unwrap(),
            "{timing}"
        );
        assert!(stats["control_checks"].as_u64().unwrap() > 0);
        assert!(
            stats["peak_private_bytes"].as_u64().unwrap()
                <= configured["work"]["private_work_bytes"].as_u64().unwrap()
        );
        assert!(stats["peak_read_bytes"].as_u64().unwrap() <= 64 * 1024 * 1024);
        assert!(stats["peak_parent_handles"].as_u64().unwrap() <= 256);
        let view = &receipt["result"]["current"];
        let status = daemon.rpc("views.status", json!({"id":view["id"]}));
        assert_eq!(status["last_reconcile"], *stats);
        let namespace = daemon.rpc("namespace.status", json!({}));
        let maintenance = daemon.rpc("maintenance.status", json!({}));
        let ordinary: Value =
            serde_json::from_str(&success(cli(&fixture.a, &["status", "."]))).unwrap();
        for report in [&status, &namespace, &maintenance, &ordinary] {
            let repair = &report["reconciliation_policy"];
            assert_eq!(repair["periodic_full_verification"], automatic);
            assert_eq!(repair["native_periodic_full_verification"], automatic);
            assert_eq!(
                repair["interval_ms"],
                if automatic {
                    json!(120_000)
                } else {
                    Value::Null
                }
            );
            assert_eq!(repair["poll_interval_ms"], 120_000);
            assert_eq!(
                repair["configured_watch_mode"],
                if automatic { "poll" } else { "disabled" }
            );
            assert_eq!(repair["incremental_coverage"], "unavailable");
            assert_eq!(repair["change_journal"], "not-integrated");
            assert_eq!(
                repair["native_missed_changes"],
                if automatic {
                    "repaired-by-periodic-verification"
                } else {
                    "requires-explicit-refresh"
                }
            );
            assert_eq!(
                repair["full_verification_reason"],
                if automatic {
                    json!("polling-update-mechanism")
                } else {
                    Value::Null
                }
            );
            assert_eq!(
                repair["full_repair_triggers"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("poll-interval")),
                automatic
            );
        }
        let repair = &namespace["reconciliation_policy"];
        assert_eq!(repair["scope"], "monitored-views");
        assert_eq!(repair["native_views"], 0);
        assert_eq!(repair["polling_views"], usize::from(automatic));
        assert_eq!(repair["disabled_views"], usize::from(!automatic));
        assert_eq!(namespace["allocation"]["work_slots"], 1);
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
fn polling_modes_repair_unreported_restored_mtime_and_membership_changes() {
    for fallback in [false, true] {
        let fixture = Fixture::new();
        let mut daemon = start(
            &fixture,
            &policy(),
            if fallback {
                &[
                    "--watch-mode",
                    "auto",
                    "--watch-budget",
                    "1",
                    "--poll-interval",
                    "1",
                ]
            } else {
                &["--watch-mode", "poll", "--poll-interval", "1"]
            },
        );
        let (claim, guard) = owner(&daemon);
        let receipt = completed(
            &daemon,
            &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
        );
        let view = &receipt["result"]["current"];
        let before = daemon.rpc("views.status", json!({"id":view["id"]}));
        assert_eq!(before["watch_mode"], "poll");
        assert_eq!(before["watch_fallback"].is_string(), fallback);
        let path = fixture.a.join("notes.txt");
        let metadata = fs::metadata(&path).unwrap();
        git(
            &fixture.a,
            &["update-index", "--assume-unchanged", "notes.txt"],
        );
        fs::write(&path, "shared_term fresh\n").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(metadata.modified().unwrap())
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), metadata.len());
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            metadata.modified().unwrap(),
        );
        assert!(git(&fixture.a, &["status", "--porcelain", "--", "notes.txt"]).is_empty());
        fs::remove_file(fixture.a.join(".hidden")).unwrap();
        fs::write(fixture.a.join(".missed-hidden"), "shared_term hidden\n").unwrap();
        fs::write(fixture.a.join("missed.txt"), "shared_term untracked\n").unwrap();
        fs::write(fixture.a.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(fixture.a.join("ignored.txt"), "shared_term ignored\n").unwrap();
        let lookup = daemon.rpc(
            "lookup",
            json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
        );
        let started = Instant::now();
        loop {
            let status = daemon.rpc("views.status", json!({"id":view["id"]}));
            if status["ready"] == true
                && status["input_epoch"].as_u64().unwrap() > before["input_epoch"].as_u64().unwrap()
            {
                let matches = retry_busy(
                    &daemon,
                    "search",
                    json!({
                        "root":lookup["root"],"view":view["id"],"expected_version":view["version"],
                        "query":{"pattern":"fresh"}
                    }),
                );
                let listing = retry_busy(
                    &daemon,
                    "files",
                    json!({
                        "root":lookup["root"],"view":view["id"],"expected_version":view["version"],
                        "query":{"hidden":true}
                    }),
                );
                let files = listing["files"].as_array().unwrap();
                if !matches["matches"].as_array().unwrap().is_empty()
                    && files.contains(&json!(".missed-hidden"))
                    && files.contains(&json!("missed.txt"))
                    && !files.contains(&json!(".hidden"))
                    && !files.contains(&json!("ignored.txt"))
                {
                    assert_eq!(matches["backend"], "shared-v2");
                    assert_eq!(listing["backend"], "shared-v2");
                    assert_eq!(status["last_reconcile"]["full"], true);
                    assert_eq!(status["last_reconcile"]["content_reads_avoided"], 0);
                    assert_eq!(
                        status["reconciliation_policy"]["periodic_full_verification"],
                        true
                    );
                    assert_eq!(status["reconciliation_policy"]["interval_ms"], 1000);
                    assert_eq!(
                        status["reconciliation_policy"]["full_verification_reason"],
                        "polling-update-mechanism"
                    );
                    assert_eq!(
                        status["reconciliation_policy"]["configured_watch_mode"],
                        if fallback { "auto" } else { "poll" }
                    );
                    break;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "periodic repair did not complete: {status}"
            );
            thread::sleep(Duration::from_millis(20));
        }
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
fn sibling_query_batches_preserve_managed_routes() {
    let mut fixture = Fixture::new();
    prepare_query_batches(&mut fixture);
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    completed(
        &daemon,
        &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
    );
    let before = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    assert_query_batch_parity(&fixture.a, "(via shared daemon v2)");
    let after = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    for field in ["view", "version", "generation"] {
        assert_eq!(before[field], after[field], "{field}");
    }
    assert_eq!(
        daemon.rpc("views.status", json!({"id":after["view"]}))["ready"],
        true
    );
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

#[test]
fn multiline_count_matches_uses_managed_per_file_totals() {
    let mut fixture = Fixture::new();
    prepare_multiline_counts(&mut fixture);
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    completed(
        &daemon,
        &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
    );
    assert_multiline_counts(&fixture.a.join("counts"), "(via shared daemon v2)");
    assert_return_count_parity(&fixture.a, "(via shared daemon v2)");
    assert_compact_shared_count_responses(&daemon, &fixture.a);
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

#[test]
fn ordered_filters_and_replacements_use_managed_views() {
    let mut fixture = Fixture::new();
    prepare_compat(&mut fixture);
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    completed(
        &daemon,
        &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
    );
    assert_compat_queries(&fixture.a.join("compat"), "(via shared daemon v2)");
    let view = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    assert_filter_order_follows_request(|query| {
        retry_busy(
            &daemon,
            "search",
            json!({
                "root":view["root"],"view":view["view"],"expected_version":view["version"],
                "query":query
            }),
        )
    });
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

fn completed(daemon: &Daemon, operation: &Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    let record = terminal_before(daemon, operation, deadline);
    diagnostics::completed_record(record, |view| {
        diagnostics::view_status_before(daemon, view, deadline)
    })
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
                "operation did not complete: {record}; view diagnostic: {}",
                diagnostics::operation_context(&record, |view| {
                    diagnostics::view_status_before(daemon, view, deadline)
                })
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
    let mut timing = diagnostics::StopTiming::new(Instant::now());
    let mut request = idle_request(daemon);
    loop {
        let response = daemon
            .try_rpc("stop-if-idle", request.clone())
            .unwrap_or_else(|error| {
                panic!(
                    "stop-if-idle: {error}; timing: {}",
                    timing.snapshot(Instant::now())
                )
            });
        if let Some(error) = response.get("error") {
            assert_eq!(
                error["data"]["category"],
                "busy",
                "{response}; timing: {}",
                timing.snapshot(Instant::now())
            );
            assert_eq!(
                error["data"]["retryable"],
                true,
                "{response}; timing: {}",
                timing.snapshot(Instant::now())
            );
            assert_eq!(
                error["data"]["committed_state"],
                "not-committed",
                "{response}; timing: {}",
                timing.snapshot(Instant::now())
            );
        } else if response["result"]["data"]["stopping"] == true {
            timing.accepted(Instant::now());
            break;
        } else {
            request = idle_request(daemon);
        }
        timing.assert_within_budget(Instant::now(), Some(&response));
        thread::sleep(Duration::from_millis(20));
    }
    while daemon
        .child
        .try_wait()
        .unwrap_or_else(|error| {
            panic!(
                "daemon exit check: {error}; timing: {}",
                timing.snapshot(Instant::now())
            )
        })
        .is_none()
    {
        timing.assert_within_budget(Instant::now(), None);
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

fn managed_cli(daemon: &Daemon, view: &Value, mut invoke: impl FnMut() -> Output) -> Output {
    let started = Instant::now();
    let mut last_fallback = None;
    loop {
        let before = daemon.rpc("views.status", json!({"id":view["view"]}));
        if before["ready"] == true {
            let output = invoke();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.lines().any(|line| {
                line == "warning: shared view unavailable (view-input-pending: view-input-pending) - scanning filesystem (not a legacy index)"
            }) {
                return output;
            }
            assert!(matches!(output.status.code(), Some(0 | 1)), "{output:?}");
            // A notification can close the gate after the ready snapshot.
            // Discard this scan; only a later managed result can prove parity.
            let after = daemon.rpc("views.status", json!({"id":view["view"]}));
            assert!(
                after["input_epoch"].as_u64().unwrap()
                    > before["authoritative"]["input_epoch"].as_u64().unwrap(),
                "fallback without a newer input epoch: before={before}, after={after}, output={output:?}"
            );
            last_fallback = Some(output);
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "managed CLI remains unavailable: {before}; last fallback={last_fallback:?}"
        );
        thread::sleep(Duration::from_millis(20));
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

#[track_caller]
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
        let indexed = managed_cli(daemon, &view, || invoke(&args));
        args.insert(0, "--no-index");
        let scan = invoke(&args);
        assert!(
            String::from_utf8_lossy(&indexed.stderr).contains("(via shared daemon v2)"),
            "CLI did not prove shared-v2 service for {root:?} (hidden={hidden}, lookup={view}): {}",
            String::from_utf8_lossy(&indexed.stderr)
        );
        assert_eq!(canonical_matches(indexed), canonical_matches(scan));
        let mut args = vec!["--files", "--glob", "!**/.git/**"];
        if hidden {
            args.push("--hidden");
        }
        args.push(".");
        let indexed = managed_cli(daemon, &view, || invoke(&args));
        args.insert(0, "--no-index");
        let scan = invoke(&args);
        let paths = |output: Output| {
            let mut paths: Vec<String> = success(output).lines().map(str::to_owned).collect();
            paths.sort();
            paths
        };
        assert!(
            indexed.stderr.is_empty(),
            "CLI file listing diagnostics for {root:?} (hidden={hidden}, lookup={view}): {}",
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
    // ripgrep's per-file `bytes_printed` excludes `end`; the summary sums them.
    let printed = |output: &Output| {
        let (mut emitted, mut ends, mut summary) = (0, Vec::new(), None);
        for line in String::from_utf8_lossy(&output.stdout).split_inclusive('\n') {
            let row: Value = serde_json::from_str(line).unwrap();
            let reported = row["data"]["stats"]["bytes_printed"].as_u64();
            match row["type"].as_str().unwrap() {
                "end" => ends.push((reported.unwrap(), std::mem::take(&mut emitted))),
                "summary" => summary = reported,
                _ => emitted += line.len() as u64,
            }
        }
        assert!(ends.iter().all(|(r, e)| r == e), "{ends:?}");
        assert_eq!(summary, Some(ends.iter().map(|(r, _)| r).sum()));
        summary
    };
    assert_eq!(printed(&indexed), printed(&scan));
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

fn prepare_eligibility(f: &mut Fixture) {
    let root = f.a.join("eligibility");
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("data")).unwrap();
    fs::write(
        root.join("docs/report.doc"),
        "{\\rtf1\\ansi tracked rtf needle}\n",
    )
    .unwrap();
    fs::write(root.join("data/table.pdf"), "tracked pdf needle\n").unwrap();
    fs::write(root.join("data/early.doc"), b"\0nul needle\n").unwrap();
    fs::write(root.join("plain.txt"), "plain needle\n").unwrap();
    git(&f.a, &["add", "eligibility"]);
    git(&f.a, &["commit", "-qm", "content eligibility"]);
    f.revision = git(&f.a, &["rev-parse", "HEAD"]);
    fs::write(root.join("docs/untracked.dat"), "untracked dat needle\n").unwrap();
}

fn content_only_attach(fixture: &Fixture, claim: &OwnerClaim) -> Value {
    let mut input = attach_input(fixture, claim);
    input["request"]["profile"]["eligibility"] = json!("content-only");
    input
}

fn sorted_rows(output: &Output) -> Vec<String> {
    let mut rows: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    rows.sort_unstable();
    rows
}

/// Runs `args` indexed and as an explicit scan with the same eligibility.
fn eligibility_query(root: &Path, args: &[&str]) -> (Output, Output) {
    let mut common = vec!["--color", "never", "--path-separator", "/", "--stats"];
    common.extend_from_slice(args);
    let indexed = cli(root, &common);
    let mut scan = vec!["--no-index"];
    scan.extend_from_slice(&common);
    (indexed, cli(root, &scan))
}

#[test]
fn content_only_profile_serves_extension_named_text_from_the_managed_view() {
    let mut fixture = Fixture::new();
    prepare_eligibility(&mut fixture);
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let hello = daemon.rpc("hello", json!({}));
    assert!(
        hello["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("content-eligibility-profile"))
    );
    assert_eq!(
        hello["profile"],
        serde_json::from_str::<Value>(PROFILE).unwrap()
    );
    let (claim, guard) = owner(&daemon);
    let receipt = completed(
        &daemon,
        &daemon.rpc("views.attach", content_only_attach(&fixture, &claim)),
    );
    let view = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    assert_eq!(
        view["generation"]["profile"]["eligibility"], "content-only",
        "{receipt}"
    );

    let content = ["--content-eligibility", "content"];
    let expected = [
        "eligibility/data/table.pdf:1",
        "eligibility/docs/report.doc:1",
        "eligibility/docs/untracked.dat:1",
        "eligibility/plain.txt:1",
    ];
    let mut args = content.to_vec();
    args.extend(["-c", "-e", "needle", "--", "eligibility"]);
    let (indexed, scanned) = eligibility_query(&fixture.a, &args);
    let stderr = String::from_utf8_lossy(&indexed.stderr);
    assert!(stderr.contains("(via shared daemon v2)"), "{stderr}");
    assert!(!stderr.contains("warning:"), "{stderr}");
    assert_eq!(sorted_rows(&indexed), expected);
    assert_eq!(sorted_rows(&scanned), expected);
    assert_eq!(indexed.status.code(), Some(0));

    // The legacy default never reads a view admitted under wider coverage.
    let (indexed, scanned) =
        eligibility_query(&fixture.a, &["-c", "-e", "needle", "--", "eligibility"]);
    let stderr = String::from_utf8_lossy(&indexed.stderr);
    assert!(
        stderr.contains("warning: shared view unavailable")
            && stderr.contains("content eligibility"),
        "{stderr}"
    );
    assert!(!stderr.contains("(via shared daemon v2)"), "{stderr}");
    assert_eq!(sorted_rows(&indexed), ["eligibility/plain.txt:1"]);
    assert_eq!(sorted_rows(&indexed), sorted_rows(&scanned));

    // Filename membership is the same under either eligibility.
    for args in [
        vec!["--files", "eligibility"],
        vec!["--content-eligibility", "content", "--files", "eligibility"],
    ] {
        let (indexed, scanned) = eligibility_query(&fixture.a, &args);
        let stderr = String::from_utf8_lossy(&indexed.stderr);
        assert!(stderr.contains("(via shared daemon v2)"), "{stderr}");
        assert_eq!(sorted_rows(&indexed).len(), 5);
        assert_eq!(sorted_rows(&indexed), sorted_rows(&scanned));
    }

    // Incremental verification keeps the profile: text edits are indexed and
    // NUL content is still excluded under an extension-heuristic filename.
    fs::write(
        fixture.a.join("eligibility/docs/report.doc"),
        "{\\rtf1 revised marker}\n",
    )
    .unwrap();
    fs::write(
        fixture.a.join("eligibility/data/table.pdf"),
        b"pdf\0needle\n",
    )
    .unwrap();
    daemon.rpc(
        "views.invalidate",
        json!({"view":view["view"],"expected_version":view["version"],"owner":claim.owner,
            "changed":["eligibility/docs/report.doc","eligibility/data/table.pdf"],"full":false}),
    );
    let refreshed = completed(
        &daemon,
        &daemon.rpc(
            "views.refresh",
            json!({"token":token(&claim,2),"request":{"view":view["view"],
                "expected_version":view["version"],"owner":claim.owner,"allocation_version":1}}),
        ),
    );
    // The invalidation also schedules automatic work that may consume the
    // hints first, leaving this refresh a full pass; either path must keep
    // the attached profile. Hint-only processing is covered in tgrep-core.
    assert_eq!(
        refreshed["result"]["current"]["current"]["key"]["profile"]["eligibility"], "content-only",
        "{refreshed}"
    );
    for (pattern, rows) in [
        (
            "needle",
            vec![
                "eligibility/docs/untracked.dat:1",
                "eligibility/plain.txt:1",
            ],
        ),
        ("revised marker", vec!["eligibility/docs/report.doc:1"]),
    ] {
        let mut args = content.to_vec();
        args.extend(["-c", "-e", pattern, "--", "eligibility"]);
        let (indexed, scanned) = eligibility_query(&fixture.a, &args);
        let stderr = String::from_utf8_lossy(&indexed.stderr);
        assert!(stderr.contains("(via shared daemon v2)"), "{stderr}");
        assert_eq!(sorted_rows(&indexed), rows, "{pattern}");
        assert_eq!(sorted_rows(&scanned), rows, "{pattern}");
    }

    // A same-root join cannot widen or narrow the attached profile.
    let current = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    let mut join = attach_input(&fixture, &claim);
    join["token"] = token(&claim, 3);
    join["request"]["lease"] = json!("default-client");
    join["request"]["revision"] = Value::Null;
    join["request"]["accept_current"] =
        json!({"view":current["view"],"version":current["version"]});
    let failed = terminal(&daemon, &daemon.rpc("views.attach", join));
    assert_eq!(failed["state"], "failed", "{failed}");
    assert!(failed.to_string().contains("profile differs"), "{failed}");

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

#[test]
fn content_only_query_scans_with_matching_rules_against_a_default_view() {
    let mut fixture = Fixture::new();
    prepare_eligibility(&mut fixture);
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    completed(
        &daemon,
        &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
    );
    let (indexed, scanned) = eligibility_query(
        &fixture.a,
        &[
            "--content-eligibility",
            "content",
            "-c",
            "-e",
            "needle",
            "--",
            "eligibility",
        ],
    );
    let stderr = String::from_utf8_lossy(&indexed.stderr);
    assert!(
        stderr.contains("warning: shared view unavailable")
            && stderr.contains("content eligibility"),
        "{stderr}"
    );
    assert_eq!(sorted_rows(&indexed).len(), 4);
    assert_eq!(sorted_rows(&indexed), sorted_rows(&scanned));
    let (indexed, _) = eligibility_query(&fixture.a, &["-c", "-e", "needle", "--", "eligibility"]);
    let stderr = String::from_utf8_lossy(&indexed.stderr);
    assert!(stderr.contains("(via shared daemon v2)"), "{stderr}");
    assert_eq!(sorted_rows(&indexed), ["eligibility/plain.txt:1"]);

    // The server enforces the same boundary for clients that skip the check.
    let view = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    let response = daemon
        .try_rpc(
            "search",
            json!({"root":view["root"],"view":view["view"],"expected_version":view["version"],
                "query":{"pattern":"needle","content_eligibility":"content-only"}}),
        )
        .unwrap();
    assert!(
        response["error"]
            .to_string()
            .contains("content eligibility"),
        "{response}"
    );

    // While a default lease is live, a content-only join fails closed.
    let mut join = content_only_attach(&fixture, &claim);
    join["token"] = token(&claim, 2);
    join["request"]["lease"] = json!("content-client");
    join["request"]["revision"] = Value::Null;
    join["request"]["accept_current"] = json!({"view":view["view"],"version":view["version"]});
    let failed = terminal(&daemon, &daemon.rpc("views.attach", join));
    assert_eq!(failed["state"], "failed", "{failed}");
    assert!(failed.to_string().contains("profile differs"), "{failed}");

    // Once every lease is released, a fresh attach with an explicit revision
    // replaces the inactive root view with the content-only profile.
    daemon.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"managed-client"}),
    );
    let mut upgrade = content_only_attach(&fixture, &claim);
    upgrade["token"] = token(&claim, 3);
    upgrade["request"]["lease"] = json!("content-client");
    completed(&daemon, &daemon.rpc("views.attach", upgrade));
    let upgraded = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    assert_ne!(upgraded["view"], view["view"]);
    assert_eq!(
        upgraded["generation"]["profile"]["eligibility"],
        "content-only"
    );
    let (indexed, scanned) = eligibility_query(
        &fixture.a,
        &[
            "--content-eligibility",
            "content",
            "-c",
            "-e",
            "needle",
            "--",
            "eligibility",
        ],
    );
    let stderr = String::from_utf8_lossy(&indexed.stderr);
    assert!(stderr.contains("(via shared daemon v2)"), "{stderr}");
    assert_eq!(sorted_rows(&indexed).len(), 4);
    assert_eq!(sorted_rows(&indexed), sorted_rows(&scanned));
    daemon.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"content-client"}),
    );
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
    drop(daemon);
    fixture.temp.close().unwrap();
}

#[test]
fn unmanaged_leases_and_search_only_flags_refuse_content_only_profiles() {
    let fixture = Fixture::new();
    let mut configured = policy();
    configured["storage"] = json!("compatibility-retain-all");
    let mut daemon = start(&fixture, &configured, &["--no-watch"]);
    let common = tgrep_core::generations::Repository::discover(&fixture.a)
        .unwrap()
        .common_dir()
        .to_path_buf();
    let legacy =
        tgrep_core::managed::read_private_control_file(&common.join("tgrep-daemon-v1.json"))
            .unwrap();
    let mut profile = serde_json::from_str::<Value>(PROFILE).unwrap();
    profile["eligibility"] = json!("content-only");
    let response = daemon
        .raw(&json!({
            "jsonrpc":"2.0","protocol":1,"instance":legacy["instance"],
            "repository":daemon.marker["repository"],"id":1,"method":"attach",
            "params":{"root":fs::canonicalize(&fixture.a).unwrap(),"revision":fixture.revision,
                "profile":profile,"lease":"legacy-client"}
        }))
        .unwrap();
    assert!(
        response["error"]
            .to_string()
            .contains("default content eligibility"),
        "{response}"
    );
    stop(&mut daemon);
    drop(daemon);
    for args in [
        vec!["--content-eligibility", "content", "index", "."],
        vec!["--content-eligibility", "content", "serve", "."],
    ] {
        let output = cli(&fixture.a, &args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--content-eligibility"),
            "{args:?}"
        );
    }
    assert!(!fixture.a.join(".tgrep").exists());
    fixture.temp.close().unwrap();
}
