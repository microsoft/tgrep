// Copyright (c) Microsoft Corporation. All rights reserved.

use super::*;

pub(super) fn pause(daemon: &Daemon, point: &str) -> Value {
    daemon.rpc(
        "testing.install",
        json!({
            "point":point,"operation":null,"skip_hits":0,
            "action":{"mode":"pause","timeout_ms":30000}
        }),
    )
}

pub(super) fn pause_for_operation(daemon: &Daemon, point: &str, token: &Value) -> Value {
    daemon.rpc(
        "testing.install",
        json!({
            "point":point,"operation":null,"skip_hits":0,
            "action":{"mode":"pause-token","timeout_ms":30000,"token":token}
        }),
    )
}

pub(super) fn reached(daemon: &Daemon, hook: &Value) -> Value {
    let started = Instant::now();
    loop {
        let status = daemon.rpc("testing.status", json!({}));
        assert_eq!(status["ticket"], hook["ticket"]);
        match status["stage"].as_str().unwrap() {
            "waiting" => return status,
            "armed" => {}
            _ => panic!("hook did not remain armed or reach its barrier: {status}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "boundary was not reached: {status}; maintenance: {}; pending operations: {}",
            daemon.rpc("maintenance.status", json!({})),
            daemon.rpc("operations.pending", json!({}))
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn crash(daemon: &mut Daemon) {
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
}

fn collected(daemon: &Daemon, object: &Value) -> bool {
    let response = daemon
        .try_rpc("objects.inspect", json!({"id":object}))
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

fn reconnect(
    fixture: &Fixture,
    daemon: &Daemon,
    view: &Value,
    old: &OwnerClaim,
    point: &str,
) -> (OwnerClaim, OwnerGuard, u64) {
    let (claim, guard) = owner(daemon);
    let mut attach = attach_input(fixture, &claim);
    attach["request"]["revision"] = Value::Null;
    attach["request"]["lease"] = json!("reconnected-client");
    attach["request"]["accept_current"] = json!({"view":view["id"],"version":view["version"]});
    let attached = retry_attach(
        daemon,
        &claim,
        Some(view),
        attach,
        point,
        Instant::now() + Duration::from_secs(30),
        |_| {},
    )
    .unwrap_or_else(|failures| {
        panic!("{point}: reconnect exhausted its retry bound: {failures:?}")
    });
    daemon.rpc("owners.release", json!({"claim":old}));
    (claim, guard, next_sequence(&attached))
}

fn next_sequence(operation: &Value) -> u64 {
    operation["token"]["sequence"]
        .as_u64()
        .unwrap()
        .checked_add(1)
        .unwrap()
}

fn initial_attach(daemon: &Daemon, claim: &OwnerClaim, attach: Value, point: &str) -> Value {
    retry_attach(
        daemon,
        claim,
        None,
        attach,
        point,
        Instant::now() + Duration::from_secs(30),
        |_| {},
    )
    .unwrap_or_else(|failures| {
        panic!("{point}: initial attachment exhausted its retry bound: {failures:?}")
    })
}

fn retry_attach(
    daemon: &Daemon,
    claim: &OwnerClaim,
    view: Option<&Value>,
    attach: Value,
    point: &str,
    deadline: Instant,
    before_attempt: impl FnMut(&Value),
) -> Result<Value, Vec<Value>> {
    super::attach_retry::retry_attach(
        daemon,
        claim,
        view,
        attach,
        super::attach_retry::Retry {
            point,
            deadline,
            allowance: super::attach_retry::RetryableAttach::CatalogIo,
        },
        before_attempt,
    )
}

fn retryable_attach_catalog_error(record: &Value) -> bool {
    super::attach_retry::RetryableAttach::CatalogIo.permits(record)
}

#[test]
fn reconnect_retry_requires_exact_uncommitted_catalog_contention() {
    let permitted = json!({
        "kind":"attach","state":"failed","cancelled":false,"committed_state":"not-committed",
        "error":{"category":"busy","reason_code":"catalog-io","retryable":true,"committed_state":"not-committed"}
    });
    assert!(retryable_attach_catalog_error(&permitted));
    for (pointer, value) in [
        ("/kind", json!("migrate")),
        ("/kind", json!("refresh")),
        ("/state", json!("accepted")),
        ("/state", json!("preparing")),
        ("/state", json!("completed")),
        ("/state", json!("cancelled")),
        ("/cancelled", json!(true)),
        ("/committed_state", json!("committed")),
        ("/committed_state", json!("unknown")),
        ("/committed_state", Value::Null),
        ("/error/category", json!("io")),
        ("/error/reason_code", json!("view-work-active")),
        ("/error/retryable", json!(false)),
        ("/error/committed_state", json!("committed")),
        ("/error/committed_state", json!("unknown")),
        ("/error", Value::Null),
    ] {
        let mut rejected = permitted.clone();
        *rejected.pointer_mut(pointer).unwrap() = value;
        assert!(
            !retryable_attach_catalog_error(&rejected),
            "{pointer}: {rejected}"
        );
    }
}

#[test]
fn initial_attach_retries_catalog_busy_without_changing_pending_intent() {
    for point in ["attach-before-lease", "attach-before-resume"] {
        for fail_attempts in [1, 8] {
            let fixture = Fixture::new();
            let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
            let (claim, guard) = owner(&daemon);
            let input = attach_input(&fixture, &claim);
            let missing = daemon
                .try_rpc("lookup", json!({"root":input["request"]["root"]}))
                .unwrap();
            assert_eq!(
                missing["error"]["data"]["reason_code"],
                "worktree-not-attached"
            );
            let mut submitted = Vec::new();
            let mut pending = Vec::new();
            let result = retry_attach(
                &daemon,
                &claim,
                None,
                input.clone(),
                point,
                Instant::now() + Duration::from_secs(30),
                |attempt| {
                    if !submitted.is_empty() {
                        let lookup = daemon
                            .try_rpc("lookup", json!({"root":input["request"]["root"]}))
                            .unwrap();
                        if point == "attach-before-lease" {
                            assert_eq!(
                                lookup["error"]["data"]["reason_code"],
                                "worktree-not-attached"
                            );
                        } else {
                            let status = daemon.rpc(
                                "views.status",
                                json!({"id":lookup["result"]["data"]["view"]}),
                            );
                            assert_eq!(status["leases"], 1);
                            assert_eq!(status["ready"], false);
                            assert_eq!(status["authoritative"]["committed"], false);
                            assert!(status["authoritative"]["current"].is_null());
                            assert_eq!(
                                status["authoritative"]["pending"]["commit"],
                                fixture.revision
                            );
                            pending.push(status["authoritative"].clone());
                        }
                    }
                    submitted.push(attempt.clone());
                    if submitted.len() <= fail_attempts {
                        daemon.rpc(
                            "testing.install",
                            json!({
                                "point":point,"operation":null,"skip_hits":0,
                                "action":{"mode":"catalog-busy-token","token":attempt["token"]}
                            }),
                        );
                    }
                },
            );
            assert_eq!(submitted.len(), if fail_attempts == 1 { 2 } else { 8 });
            for (index, attempt) in submitted.iter().enumerate() {
                assert_eq!(attempt["request"], input["request"]);
                assert_eq!(attempt["token"], token(&claim, index as u64 + 1));
            }
            let failed = daemon.rpc("operations.lookup", input["token"].clone());
            assert!(retryable_attach_catalog_error(&failed), "{point}: {failed}");
            assert!(
                failed["error"]["detail"]
                    .as_str()
                    .unwrap()
                    .contains("injected catalog contention")
            );
            if point == "attach-before-lease" {
                assert!(failed["progress"]["view"].is_null());
                assert!(failed["progress"]["resolved_commit"].is_null());
                assert!(pending.is_empty());
            } else {
                assert_eq!(failed["progress"]["view"], pending[0]["id"]);
                assert_eq!(failed["progress"]["resolved_commit"], fixture.revision);
                for snapshot in &pending {
                    assert_eq!(snapshot, &pending[0]);
                }
            }
            let last_injected = daemon.rpc(
                "operations.lookup",
                submitted[fail_attempts - 1]["token"].clone(),
            );
            let hook = daemon.rpc("testing.status", json!({}));
            assert_eq!(hook["stage"], "fired");
            assert_eq!(hook["reached_operation"], last_injected["id"]);
            if fail_attempts == 1 {
                let attached = result.unwrap();
                assert_eq!(attached["token"], token(&claim, 2));
                assert_ne!(attached["id"], failed["id"]);
                assert_eq!(
                    attached["result"]["lease"]["operation"],
                    if point == "attach-before-lease" {
                        attached["id"].clone()
                    } else {
                        failed["id"].clone()
                    }
                );
                assert_eq!(attached["result"]["lease"]["token"], "managed-client");
                assert_eq!(
                    attached["result"]["lease"]["owner"],
                    input["request"]["owner"]
                );
                assert_eq!(attached["result"]["current"]["version"], 1);
                assert_eq!(
                    attached["result"]["current"]["current"]["commit"],
                    fixture.revision
                );
                if let Some(pending) = pending.first() {
                    assert_eq!(attached["result"]["current"]["id"], pending["id"]);
                    assert_eq!(
                        attached["result"]["current"]["current"]["key"],
                        pending["pending"]["key"]
                    );
                }
                assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
                completed(
                    &daemon,
                    &daemon.rpc(
                        "views.refresh",
                        json!({"token":token(&claim,next_sequence(&attached)),"request":{
                            "view":attached["result"]["current"]["id"],"expected_version":1,
                            "owner":claim.owner,"allocation_version":1
                        }}),
                    ),
                );
            } else {
                let failures = result.unwrap_err();
                assert_eq!(failures.len(), 8);
                for failure in failures {
                    assert_eq!(
                        daemon.rpc(
                            "views.attach",
                            json!({
                                "token":failure["token"],"request":failure["request"]
                            })
                        ),
                        failure
                    );
                }
            }
            assert_eq!(daemon.rpc("views.attach", input.clone()), failed);
            let expired = retry_attach(&daemon, &claim, None, input, point, Instant::now(), |_| {
                panic!("an expired deadline must not submit another attempt")
            });
            assert!(expired.unwrap_err().is_empty());
            let released = daemon.rpc("owners.release", json!({"claim":claim}));
            assert_eq!(
                released["leases_released"],
                if point == "attach-before-lease" && fail_attempts == 8 {
                    0
                } else {
                    1
                }
            );
            drop(guard);
            stop(&mut daemon);
            drop(daemon);
            fixture.temp.close().unwrap();
        }
    }
}

#[test]
fn initial_attach_replays_in_flight_and_completed_acceptance_without_new_tokens() {
    let fixture = Fixture::new();
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    let input = attach_input(&fixture, &claim);
    let hook = pause_for_operation(&daemon, "attach-before-resume", &input["token"]);
    daemon.without_response("views.attach", input.clone());
    let barrier = reached(&daemon, &hook);
    let accepted = daemon.rpc("operations.lookup", input["token"].clone());
    assert_eq!(barrier["reached_operation"], accepted["id"]);
    assert!(matches!(
        accepted["state"].as_str(),
        Some("accepted" | "preparing")
    ));
    let mut submitted = Vec::new();
    let attached = retry_attach(
        &daemon,
        &claim,
        None,
        input.clone(),
        "lost-acceptance",
        Instant::now() + Duration::from_secs(30),
        |attempt| {
            submitted.push(attempt.clone());
            daemon.rpc("testing.release", json!({"id":hook["ticket"]}));
        },
    )
    .unwrap();
    assert_eq!(submitted.as_slice(), std::slice::from_ref(&input));
    assert_eq!(attached["id"], accepted["id"]);
    assert_eq!(attached["token"], input["token"]);
    assert_eq!(attached["result"]["lease"]["operation"], accepted["id"]);
    daemon.without_response("views.attach", input.clone());
    assert_eq!(
        initial_attach(&daemon, &claim, input, "lost-completion"),
        attached
    );
    assert_eq!(next_sequence(&attached), 2);
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
    let released = daemon.rpc("owners.release", json!({"claim":claim}));
    assert_eq!(released["leases_released"], 1);
    drop(guard);
    stop(&mut daemon);
    drop(daemon);
    fixture.temp.close().unwrap();
}

#[test]
fn reconnect_retries_injected_catalog_busy_with_fresh_tokens_and_bounded_attempts() {
    let fixture = Fixture::new();
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (original, original_guard) = owner(&daemon);
    let first = initial_attach(
        &daemon,
        &original,
        attach_input(&fixture, &original),
        "reconnect-control-setup",
    );
    let view = &first["result"]["current"];
    for fail_attempts in [1, 8] {
        let (claim, guard) = owner(&daemon);
        let mut attach = attach_input(&fixture, &claim);
        attach["request"]["revision"] = Value::Null;
        attach["request"]["lease"] = json!("reconnected-client");
        attach["request"]["accept_current"] = json!({"view":view["id"],"version":view["version"]});
        let request = attach["request"].clone();
        let mut submitted = Vec::new();
        let result = retry_attach(
            &daemon,
            &claim,
            Some(view),
            attach,
            "injected-catalog-busy",
            Instant::now() + Duration::from_secs(30),
            |input| {
                submitted.push(input.clone());
                if submitted.len() <= fail_attempts {
                    daemon.rpc(
                        "testing.install",
                        json!({
                            "point":"attach-before-resume","operation":null,"skip_hits":0,
                            "action":{"mode":"catalog-busy-token","token":input["token"]}
                        }),
                    );
                }
            },
        );
        assert_eq!(submitted.len(), if fail_attempts == 1 { 2 } else { 8 });
        for (index, input) in submitted.iter().enumerate() {
            assert_eq!(input["request"], request);
            assert_eq!(input["token"], token(&claim, index as u64 + 1));
        }
        let failed = daemon.rpc("operations.lookup", submitted[0]["token"].clone());
        assert!(retryable_attach_catalog_error(&failed), "{failed}");
        assert_eq!(failed["progress"]["view"], view["id"]);
        assert_eq!(
            failed["progress"]["resolved_commit"],
            view["current"]["commit"]
        );
        assert!(
            failed["error"]["detail"]
                .as_str()
                .unwrap()
                .contains("injected catalog contention"),
            "{failed}"
        );
        let last_injected = daemon.rpc(
            "operations.lookup",
            submitted[fail_attempts - 1]["token"].clone(),
        );
        let hook = daemon.rpc("testing.status", json!({}));
        assert_eq!(hook["stage"], "fired", "{hook}");
        assert_eq!(hook["reached_operation"], last_injected["id"], "{hook}");
        if fail_attempts == 1 {
            let attached = result.unwrap();
            assert_ne!(attached["id"], failed["id"]);
            assert_eq!(attached["token"], submitted[1]["token"]);
            assert_eq!(attached["result"]["lease"]["token"], request["lease"]);
            assert_eq!(attached["result"]["lease"]["operation"], failed["id"]);
            assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
            completed(
                &daemon,
                &daemon.rpc(
                    "views.refresh",
                    json!({"token":token(&claim,next_sequence(&attached)),"request":{
                        "view":view["id"],"expected_version":view["version"],
                        "owner":claim.owner,"allocation_version":1
                    }}),
                ),
            );
        } else {
            let failures = result.unwrap_err();
            assert_eq!(failures.len(), 8);
            for failure in failures {
                assert_eq!(
                    daemon.rpc("operations.lookup", failure["token"].clone()),
                    failure
                );
            }
        }
        assert_eq!(
            daemon.rpc("operations.lookup", submitted[0]["token"].clone()),
            failed
        );
        let expired = retry_attach(
            &daemon,
            &claim,
            Some(view),
            submitted[0].clone(),
            "expired-before-submission",
            Instant::now(),
            |_| panic!("an expired deadline must not submit another attempt"),
        );
        assert!(expired.unwrap_err().is_empty());
        daemon.rpc("owners.release", json!({"claim":claim}));
        drop(guard);
    }
    daemon.rpc("owners.release", json!({"claim":original}));
    drop(original_guard);
    stop(&mut daemon);
    drop(daemon);
    fixture.temp.close().unwrap();
}

#[test]
fn committed_idle_acceptance_error_does_not_stop_a_busy_daemon() {
    let fixture = Fixture::new();
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    initial_attach(
        &daemon,
        &claim,
        attach_input(&fixture, &claim),
        "idle-control-setup",
    );
    let input = idle_request(&daemon);
    let hook = daemon.rpc(
        "testing.install",
        json!({
            "point":"idle-accepted","operation":null,"skip_hits":0,
            "action":{"mode":"error-token","category":"io","token":input["token"]}
        }),
    );
    let response = retry_busy_response(&daemon, "stop-if-idle", input.clone());
    assert_eq!(response["error"]["data"]["category"], "io", "{response}");
    assert_eq!(response["error"]["data"]["committed_state"], "committed");
    let hit = daemon.rpc("testing.status", json!({}));
    assert_eq!(hit["ticket"], hook["ticket"]);
    assert_eq!(hit["stage"], "fired");
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
    let replay = retry_busy(&daemon, "stop-if-idle", input);
    assert_eq!(replay["stopping"], false);
    assert!(replay["leases"].as_u64().unwrap() > 0);
    daemon.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"managed-client"}),
    );
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
}

#[test]
fn live_post_commit_recovery_restores_attach_refresh_and_migration_without_restart() {
    use super::live::wait_for;

    for mode in ["attach", "refresh", "migrate"] {
        let fixture = Fixture::new();
        let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
        let (claim, guard) = owner(&daemon);
        let install = |token: &Value| {
            daemon.rpc(
                "testing.install",
                json!({
                    "point":"view-after-commit","operation":null,"skip_hits":0,
                    "action":{"mode":"error-token","category":"io","token":token}
                }),
            )
        };
        let attach = attach_input(&fixture, &claim);
        let mut hook = (mode == "attach").then(|| install(&attach["token"]));
        let mut affected = if mode == "attach" {
            completed(&daemon, &daemon.rpc("views.attach", attach))
        } else {
            initial_attach(&daemon, &claim, attach, mode)
        };
        let sequence = next_sequence(&affected);
        let first = affected["result"]["current"].clone();
        if mode != "attach" {
            wait_for(
                &daemon,
                "views.status",
                json!({"id":first["id"]}),
                |status| status["ready"] == true && status["work"].is_null(),
            );
            fs::write(
                fixture.a.join("notes.txt"),
                "shared_term publication recovery input\n",
            )
            .unwrap();
            let refresh = json!({"token":token(&claim,sequence),"request":{
                "view":first["id"],"expected_version":first["version"],
                "owner":claim.owner,"allocation_version":1
            }});
            if mode == "migrate" {
                git(&fixture.a, &["add", "notes.txt"]);
                git(&fixture.a, &["commit", "-qm", "recovery target"]);
                let target = git(&fixture.a, &["rev-parse", "HEAD"]);
                completed(&daemon, &daemon.rpc("views.refresh", refresh));
                wait_for(
                    &daemon,
                    "views.status",
                    json!({"id":first["id"]}),
                    |status| status["ready"] == true && status["work"].is_null(),
                );
                let migration = json!({
                    "token":token(&claim,sequence + 1),"request":{
                        "view":first["id"],"root":first["root"],"expected_version":first["version"],
                        "target_commit":target,"profile":serde_json::from_str::<Value>(PROFILE).unwrap(),
                        "owner":claim.owner,"allocation_version":1
                    }
                });
                hook = Some(install(&migration["token"]));
                affected = completed(&daemon, &daemon.rpc("views.advance", migration));
            } else {
                hook = Some(install(&refresh["token"]));
                affected = completed(&daemon, &daemon.rpc("views.refresh", refresh));
            }
        }
        let hit = daemon.rpc("testing.status", json!({}));
        assert_eq!(hit["ticket"], hook.unwrap()["ticket"]);
        assert_eq!(hit["stage"], "fired");
        assert_eq!(hit["reached_operation"], affected["id"]);
        let error_wait = Instant::now();
        while affected["error"].is_null() {
            assert!(
                error_wait.elapsed() < Duration::from_secs(20),
                "post-commit failure was not recorded: {affected}"
            );
            thread::sleep(Duration::from_millis(5));
            affected = daemon.rpc("operations.inspect", json!({"id":affected["id"]}));
        }
        assert_eq!(affected["error"]["category"], "io", "{mode}: {affected}");
        assert_eq!(affected["committed_state"], "committed");
        let committed = &affected["result"]["current"];
        assert_eq!(
            committed["version"],
            json!(if mode == "migrate" { 2 } else { 1 })
        );
        if mode == "refresh" {
            assert_ne!(committed["checkpoint"], first["checkpoint"]);
        }
        fs::write(
            fixture.a.join("after-client.txt"),
            "shared_term input after lost publication response\n",
        )
        .unwrap();
        daemon.rpc(
            "views.invalidate",
            json!({
                "view":first["id"],"owner":claim.owner,"expected_version":committed["version"],
                "changed":["after-client.txt"],"full":false
            }),
        );
        let ready = wait_for(
            &daemon,
            "views.status",
            json!({"id":first["id"]}),
            |status| status["ready"] == true && status["work"].is_null(),
        );
        assert_eq!(ready["authoritative"]["version"], committed["version"]);
        assert_eq!(ready["authoritative"]["current"], committed["current"]);
        assert_eq!(ready["authoritative"]["instance"], json!(claim.instance));
        assert_ne!(
            ready["authoritative"]["checkpoint"],
            committed["checkpoint"]
        );
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
fn independent_migration_crash_hook_preserves_background_reconciliation() {
    let fixture = Fixture::new();
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    let attached = initial_attach(
        &daemon,
        &claim,
        attach_input(&fixture, &claim),
        "independent-migration-setup",
    );
    let sequence = next_sequence(&attached);
    let current = &attached["result"]["current"];
    fs::write(
        fixture.a.join("notes.txt"),
        "shared_term migration target\n",
    )
    .unwrap();
    git(&fixture.a, &["add", "notes.txt"]);
    git(&fixture.a, &["commit", "-qm", "target"]);
    let target = git(&fixture.a, &["rev-parse", "HEAD"]);
    completed(
        &daemon,
        &daemon.rpc(
            "views.refresh",
            json!({"token":token(&claim,sequence),"request":{
                "view":current["id"],"expected_version":1,"owner":claim.owner,"allocation_version":1
            }}),
        ),
    );
    let before = super::live::wait_for(
        &daemon,
        "views.status",
        json!({"id":current["id"]}),
        |status| status["ready"] == true && status["work"].is_null(),
    );
    let migration_token = token(&claim, sequence + 1);
    let hook = pause_for_operation(&daemon, "object-intent-saved", &migration_token);
    let invalidated = daemon.rpc(
        "views.invalidate",
        json!({
            "view":current["id"],"owner":claim.owner,"expected_version":1,
            "changed":[],"full":true
        }),
    );
    let started = Instant::now();
    let premature = loop {
        let observed = daemon.rpc("testing.status", json!({}));
        assert_eq!(observed["ticket"], hook["ticket"]);
        if observed["stage"] == "waiting" {
            break Some(observed);
        }
        assert_eq!(observed["stage"], "armed", "{observed}");
        let status = daemon.rpc("views.status", json!({"id":current["id"]}));
        if status["ready"] == true
            && status["work"].is_null()
            && status["authoritative"]["input_epoch"] == invalidated["input_epoch"]
            && status["authoritative"]["checkpoint"] != before["authoritative"]["checkpoint"]
        {
            break None;
        }
        assert!(started.elapsed() < Duration::from_secs(20), "{status}");
        thread::sleep(Duration::from_millis(5));
    };
    let operation = daemon.rpc("views.advance", json!({"token":migration_token,"request":{
        "view":current["id"],"root":current["root"],"expected_version":1,"target_commit":target,
        "profile":serde_json::from_str::<Value>(PROFILE).unwrap(),"owner":claim.owner,"allocation_version":1
    }}));
    let fired_before_admission = premature.is_some();
    let barrier = premature.unwrap_or_else(|| reached(&daemon, &hook));
    let captured = daemon.rpc(
        "operations.inspect",
        json!({"id":barrier["reached_operation"]}),
    );
    assert_eq!(
        barrier["reached_operation"], operation["id"],
        "crash hook captured unrelated work before migration admission: {captured}"
    );
    assert!(!fired_before_admission);
    assert_eq!(captured["kind"], "migrate");
    daemon.rpc("testing.release", json!({"id":hook["ticket"]}));
    let completed = completed(&daemon, &operation);
    assert_eq!(completed["result"]["current"]["current"]["commit"], target);
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
    daemon.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"managed-client"}),
    );
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
}

#[test]
fn process_crashes_at_generation_and_view_boundaries_recover_the_exact_pin() {
    for point in [
        "object-intent-saved",
        "object-directory-created",
        "object-directory-sealed",
        "object-guard-created",
        "object-guard-sealed",
        "member-creation-intent-saved",
        "member-created",
        "generation-built",
        "generation-published",
        "checkpoint-published",
        "view-before-commit",
        "view-after-commit",
        "view-after-swap",
    ] {
        let fixture = Fixture::new();
        let configured = policy();
        let mut daemon = start(&fixture, &configured, &["--no-watch"]);
        let (claim, guard) = owner(&daemon);
        let attached = initial_attach(&daemon, &claim, attach_input(&fixture, &claim), point);
        let sequence = next_sequence(&attached);
        let before = daemon.rpc(
            "lookup",
            json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
        );
        fs::write(
            fixture.a.join("notes.txt"),
            "shared_term migration target\n",
        )
        .unwrap();
        git(&fixture.a, &["add", "notes.txt"]);
        git(&fixture.a, &["commit", "-qm", "target"]);
        let target = git(&fixture.a, &["rev-parse", "HEAD"]);
        completed(&daemon, &daemon.rpc("views.refresh", json!({"token":token(&claim,sequence),"request":{
            "view":before["view"],"expected_version":1,"owner":claim.owner,"allocation_version":1
        }})));
        super::live::wait_for(
            &daemon,
            "views.status",
            json!({"id":before["view"]}),
            |status| status["ready"] == true && status["work"].is_null(),
        );
        let migration_token = token(&claim, sequence + 1);
        let hook = pause_for_operation(&daemon, point, &migration_token);
        let operation = daemon.rpc("views.advance", json!({"token":migration_token,"request":{
            "view":before["view"],"root":before["root"],"expected_version":1,"target_commit":target,
            "profile":serde_json::from_str::<Value>(PROFILE).unwrap(),"owner":claim.owner,"allocation_version":1
        }}));
        let barrier = reached(&daemon, &hook);
        assert_eq!(barrier["reached_operation"], operation["id"], "{point}");
        crash(&mut daemon);
        drop(daemon);

        fs::write(
            fixture.a.join("notes.txt"),
            "shared_term committed after crash\n",
        )
        .unwrap();
        git(
            &fixture.a,
            &["commit", "-qam", "later HEAD must not select recovery pin"],
        );
        let mut restarted = start(&fixture, &configured, &["--no-watch"]);
        let recovered = restarted.rpc("views.recover", json!({"id":before["view"]}));
        let committed = matches!(point, "view-after-commit" | "view-after-swap");
        assert_eq!(
            recovered["version"],
            json!(1 + u64::from(committed)),
            "{point}: {recovered}"
        );
        assert_eq!(
            recovered["current"]["commit"],
            if committed {
                json!(target)
            } else {
                json!(fixture.revision)
            }
        );
        let (new_claim, new_guard, _) = reconnect(&fixture, &restarted, &recovered, &claim, point);
        let terminal = terminal(&restarted, &operation);
        assert_eq!(
            terminal["committed_state"],
            if committed {
                "committed"
            } else {
                "not-committed"
            },
            "{point}: {terminal}"
        );
        if committed {
            assert_eq!(
                terminal["result"]["current"]["current"]["commit"],
                recovered["current"]["commit"]
            );
        }
        assert_scan_parity(&restarted, &fixture.a, fixture.temp.path());
        restarted.rpc(
            "views.detach",
            json!({"owner":new_claim.owner,"lease":"reconnected-client"}),
        );
        restarted.rpc("owners.release", json!({"claim":new_claim}));
        drop((guard, new_guard));
        stop(&mut restarted);
    }
}

#[test]
fn collection_crash_boundaries_preserve_live_views_and_resumable_progress() {
    for point in [
        "object-withdrawn",
        "object-pending-deletion",
        "member-intent-saved",
        "member-before-io",
        "member-after-io",
        "member-before-credit",
        "member-after-credit",
        "object-before-remove",
        "object-after-remove",
        "guard-after-remove",
        "object-removed",
    ] {
        let fixture = Fixture::new();
        let mut configured = policy();
        configured["retention"] = json!({"mode":"bounded","target_bytes":1});
        configured["collection"]["max_pages"] = json!(64);
        configured["collection"]["max_duration_ms"] = json!(5000);
        let mut daemon = start(&fixture, &configured, &["--no-watch"]);
        let (claim, guard) = owner(&daemon);
        let attached = initial_attach(&daemon, &claim, attach_input(&fixture, &claim), point);
        let sequence = next_sequence(&attached);
        let current = &attached["result"]["current"];
        let old_checkpoint = current["checkpoint"].clone();
        let namespace = daemon.rpc("namespace.status", json!({}));
        let object_path = fixture
            .storage
            .join(tgrep_core::managed::STORE_DIRECTORY)
            .join(namespace["header"]["repository"].as_str().unwrap())
            .join("objects")
            .join(old_checkpoint.as_str().unwrap());
        assert!(
            object_path.is_dir(),
            "{point}: checkpoint container was not published"
        );
        completed(&daemon, &daemon.rpc("views.refresh", json!({"token":token(&claim,sequence),"request":{
            "view":current["id"],"expected_version":1,"owner":claim.owner,"allocation_version":1
        }})));
        let request = json!({
            "policy_version":1,"allocation_version":1,"cursor":null,
            "bounds":{"max_duration_ms":5000,"max_examined":64,"max_removed":16,"max_delete_bytes":1048576,"max_pages":64}
        });
        let collection_token = token(&claim, sequence + 1);
        let hook = pause_for_operation(&daemon, point, &collection_token);
        let operation = daemon.rpc(
            "collections.start",
            json!({"token":collection_token,"request":request}),
        );
        assert_eq!(
            reached(&daemon, &hook)["reached_operation"],
            operation["id"]
        );
        if point != "member-before-credit" {
            search(&daemon, &fixture.a);
        }
        crash(&mut daemon);
        drop(daemon);

        let mut restarted = start(&fixture, &configured, &["--no-watch"]);
        let recovered = restarted.rpc("views.recover", json!({"id":current["id"]}));
        assert_eq!(recovered["current"], current["current"], "{point}");
        let (new_claim, new_guard, next) =
            reconnect(&fixture, &restarted, &recovered, &claim, point);
        let recovered_operation = terminal(&restarted, &operation);
        assert_eq!(
            recovered_operation["committed_state"], "committed",
            "{point}: {recovered_operation}"
        );
        assert!(
            recovered_operation["result"].is_object(),
            "{point}: {recovered_operation}"
        );
        assert_eq!(recovered_operation["result"]["elapsed_nanos"], Value::Null);
        let mut continuation = recovered_operation["result"]["next"].clone();
        for sequence in next..next + 16 {
            let mut next_request = request.clone();
            next_request["cursor"] = continuation;
            let result = completed(
                &restarted,
                &restarted.rpc(
                    "collections.start",
                    json!({
                        "token":token(&new_claim,sequence),"request":next_request
                    }),
                ),
            );
            continuation = result["result"]["next"].clone();
            if collected(&restarted, &old_checkpoint) {
                break;
            }
        }

        assert!(collected(&restarted, &old_checkpoint), "{point}");
        assert!(
            !object_path.exists(),
            "{point}: collected container still exists"
        );
        assert_scan_parity(&restarted, &fixture.a, fixture.temp.path());
        restarted.rpc(
            "views.detach",
            json!({"owner":new_claim.owner,"lease":"reconnected-client"}),
        );
        restarted.rpc("owners.release", json!({"claim":new_claim}));
        drop((guard, new_guard));
        stop(&mut restarted);
    }
}

fn container_contents(path: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            assert!(
                entry.file_type().unwrap().is_file(),
                "unexpected container entry {:?}",
                entry.path()
            );
            (
                entry.file_name().into_string().unwrap(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn operations_cancel_stops_an_in_flight_collection_without_later_delete_credit() {
    for point in [
        "object-withdrawn",
        "object-pending-deletion",
        "member-intent-saved",
        "member-before-io",
        "collection-before-complete",
    ] {
        let before_io = point != "collection-before-complete";
        let fixture = Fixture::new();
        let mut configured = policy();
        configured["retention"] = json!({"mode":"bounded","target_bytes":1});
        configured["collection"]["max_pages"] = json!(64);
        configured["collection"]["max_duration_ms"] = json!(5000);
        let mut daemon = start(&fixture, &configured, &["--no-watch"]);
        let (claim, guard) = owner(&daemon);
        let attached = initial_attach(&daemon, &claim, attach_input(&fixture, &claim), point);
        let sequence = next_sequence(&attached);
        let current = &attached["result"]["current"];
        let old_checkpoint = current["checkpoint"].clone();
        let namespace = daemon.rpc("namespace.status", json!({}));
        let object_path = fixture
            .storage
            .join(tgrep_core::managed::STORE_DIRECTORY)
            .join(namespace["header"]["repository"].as_str().unwrap())
            .join("objects")
            .join(old_checkpoint.as_str().unwrap());
        completed(&daemon, &daemon.rpc("views.refresh", json!({"token":token(&claim,sequence),"request":{
            "view":current["id"],"expected_version":1,"owner":claim.owner,"allocation_version":1
        }})));
        let original = container_contents(&object_path);
        assert!(!original.is_empty(), "{point}: checkpoint has no members");
        let retained_memory = |usage: &Value| {
            (
                usage["memory"]["retained_allocations"].clone(),
                usage["memory"]["retained_private_estimate_bytes"].clone(),
            )
        };
        let memory_before = retained_memory(&daemon.rpc("namespace.status", json!({}))["usage"]);
        let request = json!({
            "policy_version":1,"allocation_version":1,"cursor":null,
            "bounds":{"max_duration_ms":5000,"max_examined":64,"max_removed":16,"max_delete_bytes":1048576,"max_pages":64}
        });
        let collection_token = token(&claim, sequence + 1);
        let hook = pause_for_operation(&daemon, point, &collection_token);
        let operation = daemon.rpc(
            "collections.start",
            json!({"token":collection_token,"request":request}),
        );
        assert_eq!(
            reached(&daemon, &hook)["reached_operation"],
            operation["id"],
            "{point}"
        );
        let paused = daemon.rpc("operations.inspect", json!({"id":operation["id"]}));
        let cancelling = daemon.rpc("operations.cancel", json!({"id":operation["id"]}));
        // The cancel request must be serviced while the worker is still parked.
        let barrier = daemon.rpc("testing.status", json!({}));
        daemon.rpc("testing.release", json!({"id":hook["ticket"]}));
        assert_eq!(paused["state"], "preparing", "{point}: {paused}");
        assert_eq!(barrier["ticket"], hook["ticket"], "{point}");
        assert_eq!(barrier["stage"], "waiting", "{point}: {barrier}");
        assert_eq!(cancelling["cancelled"], true, "{point}: {cancelling}");
        assert_eq!(cancelling["state"], "cancelling", "{point}: {cancelling}");

        let cancelled = terminal(&daemon, &operation);
        let result = &cancelled["result"];
        assert_eq!(cancelled["state"], "cancelled", "{point}: {cancelled}");
        assert_eq!(cancelled["cancelled"], true, "{point}: {cancelled}");
        assert_eq!(cancelled["progress"], *result, "{point}: {cancelled}");
        assert_eq!(result["cancelled"], true, "{point}: {cancelled}");
        assert_eq!(
            result["verification_context_retained"], false,
            "{point}: {cancelled}"
        );
        if before_io {
            assert_eq!(
                cancelled["committed_state"], "committed",
                "{point}: {cancelled}"
            );
            assert!(
                result["retired"].as_u64().unwrap() >= 1,
                "{point}: {cancelled}"
            );
            for field in [
                "removed",
                "logical_bytes_reclaimed",
                "recovered_logical_bytes",
            ] {
                assert_eq!(result[field], 0, "{point} {field}: {cancelled}");
            }
            assert!(result["next"].is_object(), "{point}: {cancelled}");
            assert!(!collected(&daemon, &old_checkpoint), "{point}");
            assert_eq!(
                container_contents(&object_path),
                original,
                "{point}: cancelled collection changed checkpoint members"
            );
        } else {
            for field in [
                "removed",
                "logical_bytes_reclaimed",
                "recovered_logical_bytes",
            ] {
                assert_eq!(
                    result[field], paused["progress"][field],
                    "{point} {field}: {cancelled}; paused: {paused}"
                );
            }
        }
        let maintenance =
            super::live::wait_for(&daemon, "maintenance.status", json!({}), |status| {
                let state = &status["details"]["verification"]["state"];
                assert_ne!(state, "cached", "{point}: {status}");
                state == "empty"
            });
        let verification = &maintenance["details"]["verification"];
        assert_eq!(
            verification["retained_contexts"],
            json!({"status":"observed","value":0}),
            "{point}: {maintenance}"
        );
        assert_eq!(
            verification["retained_capacity_bytes"], 0,
            "{point}: {maintenance}"
        );
        let usage = daemon.rpc("namespace.status", json!({}))["usage"].clone();
        assert_eq!(
            retained_memory(&usage),
            memory_before,
            "{point}: cancelled collection retained memory: {usage}"
        );

        let replay = daemon.rpc(
            "collections.start",
            json!({"token":collection_token,"request":request}),
        );
        assert_eq!(replay["id"], operation["id"], "{point}: {replay}");
        assert_eq!(
            terminal(&daemon, &replay)["result"],
            *result,
            "{point}: replay"
        );

        let mut continuation = result["next"].clone();
        let mut next = sequence + 2;
        while !collected(&daemon, &old_checkpoint) {
            assert!(
                next < sequence + 18,
                "{point}: cancelled collection did not resume to completion"
            );
            let mut next_request = request.clone();
            next_request["cursor"] = continuation;
            let resumed = completed(
                &daemon,
                &daemon.rpc(
                    "collections.start",
                    json!({"token":token(&claim,next),"request":next_request}),
                ),
            );
            assert_eq!(resumed["result"]["cancelled"], false, "{point}: {resumed}");
            continuation = resumed["result"]["next"].clone();
            next += 1;
        }
        assert!(
            !object_path.exists(),
            "{point}: collected container still exists"
        );
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
fn owner_release_drains_roots_while_unrelated_control_cleanup_is_paused() {
    let fixture = Fixture::new();
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    let mut request = attach_input(&fixture, &claim);
    request["request"]["root"] = json!(fs::canonicalize(&fixture.b).unwrap());
    let attached = initial_attach(&daemon, &claim, request, "root-drain-setup");
    let current = &attached["result"]["current"];
    super::live::wait_for(
        &daemon,
        "views.status",
        json!({"id":current["id"]}),
        |status| status["ready"] == true && status["work"].is_null(),
    );
    let namespace = fixture
        .storage
        .join(tgrep_core::managed::STORE_DIRECTORY)
        .join(daemon.marker["repository"].as_str().unwrap());
    let root_guard = namespace.join("guards").join(format!(
        "root-{}.lock",
        current["root_anchor"]["guard"].as_str().unwrap()
    ));
    assert!(root_guard.is_file());
    let (ended, ended_guard) = owner(&daemon);
    let ended_proof = namespace
        .join("owners")
        .join(format!("{}.json", ended.owner));
    let hook = pause(&daemon, "control-intent-saved");
    daemon.rpc("owners.release", json!({"claim":ended}));
    drop(ended_guard);
    reached(&daemon, &hook);
    assert_scan_parity(&daemon, &fixture.b, fixture.temp.path());

    let released = daemon
        .try_rpc("owners.release", json!({"claim":claim}))
        .unwrap();
    let replay = daemon
        .try_rpc("owners.release", json!({"claim":claim}))
        .unwrap();
    let owner_state = daemon.rpc("owners.inspect", json!({"id":claim.owner}));
    let view_state = daemon.rpc("views.recover", json!({"id":current["id"]}));
    let control_retained = root_guard.is_file();
    let root_released = fs::rename(&fixture.b, fixture.temp.path().join("released-worktree"));
    let paused = daemon.rpc("testing.status", json!({}));
    daemon.rpc("testing.release", json!({"id":hook["ticket"]}));

    assert!(released.get("error").is_none(), "{released}");
    assert_eq!(released["result"]["data"]["owner"]["released"], true);
    assert_eq!(released["result"]["data"]["leases_released"], 1);
    assert!(replay.get("error").is_none(), "{replay}");
    assert_eq!(replay["result"]["data"]["leases_released"], 0);
    assert_eq!(owner_state["released"], true);
    assert_eq!(view_state["active"], false);
    assert!(
        control_retained,
        "root draining synchronously unlinked its guard"
    );
    assert!(root_released.is_ok(), "{root_released:?}");
    assert_eq!(paused["ticket"], hook["ticket"]);
    assert_eq!(paused["stage"], "waiting");

    let started = Instant::now();
    while root_guard.exists() || ended_proof.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "retired controls were not reclaimed: {}",
            daemon.rpc("maintenance.status", json!({}))
        );
        thread::sleep(Duration::from_millis(10));
    }
    let retained = fs::metadata(
        namespace
            .join("owners")
            .join(format!("{}.json", claim.owner)),
    )
    .unwrap()
    .len();
    assert_eq!(
        daemon.rpc("namespace.status", json!({}))["usage"]["control_logical_bytes"],
        retained
    );
    drop(guard);
    stop(&mut daemon);
}

#[test]
fn control_unlink_crashes_preserve_sibling_proofs_and_credit_only_the_ended_owner() {
    for point in [
        "control-intent-saved",
        "control-after-remove",
        "control-before-credit",
    ] {
        let fixture = Fixture::new();
        let configured = policy();
        let mut daemon = start(&fixture, &configured, &["--no-watch"]);
        let (claim, guard) = owner(&daemon);
        let attached = initial_attach(&daemon, &claim, attach_input(&fixture, &claim), point);
        let current = &attached["result"]["current"];
        let (ended, ended_guard) = owner(&daemon);
        let namespace = fixture
            .storage
            .join(tgrep_core::managed::STORE_DIRECTORY)
            .join(daemon.marker["repository"].as_str().unwrap());
        let controls = namespace.join("owners");
        let ended_proof = controls.join(format!("{}.json", ended.owner));
        let before = fs::metadata(&ended_proof).unwrap().len();
        let usage =
            daemon.rpc("namespace.status", json!({}))["usage"]["control_logical_bytes"].clone();
        let hook = pause(&daemon, point);
        daemon.rpc("owners.release", json!({"claim":ended}));
        drop(ended_guard);
        reached(&daemon, &hook);
        assert_eq!(
            ended_proof.exists(),
            point == "control-intent-saved",
            "{point}"
        );
        assert_eq!(
            daemon.rpc("namespace.status", json!({}))["usage"]["control_logical_bytes"],
            usage
        );
        search(&daemon, &fixture.a);
        crash(&mut daemon);
        drop(daemon);

        let mut restarted = start(&fixture, &configured, &["--no-watch"]);
        let recovered = restarted.rpc("views.recover", json!({"id":current["id"]}));
        assert_eq!(recovered["current"], current["current"]);
        let (new_claim, new_guard, _) = reconnect(&fixture, &restarted, &recovered, &claim, point);
        let started = Instant::now();
        loop {
            let result = restarted
                .try_rpc("owners.inspect", json!({"id":ended.owner}))
                .unwrap();
            if result.get("error").is_some() {
                assert_eq!(
                    result["error"]["data"]["category"], "receipt-expired",
                    "{point}: {result}"
                );
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "{point}: ended owner not cleaned: {result}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!ended_proof.exists());
        assert!(!controls.join(format!("{}.lock", ended.owner)).exists());
        let retained = fs::metadata(controls.join(format!("{}.json", claim.owner)))
            .unwrap()
            .len()
            + fs::metadata(controls.join(format!("{}.json", new_claim.owner)))
                .unwrap()
                .len();
        assert!(before > 0);
        assert_eq!(
            restarted.rpc("namespace.status", json!({}))["usage"]["control_logical_bytes"],
            retained
        );
        for _ in 0..2 {
            let mut cursor = Value::Null;
            for pass in 0..64 {
                let mut request = idle_request(&restarted);
                request["request"] = json!({"cursor":cursor});
                let recovered = retry_busy(&restarted, "maintenance.recover", request);
                assert_eq!(recovered["state"], "completed");
                assert_eq!(
                    recovered["result"]["cleanup"]["control_logical_bytes_reclaimed"],
                    0
                );
                assert_eq!(
                    recovered["result"]["cleanup"]["control_recovered_logical_bytes"],
                    0
                );
                cursor = recovered["result"]["next"].clone();
                if cursor.is_null() {
                    break;
                }
                assert!(pass < 63, "recovery did not finish a bounded traversal");
            }
        }
        assert_eq!(
            restarted.rpc("namespace.status", json!({}))["usage"]["control_logical_bytes"],
            retained
        );
        assert_scan_parity(&restarted, &fixture.a, fixture.temp.path());
        restarted.rpc(
            "views.detach",
            json!({"owner":new_claim.owner,"lease":"reconnected-client"}),
        );
        restarted.rpc("owners.release", json!({"claim":new_claim}));
        drop((guard, new_guard));
        stop(&mut restarted);
    }
}
