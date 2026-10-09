// Copyright (c) Microsoft Corporation. All rights reserved.

use super::clients::{held_owner, manage};
use super::faults::{pause, pause_for_operation, reached};
use super::*;

fn advance(claim: &OwnerClaim, sequence: u64, view: &Value, target: &str) -> Value {
    json!({"token":token(claim,sequence),"request":{
        "root":view["root"],"view":view["view"],"expected_version":view["version"],
        "target_commit":target,"profile":serde_json::from_str::<Value>(PROFILE).unwrap(),
        "owner":claim.owner,"allocation_version":1
    }})
}

fn refresh(daemon: &Daemon, claim: &OwnerClaim, sequence: u64, view: &Value) {
    daemon.rpc(
        "views.invalidate",
        json!({"view":view["view"],"expected_version":view["version"],
        "owner":claim.owner,"changed":[],"full":true}),
    );
    completed(daemon, &daemon.rpc("views.refresh", json!({"token":token(claim,sequence),"request":{
        "view":view["view"],"expected_version":view["version"],"owner":claim.owner,"allocation_version":1
    }})));
}

fn release(daemon: &Daemon, hook: &Value) {
    daemon.rpc("testing.release", json!({"id":hook["ticket"]}));
}

fn published_generations(daemon: &Daemon) -> Vec<Value> {
    let mut cursor = Value::Null;
    let mut generations = Vec::new();
    for _ in 0..64 {
        let page = daemon.rpc("objects.page", json!({"cursor":cursor}));
        generations.extend(
            page["objects"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|object| object["kind"] == "generation" && object["state"] == "published")
                .map(|object| {
                    assert!(object["id"].is_string(), "{object}");
                    object["id"].clone()
                }),
        );
        cursor = page["next"].clone();
        if cursor.is_null() {
            return generations;
        }
    }
    panic!("catalog traversal did not converge");
}

pub(super) fn wait_for(
    daemon: &Daemon,
    method: &str,
    params: Value,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let started = Instant::now();
    loop {
        let result = daemon.rpc(method, params.clone());
        if predicate(&result) {
            return result;
        }
        assert!(
            method != "operations.inspect"
                || !matches!(
                    result["state"].as_str(),
                    Some("completed" | "failed" | "cancelled")
                ),
            "operation ended before the required boundary: {result}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{method} did not converge: {result}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn ready_views_and_migration_continue_while_a_catalog_reader_pins_the_wal() {
    let fixture = Fixture::new();
    let configured = policy();
    let mut daemon = start(&fixture, &configured, &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    let attached = completed(
        &daemon,
        &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
    );
    let id = attached["result"]["current"]["id"].clone();
    wait_for(&daemon, "views.status", json!({"id":id}), |status| {
        status["ready"] == true && status["work"].is_null()
    });
    let mut sibling = attach_input(&fixture, &claim);
    sibling["token"] = token(&claim, 2);
    sibling["request"]["root"] = json!(fs::canonicalize(&fixture.b).unwrap());
    sibling["request"]["lease"] = json!("sibling-client");
    let sibling = completed(&daemon, &daemon.rpc("views.attach", sibling));
    let sibling_id = sibling["result"]["current"]["id"].clone();
    wait_for(
        &daemon,
        "views.status",
        json!({"id":sibling_id}),
        |status| status["ready"] == true && status["work"].is_null(),
    );
    let namespace = PathBuf::from(daemon.marker["storage"].as_str().unwrap());
    let reader = rusqlite::Connection::open_with_flags(
        namespace.join("catalog.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader
        .execute_batch("BEGIN; SELECT namespace FROM state")
        .unwrap();
    let wal_bytes = || {
        fs::metadata(namespace.join("catalog.sqlite-wal"))
            .unwrap()
            .len()
    };
    let limit = configured["work"]["metadata_bytes"].as_u64().unwrap();
    let old_gate = limit / 4;
    let mut crossed = false;
    for _ in 0..1024 {
        search(&daemon, &fixture.a);
        if wal_bytes() > old_gate + 65536 {
            crossed = true;
            break;
        }
    }
    assert!(crossed, "fixture did not reach the old truncation gate");
    assert!(!reader.is_autocommit());
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
    let view = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    fs::write(
        fixture.a.join("notes.txt"),
        "shared_term pinned-WAL target\n",
    )
    .unwrap();
    git(
        &fixture.a,
        &["commit", "-qam", "target during retained catalog snapshot"],
    );
    let target = git(&fixture.a, &["rev-parse", "HEAD"]);
    refresh(&daemon, &claim, 3, &view);
    wait_for(&daemon, "views.status", json!({"id":id}), |status| {
        status["ready"] == true && status["work"].is_null()
    });
    let input = advance(&claim, 4, &view, &target);
    let hook = pause_for_operation(&daemon, "generation-built", &input["token"]);
    let operation = daemon.rpc("views.advance", input);
    reached(&daemon, &hook);
    assert_scan_parity(&daemon, &fixture.b, fixture.temp.path());
    release(&daemon, &hook);
    let migrated = completed(&daemon, &operation);
    assert_eq!(migrated["result"]["current"]["version"], 2);
    assert_eq!(migrated["result"]["current"]["current"]["commit"], target);
    wait_for(&daemon, "views.status", json!({"id":id}), |status| {
        status["ready"] == true && status["work"].is_null()
    });
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
    assert_scan_parity(&daemon, &fixture.b, fixture.temp.path());
    assert!(!reader.is_autocommit());
    assert!(wal_bytes() < limit);
    let diagnostics = daemon.rpc("maintenance.status", json!({}));
    assert_eq!(
        diagnostics["details"]["last_catalog_write"]["checkpoint"]["busy"],
        true
    );
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    for _ in 0..32 {
        search(&daemon, &fixture.a);
        if wal_bytes() < old_gate {
            break;
        }
    }
    assert!(wal_bytes() < old_gate, "WAL reclamation did not resume");
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
fn concurrent_clients_share_one_new_publication_with_distinct_exact_commits_and_private_views() {
    let fixture = Fixture::new();
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (claim, mut holder) = held_owner(&fixture, &daemon);
    completed(
        &daemon,
        &manage(&fixture.a, "views.attach", attach_input(&fixture, &claim)),
    );
    let original = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    fs::write(
        fixture.b.join("notes.txt"),
        "shared_term newer committed tree\n",
    )
    .unwrap();
    git(&fixture.b, &["commit", "-qam", "new tree"]);
    let newer = git(&fixture.b, &["rev-parse", "HEAD"]);
    let third = fixture.third(&newer);
    git(
        &third,
        &[
            "commit",
            "-qm",
            "same tree, distinct commit",
            "--allow-empty",
        ],
    );
    let same_tree = git(&third, &["rev-parse", "HEAD"]);
    assert_ne!(newer, same_tree);
    fs::write(
        fixture.b.join("notes.txt"),
        "shared_term private second view\n",
    )
    .unwrap();
    fs::write(third.join("notes.txt"), "shared_term private third view\n").unwrap();
    fs::write(third.join(".ignore"), ".hidden\n").unwrap();
    let mut input = attach_input(&fixture, &claim);
    input["token"] = token(&claim, 2);
    input["request"]["root"] = json!(fs::canonicalize(&fixture.b).unwrap());
    input["request"]["revision"] = json!(newer);
    input["request"]["lease"] = json!("newer-view");
    let hook = pause_for_operation(&daemon, "generation-built", &input["token"]);
    let second = manage(&fixture.a, "views.attach", input.clone());
    assert_eq!(reached(&daemon, &hook)["reached_operation"], second["id"]);
    assert_eq!(
        search(&daemon, &fixture.a)["incarnation"],
        original["incarnation"]
    );
    let preparing = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.b).unwrap()}),
    );
    assert_eq!(preparing["ready"], false);
    assert_eq!(preparing["commit"], newer);
    assert!(preparing["incarnation"].is_null());
    input["token"] = token(&claim, 3);
    input["request"]["root"] = json!(fs::canonicalize(&third).unwrap());
    input["request"]["revision"] = json!(same_tree);
    input["request"]["lease"] = json!("same-tree-view");
    let other = manage(&fixture.a, "views.attach", input);
    wait_for(
        &daemon,
        "operations.inspect",
        json!({"id":other["id"]}),
        |record| record["progress"]["waiting_reason"] == "generation-already-materializing",
    );
    release(&daemon, &hook);
    let built = completed(&daemon, &second);
    let reused = completed(&daemon, &other);
    assert_eq!(built["result"]["build"]["published"], true, "{built}");
    assert_eq!(
        reused["result"]["build"]["reused_generation"], true,
        "{reused}"
    );
    assert_eq!(reused["result"]["build"]["published"], false, "{reused}");
    assert_eq!(reused["result"]["build"]["blobs_read"], 0, "{reused}");
    let second = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.b).unwrap()}),
    );
    let other = daemon.rpc("lookup", json!({"root":fs::canonicalize(&third).unwrap()}));
    assert_eq!(second["incarnation"], other["incarnation"]);
    assert_ne!(second["incarnation"], original["incarnation"]);
    assert_eq!(second["commit"], newer);
    assert_eq!(other["commit"], same_tree);
    assert_eq!(published_generations(&daemon).len(), 2);
    for root in [&fixture.a, &fixture.b, &third] {
        assert_scan_parity(&daemon, root, fixture.temp.path());
    }
    let second_matches = search(&daemon, &fixture.b).to_string();
    let third_matches = search(&daemon, &third).to_string();
    assert!(
        second_matches.contains("private second view")
            && !second_matches.contains("private third view")
    );
    assert!(
        third_matches.contains("private third view")
            && !third_matches.contains("private second view")
    );
    for lease in ["managed-client", "newer-view", "same-tree-view"] {
        daemon.rpc("views.detach", json!({"owner":claim.owner,"lease":lease}));
    }
    daemon.rpc("owners.release", json!({"claim":claim}));
    holder.close_input();
    success(holder.finish(Duration::from_secs(30)).unwrap());
    stop(&mut daemon);
}

#[test]
fn conflicting_process_migrations_commit_once_and_replay_each_original_receipt() {
    let fixture = Fixture::new();
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (first, mut first_holder) = held_owner(&fixture, &daemon);
    completed(
        &daemon,
        &manage(&fixture.a, "views.attach", attach_input(&fixture, &first)),
    );
    let view = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    let (second, mut second_holder) = held_owner(&fixture, &daemon);
    let mut join = attach_input(&fixture, &second);
    join["request"]["revision"] = Value::Null;
    join["request"]["accept_current"] = json!({"view":view["view"],"version":1});
    completed(&daemon, &manage(&fixture.a, "views.attach", join));
    fs::write(
        fixture.a.join("notes.txt"),
        "shared_term first exact target\n",
    )
    .unwrap();
    git(&fixture.a, &["commit", "-qam", "first target"]);
    let target = git(&fixture.a, &["rev-parse", "HEAD"]);
    fs::write(
        fixture.a.join("notes.txt"),
        "shared_term conflicting later target\n",
    )
    .unwrap();
    git(&fixture.a, &["commit", "-qam", "second target"]);
    let later = git(&fixture.a, &["rev-parse", "HEAD"]);
    refresh(&daemon, &first, 2, &view);
    let winner_input = advance(&first, 3, &view, &target);
    let hook = pause_for_operation(&daemon, "migration-prepared", &winner_input["token"]);
    let winner = manage(&fixture.a, "views.advance", winner_input.clone());
    assert_eq!(reached(&daemon, &hook)["reached_operation"], winner["id"]);
    let loser_input = advance(&second, 2, &view, &later);
    let loser = manage(&fixture.a, "views.advance", loser_input.clone());
    wait_for(
        &daemon,
        "operations.inspect",
        json!({"id":loser["id"]}),
        |record| record["progress"]["waiting_reason"] == "view-work-active",
    );
    assert_eq!(search(&daemon, &fixture.a)["version"], 1);
    release(&daemon, &hook);
    let won = completed(&daemon, &winner);
    let lost = terminal(&daemon, &loser);
    assert_eq!(won["result"]["current"]["current"]["commit"], target);
    assert_eq!(won["result"]["current"]["version"], 2);
    assert_eq!(lost["error"]["category"], "stale-version", "{lost}");
    assert_eq!(lost["committed_state"], "not-committed", "{lost}");
    assert_eq!(
        manage(&fixture.a, "views.advance", winner_input)["id"],
        winner["id"]
    );
    assert_eq!(
        manage(&fixture.a, "views.advance", loser_input)["id"],
        loser["id"]
    );
    assert_eq!(
        daemon.rpc("views.recover", json!({"id":view["view"]}))["current"]["commit"],
        target
    );
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
    for claim in [&first, &second] {
        daemon.rpc(
            "views.detach",
            json!({"owner":claim.owner,"lease":"managed-client"}),
        );
        daemon.rpc("owners.release", json!({"claim":claim}));
    }
    for holder in [&mut first_holder, &mut second_holder] {
        holder.close_input();
        success(holder.finish(Duration::from_secs(30)).unwrap());
    }
    stop(&mut daemon);
}

#[test]
fn preparation_invalidations_close_readiness_without_mixing_or_disrupting_a_sibling() {
    let fixture = Fixture::new();
    let mut daemon = start(&fixture, &policy(), &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    completed(
        &daemon,
        &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
    );
    let mut sibling = attach_input(&fixture, &claim);
    sibling["token"] = token(&claim, 2);
    sibling["request"]["root"] = json!(fs::canonicalize(&fixture.b).unwrap());
    sibling["request"]["lease"] = json!("sibling-view");
    completed(&daemon, &daemon.rpc("views.attach", sibling));
    let view = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    fs::write(
        fixture.a.join("notes.txt"),
        "shared_term target before private edits\n",
    )
    .unwrap();
    git(&fixture.a, &["commit", "-qam", "target"]);
    let target = git(&fixture.a, &["rev-parse", "HEAD"]);
    refresh(&daemon, &claim, 3, &view);
    let input = advance(&claim, 4, &view, &target);
    let hook = pause_for_operation(&daemon, "migration-prepared", &input["token"]);
    let operation = daemon.rpc("views.advance", input);
    reached(&daemon, &hook);
    assert_eq!(search(&daemon, &fixture.a)["version"], 1);
    fs::rename(fixture.a.join("notes.txt"), fixture.a.join("renamed.txt")).unwrap();
    fs::remove_file(fixture.a.join("src").join("main.rs")).unwrap();
    fs::write(fixture.a.join(".ignore"), ".hidden\n").unwrap();
    fs::write(
        fixture.a.join("new.txt"),
        "shared_term added while preparing\n",
    )
    .unwrap();
    daemon.rpc(
        "views.invalidate",
        json!({"view":view["view"],"expected_version":1,
        "owner":claim.owner,"changed":[],"full":true}),
    );
    assert_eq!(
        daemon.rpc("views.status", json!({"id":view["view"]}))["ready"],
        false
    );
    assert_eq!(search(&daemon, &fixture.b)["version"], 1);
    release(&daemon, &hook);
    let failed = terminal(&daemon, &operation);
    assert_eq!(
        failed["error"]["reason_code"], "publication-epoch-changed",
        "{failed}"
    );
    assert_eq!(failed["committed_state"], "not-committed");
    assert_eq!(
        daemon.rpc("views.recover", json!({"id":view["view"]}))["version"],
        1
    );
    completed(
        &daemon,
        &daemon.rpc(
            "views.refresh",
            json!({"token":token(&claim,5),"request":{
                "view":view["view"],"expected_version":1,"owner":claim.owner,"allocation_version":1
            }}),
        ),
    );
    completed(
        &daemon,
        &daemon.rpc("views.advance", advance(&claim, 6, &view, &target)),
    );
    let current = daemon.rpc("views.status", json!({"id":view["view"]}));
    assert_eq!(current["ready"], true);
    assert_eq!(current["authoritative"]["version"], 2);
    assert_eq!(current["authoritative"]["current"]["commit"], target);
    assert_eq!(
        current["authoritative"]["input_epoch"],
        current["input_epoch"]
    );
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
    assert_scan_parity(&daemon, &fixture.b, fixture.temp.path());
    for lease in ["managed-client", "sibling-view"] {
        daemon.rpc("views.detach", json!({"owner":claim.owner,"lease":lease}));
    }
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
}

#[test]
fn automatic_advancement_recovers_after_a_late_fixed_participant_and_avoids_ineffective_trees() {
    let fixture = Fixture::new();
    let mut configured = policy();
    configured["advancement"] = json!({"mode":"adaptive","high_bytes":1,"low_bytes":0,
        "min_reduction_bytes":1,"min_reduction_percent":1,"cooldown_ms":1,
        "max_paths":32,"max_read_bytes":1048576,"max_attempts":1});
    let mut daemon = start(&fixture, &configured, &["--no-watch"]);
    let (claim, guard) = owner(&daemon);
    completed(
        &daemon,
        &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
    );
    let view = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    let hook = pause(&daemon, "migration-prepared");
    fs::write(
        fixture.a.join("notes.txt"),
        "shared_term beneficial automatic target\n",
    )
    .unwrap();
    git(&fixture.a, &["commit", "-qam", "beneficial target"]);
    let target = git(&fixture.a, &["rev-parse", "HEAD"]);
    refresh(&daemon, &claim, 2, &view);
    let barrier = reached(&daemon, &hook);
    let (fixed, fixed_guard) = owner(&daemon);
    let mut join = attach_input(&fixture, &fixed);
    join["request"]["revision"] = Value::Null;
    join["request"]["accept_current"] = json!({"view":view["view"],"version":1});
    join["request"]["migratable"] = json!(false);
    completed(&daemon, &daemon.rpc("views.attach", join));
    release(&daemon, &hook);
    let blocked = terminal(&daemon, &json!({"id":barrier["reached_operation"]}));
    assert_eq!(
        blocked["error"]["reason_code"], "fixed-pin-participant",
        "{blocked}"
    );
    assert_eq!(blocked["committed_state"], "not-committed");
    assert_eq!(search(&daemon, &fixture.a)["version"], 1);
    daemon.rpc(
        "views.detach",
        json!({"owner":fixed.owner,"lease":"managed-client"}),
    );
    daemon.rpc("owners.release", json!({"claim":fixed}));
    drop(fixed_guard);
    let current = wait_for(
        &daemon,
        "views.status",
        json!({"id":view["view"]}),
        |status| status["ready"] == true && status["authoritative"]["version"] == 2,
    );
    assert_eq!(current["authoritative"]["current"]["commit"], target);
    let publications = published_generations(&daemon);
    assert_eq!(publications.len(), 2);
    let mut view = view;
    view["version"] = json!(2);
    fs::write(
        fixture.a.join("untracked.txt"),
        format!("shared_term {}\n", "x".repeat(4096)),
    )
    .unwrap();
    refresh(&daemon, &claim, 3, &view);
    let query = json!({"root":view["root"],"view":view["view"],"expected_version":2,"query":{}});
    wait_for(&daemon, "status", query.clone(), |status| {
        status["adaptive"]["source_version"] == 2
            && status["adaptive"]["reason_code"] == "current-generation-key"
    });
    fs::write(
        fixture.a.join("notes.txt"),
        "shared_term untransformed blob\n",
    )
    .unwrap();
    git(&fixture.a, &["commit", "-qam", "transformed target"]);
    let transformed = git(&fixture.a, &["rev-parse", "HEAD"]);
    fs::write(
        fixture.a.join("notes.txt"),
        "shared_term untransformed blob\r\n",
    )
    .unwrap();
    refresh(&daemon, &claim, 4, &view);
    let decision = wait_for(&daemon, "status", query.clone(), |status| {
        status["adaptive"]["target_commit"] == transformed
    });
    assert_eq!(decision["adaptive"]["eligible"], false, "{decision}");
    assert_eq!(
        decision["adaptive"]["expected_reduction_bytes"], 0,
        "{decision}"
    );
    git(
        &fixture.a,
        &["commit", "-qm", "same ineffective tree", "--allow-empty"],
    );
    let same_tree = git(&fixture.a, &["rev-parse", "HEAD"]);
    refresh(&daemon, &claim, 5, &view);
    let decision = wait_for(&daemon, "status", query, |status| {
        status["adaptive"]["target_commit"] == same_tree
    });
    assert_eq!(
        decision["adaptive"]["reason_code"], "unchanged-tracked-evidence",
        "{decision}"
    );
    assert_eq!(
        decision["adaptive"]["target_blob_bytes_read"], 0,
        "{decision}"
    );
    assert_eq!(decision["version"], 2);
    assert_eq!(published_generations(&daemon), publications);
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
fn independent_repository_allocations_and_offline_collection_preserve_a_live_sibling_namespace() {
    let first = Fixture::new();
    let mut second = Fixture::new();
    second.storage = first.storage.clone();
    for (fixture, label) in [(&first, "first-repository"), (&second, "second-repository")] {
        fs::write(
            fixture.a.join("notes.txt"),
            format!("shared_term {label}\n"),
        )
        .unwrap();
        git(
            &fixture.a,
            &["commit", "-qam", "newer than the pinned base"],
        );
        fs::write(
            fixture.b.join("notes.txt"),
            format!("shared_term {label} linked\n"),
        )
        .unwrap();
    }
    let mut configured = policy();
    configured["retention"] = json!({"mode":"bounded","target_bytes":1});
    let mut a = start(&first, &configured, &["--no-watch"]);
    let mut b = start(&second, &configured, &["--no-watch"]);
    assert_ne!(a.marker["namespace"], b.marker["namespace"]);
    assert_ne!(a.marker["instance"], b.marker["instance"]);
    let (owner_a, guard_a) = owner(&a);
    let (owner_b, guard_b) = owner(&b);
    for (fixture, daemon, claim) in [(&first, &a, &owner_a), (&second, &b, &owner_b)] {
        completed(
            daemon,
            &daemon.rpc("views.attach", attach_input(fixture, claim)),
        );
        let mut input = attach_input(fixture, claim);
        input["token"] = token(claim, 2);
        input["request"]["root"] = json!(fs::canonicalize(&fixture.b).unwrap());
        input["request"]["lease"] = json!("linked");
        completed(daemon, &daemon.rpc("views.attach", input));
        let primary = daemon.rpc(
            "lookup",
            json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
        );
        let linked = daemon.rpc(
            "lookup",
            json!({"root":fs::canonicalize(&fixture.b).unwrap()}),
        );
        assert_eq!(primary["incarnation"], linked["incarnation"]);
        assert_eq!(published_generations(daemon).len(), 1);
    }
    let match_a = search(&a, &first.a).to_string();
    let match_b = search(&b, &second.a).to_string();
    assert!(match_a.contains("first-repository") && !match_a.contains("second-repository"));
    assert!(match_b.contains("second-repository") && !match_b.contains("first-repository"));
    let status_b = b.rpc("namespace.status", json!({}));
    let mut allocation = a.rpc("namespace.status", json!({}))["allocation"].clone();
    allocation["coordinator"] = json!(owner_a.owner);
    allocation["staging_bytes"] = json!(1);
    allocation["storage_bytes"] = json!(1);
    completed(
        &a,
        &a.rpc(
            "metadata.start",
            json!({"token":token(&owner_a,3),"request":{
                "method":"allocation","params":{"expected_version":1,"allocation":allocation}
            }}),
        ),
    );
    let view = a.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&first.a).unwrap()}),
    );
    let target = git(&first.a, &["rev-parse", "HEAD"]);
    let stale = terminal(
        &a,
        &a.rpc("views.advance", advance(&owner_a, 4, &view, &target)),
    );
    assert_eq!(stale["error"]["category"], "stale-version", "{stale}");
    let mut request = advance(&owner_a, 5, &view, &target);
    request["request"]["allocation_version"] = json!(2);
    let pressure = terminal(&a, &a.rpc("views.advance", request));
    assert_eq!(
        pressure["error"]["category"], "resource-pressure",
        "{pressure}"
    );
    assert_eq!(pressure["committed_state"], "not-committed");
    assert_eq!(
        a.rpc("views.recover", json!({"id":view["view"]}))["version"],
        1
    );
    assert_eq!(
        b.rpc("namespace.status", json!({}))["allocation"],
        status_b["allocation"]
    );
    for (fixture, daemon) in [(&first, &a), (&second, &b)] {
        assert_scan_parity(daemon, &fixture.a, fixture.temp.path());
        assert_scan_parity(daemon, &fixture.b, fixture.temp.path());
    }
    let namespace = a.marker["storage"].as_str().unwrap().to_owned();
    let unknown = Path::new(&namespace).join("foreign-cache");
    fs::create_dir(&unknown).unwrap();
    fs::write(unknown.join("keep.txt"), b"not a catalog-owned object").unwrap();
    for lease in ["managed-client", "linked"] {
        a.rpc("views.detach", json!({"owner":owner_a.owner,"lease":lease}));
    }
    a.rpc("owners.release", json!({"claim":owner_a}));
    drop(guard_a);
    stop(&mut a);
    let mut cursor = Value::Null;
    for sequence in 10000..10064 {
        let result = offline(
            &first,
            &namespace,
            "collections.run",
            json!({
                "token":{"scope":a.marker["namespace"],"sequence":sequence,"token":format!("offline-{sequence}")},
                "request":{"policy_version":1,"allocation_version":2,"cursor":cursor,
                    "bounds":{"max_duration_ms":1000,"max_examined":64,"max_removed":16,
                        "max_delete_bytes":1048576,"max_pages":4}}
            }),
            true,
        );
        assert_eq!(result["progress"]["errors"], 0, "{result}");
        cursor = result["progress"]["next"].clone();
        if cursor.is_null() {
            break;
        }
        assert!(sequence < 10063, "offline traversal did not converge");
    }
    let after_a = offline(&first, &namespace, "namespace.status", json!({}), false);
    assert_eq!(after_a["usage"]["object_logical_bytes"], 0, "{after_a}");
    assert_eq!(
        fs::read(unknown.join("keep.txt")).unwrap(),
        b"not a catalog-owned object"
    );
    assert_eq!(
        b.rpc("namespace.status", json!({}))["usage"]["object_logical_bytes"],
        status_b["usage"]["object_logical_bytes"]
    );
    assert_scan_parity(&b, &second.a, second.temp.path());
    for lease in ["managed-client", "linked"] {
        b.rpc("views.detach", json!({"owner":owner_b.owner,"lease":lease}));
    }
    b.rpc("owners.release", json!({"claim":owner_b}));
    drop(guard_b);
    stop(&mut b);
}

#[test]
fn native_watching_smudge_crlf_sparse_and_rescan_repair_preserve_indexed_scan_parity() {
    let mut fixture = Fixture::new();
    fs::write(
        fixture.a.join(".gitattributes"),
        "notes.txt text eol=crlf ident\n",
    )
    .unwrap();
    fs::write(fixture.a.join("notes.txt"), "$Id$\nshared_term notes\n").unwrap();
    git(&fixture.a, &["add", "."]);
    git(
        &fixture.a,
        &["commit", "-qm", "native checkout transformations"],
    );
    fixture.revision = git(&fixture.a, &["rev-parse", "HEAD"]);
    fs::remove_file(fixture.a.join("notes.txt")).unwrap();
    git(
        &fixture.a,
        &["checkout-index", "--index", "--force", "notes.txt"],
    );
    let checkout = fs::read_to_string(fixture.a.join("notes.txt")).unwrap();
    assert!(
        checkout.contains("$Id: ") && checkout.contains("\r\n"),
        "{checkout:?}"
    );
    let raw = git(&fixture.a, &["show", "HEAD:notes.txt"]);
    assert!(raw.contains("$Id$") && !raw.contains("$Id: "));
    let mut daemon = start(
        &fixture,
        &policy(),
        &["--watch-mode", "auto", "--poll-interval", "60"],
    );
    let (claim, guard) = owner(&daemon);
    completed(
        &daemon,
        &daemon.rpc("views.attach", attach_input(&fixture, &claim)),
    );
    let view = daemon.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&fixture.a).unwrap()}),
    );
    let query = json!({"root":view["root"],"view":view["view"],"expected_version":1,"query":{}});
    wait_for(&daemon, "status", query.clone(), |status| {
        status["watch_mode"] == "native"
    });
    let private = fixture.a.join("private.txt");
    fs::write(&private, "shared_term native_live\n").unwrap();
    let started = Instant::now();
    loop {
        let result = daemon
            .try_rpc(
                "search",
                json!({"root":view["root"],"view":view["view"],
            "expected_version":1,"query":{"pattern":"native_live"}}),
            )
            .unwrap();
        if result.get("error").is_none() {
            assert_eq!(result["result"]["data"]["backend"], "shared-v2");
            if !result["result"]["data"]["matches"]
                .as_array()
                .unwrap()
                .is_empty()
            {
                break;
            }
        } else {
            assert_eq!(result["error"]["data"]["category"], "busy", "{result}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "native notifications did not reconcile: {result}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let before = daemon.rpc("views.status", json!({"id":view["view"]}));
    assert!(git(&fixture.a, &["status", "--porcelain", "--", "notes.txt"]).is_empty());
    git(&fixture.a, &["sparse-checkout", "set", "--no-cone", "src/"]);
    assert!(!fixture.a.join("notes.txt").exists());
    wait_for(
        &daemon,
        "views.status",
        json!({"id":view["view"]}),
        |status| {
            status["ready"] == true
                && status["input_epoch"].as_u64().unwrap() > before["input_epoch"].as_u64().unwrap()
        },
    );
    git(&fixture.a, &["sparse-checkout", "disable"]);
    let uncertain = daemon.rpc(
        "testing.watch-event",
        json!({"view":view["view"],"kind":"failure"}),
    );
    let fallback = wait_for(&daemon, "status", query.clone(), |status| {
        status["ready"] == true
            && status["watch_mode"] == "poll"
            && status["status"]["authoritative"]["input_epoch"]
                .as_u64()
                .unwrap()
                >= uncertain["input_epoch"].as_u64().unwrap()
    });
    assert!(
        fallback["watch_fallback"]
            .as_str()
            .unwrap()
            .contains("injected native watcher uncertainty")
    );
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
    let metadata = fs::metadata(&private).unwrap();
    fs::write(&private, "shared_term native_lost\n").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&private)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
        .unwrap();
    assert_eq!(fs::metadata(&private).unwrap().len(), metadata.len());
    assert_eq!(
        fs::metadata(&private).unwrap().modified().unwrap(),
        metadata.modified().unwrap()
    );
    let rescan = daemon.rpc(
        "testing.watch-event",
        json!({"view":view["view"],"kind":"rescan"}),
    );
    wait_for(
        &daemon,
        "views.status",
        json!({"id":view["view"]}),
        |status| {
            status["ready"] == true
                && status["authoritative"]["input_epoch"].as_u64().unwrap()
                    >= rescan["input_epoch"].as_u64().unwrap()
        },
    );
    let repaired = daemon.rpc(
        "search",
        json!({"root":view["root"],"view":view["view"],
        "expected_version":1,"query":{"pattern":"native_lost"}}),
    );
    assert_eq!(repaired["backend"], "shared-v2");
    assert!(!repaired["matches"].as_array().unwrap().is_empty());
    assert_scan_parity(&daemon, &fixture.a, fixture.temp.path());
    daemon.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"managed-client"}),
    );
    daemon.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut daemon);
}
