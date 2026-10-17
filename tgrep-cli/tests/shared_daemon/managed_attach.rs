// Copyright (c) Microsoft Corporation. All rights reserved.

//! Exact-state recovery for documented retryable, not-committed attach results.

use super::*;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::panic::AssertUnwindSafe;

pub(super) trait AttachEndpoint {
    fn marker(&self) -> &Value;

    /// One raw JSON-RPC response; transport failure fails the test.
    fn raw_call(&self, method: &str, params: Value) -> Value;

    fn call(&self, method: &str, params: Value) -> Value {
        rpc_data(method, &self.raw_call(method, params), self.marker())
    }

    fn terminal_before(&self, operation: &Value, deadline: Instant) -> Value;

    fn view_status_before(&self, view: &Value, deadline: Instant) -> io::Result<Value>;
}

impl AttachEndpoint for Daemon {
    fn marker(&self) -> &Value {
        &self.marker
    }

    fn raw_call(&self, method: &str, params: Value) -> Value {
        self.try_rpc(method, params).unwrap()
    }

    fn call(&self, method: &str, params: Value) -> Value {
        self.rpc(method, params)
    }

    fn terminal_before(&self, operation: &Value, deadline: Instant) -> Value {
        super::terminal_before(self, operation, deadline)
    }

    fn view_status_before(&self, view: &Value, deadline: Instant) -> io::Result<Value> {
        super::diagnostics::view_status_before(self, view, deadline)
    }
}

/// The only terminal failures a caller may resolve and retry with a fresh token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RetryableAttach {
    CatalogIo,
    PreparationInvalidated,
}

impl RetryableAttach {
    fn reason(self) -> &'static str {
        match self {
            Self::CatalogIo => "catalog-io",
            Self::PreparationInvalidated => "view-preparation-invalidated",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::CatalogIo => "catalog contention",
            Self::PreparationInvalidated => "view-preparation invalidation",
        }
    }

    pub(super) fn permits(self, record: &Value) -> bool {
        record["kind"] == "attach"
            && record["state"] == "failed"
            && record["cancelled"] == false
            && record["committed_state"] == "not-committed"
            && record["error"]["category"] == "busy"
            && record["error"]["reason_code"] == self.reason()
            && record["error"]["retryable"] == true
            && record["error"]["committed_state"] == "not-committed"
    }
}

pub(super) struct Retry<'a> {
    pub(super) point: &'a str,
    pub(super) deadline: Instant,
    pub(super) allowance: RetryableAttach,
}

fn recover_initial_intent<E: AttachEndpoint>(
    endpoint: &E,
    attach: &Value,
    pending: &mut Option<Value>,
    point: &str,
) {
    let lookup = endpoint.raw_call("lookup", json!({"root":attach["request"]["root"]}));
    if let Some(error) = lookup.get("error") {
        assert_eq!(error["data"]["category"], "busy", "{point}: {lookup}");
        assert_eq!(
            error["data"]["reason_code"], "worktree-not-attached",
            "{point}: {lookup}"
        );
        assert_eq!(error["data"]["retryable"], true, "{point}: {lookup}");
        assert_eq!(
            error["data"]["committed_state"], "not-committed",
            "{point}: {lookup}"
        );
        assert!(
            pending.is_none(),
            "{point}: the known pending view disappeared"
        );
        // No catalog root is not a claim that all preparation state rolled back.
        return;
    }
    assert_eq!(lookup["result"]["instance"], endpoint.marker()["instance"]);
    assert_eq!(
        lookup["result"]["namespace"],
        endpoint.marker()["namespace"]
    );
    let current = endpoint.call(
        "views.recover",
        json!({"id":lookup["result"]["data"]["view"]}),
    );
    assert_eq!(current["version"], 1, "{point}: {current}");
    assert_eq!(current["committed"], false, "{point}: {current}");
    assert_eq!(current["active"], true, "{point}: {current}");
    assert!(current["current"].is_null(), "{point}: {current}");
    assert_eq!(
        current["pending"]["commit"], attach["request"]["revision"],
        "{point}: the pending exact target changed"
    );
    if let Some(previous) = pending {
        for field in [
            "id",
            "version",
            "root",
            "root_identity",
            "root_anchor",
            "pending",
            "checkpoint",
            "checkpoint_binding",
            "input_epoch",
            "reconciled_epoch",
            "instance",
        ] {
            assert_eq!(current[field], previous[field], "{point}: {field} changed");
        }
    } else {
        *pending = Some(current);
    }
}

/// Retries only an exact allowed terminal result, after replaying its receipt and
/// recovering the unchanged pending intent, within one caller-owned deadline.
pub(super) fn retry_attach<E: AttachEndpoint>(
    endpoint: &E,
    claim: &OwnerClaim,
    view: Option<&Value>,
    mut attach: Value,
    retry: Retry<'_>,
    mut before_attempt: impl FnMut(&Value),
) -> Result<Value, Vec<Value>> {
    let Retry {
        point,
        deadline,
        allowance,
    } = retry;
    let mut failures: Vec<Value> = Vec::new();
    let mut pending = None;
    let first_sequence = attach["token"]["sequence"].as_u64().unwrap();
    assert_eq!(attach["token"]["scope"], json!(claim.owner));
    assert_eq!(attach["request"]["owner"], json!(claim.owner));
    assert!(
        view.is_none() || allowance == RetryableAttach::CatalogIo,
        "{point}: preparation-invalidation recovery is defined only for exact initial attachment"
    );
    if view.is_none() {
        let revision = attach["request"]["revision"].as_str().unwrap();
        assert!(
            matches!(revision.len(), 40 | 64)
                && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "initial retry requires an already captured exact commit"
        );
        assert!(attach["request"]["accept_current"].is_null());
    }
    for attempt in 0..8 {
        if Instant::now() >= deadline {
            break;
        }
        let sequence = first_sequence.checked_add(attempt).unwrap();
        if attempt != 0 {
            attach["token"] = token(claim, sequence);
        }
        if attempt != 0 && view.is_none() {
            recover_initial_intent(endpoint, &attach, &mut pending, point);
        }
        before_attempt(&attach);
        if Instant::now() >= deadline {
            break;
        }
        let accepted = endpoint.call("views.attach", attach.clone());
        let record = endpoint.terminal_before(&accepted, deadline);
        assert_eq!(record["request"], attach["request"], "{point}: {record}");
        assert_eq!(record["token"], attach["token"], "{point}: {record}");
        if record["state"] == "completed" {
            assert_eq!(record["committed_state"], "committed", "{point}: {record}");
            let current = &record["result"]["current"];
            let lease = &record["result"]["lease"];
            assert_eq!(lease["token"], attach["request"]["lease"], "{point}");
            assert_eq!(lease["owner"], attach["request"]["owner"], "{point}");
            assert_eq!(lease["released"], false, "{point}");
            for field in ["revision", "profile", "accept_current", "migratable"] {
                assert_eq!(
                    lease["original"][field], attach["request"][field],
                    "{point}: {field}"
                );
            }
            assert_eq!(lease["original"]["root"], current["root"], "{point}");
            if let Some(view) = view {
                assert_eq!(current["id"], view["id"], "{point}");
                assert_eq!(current["version"], view["version"], "{point}");
                assert_eq!(current["current"], view["current"], "{point}");
            } else {
                assert_eq!(current["version"], 1, "{point}");
                assert_eq!(current["committed"], true, "{point}");
                assert_eq!(
                    current["current"]["commit"], attach["request"]["revision"],
                    "{point}"
                );
                assert_eq!(
                    lease["exact_original_commit"], attach["request"]["revision"],
                    "{point}"
                );
                if let Some(pending) = &pending {
                    for field in ["id", "version", "root", "root_identity", "instance"] {
                        assert_eq!(current[field], pending[field], "{point}: {field}");
                    }
                    assert_eq!(
                        current["current"]["key"], pending["pending"]["key"],
                        "{point}"
                    );
                }
            }
            if allowance == RetryableAttach::PreparationInvalidated {
                // The failed attempt inserted the lease before preparation; resumption must reuse it.
                let creator = failures.first().unwrap_or(&record);
                assert_eq!(
                    lease["operation"], creator["id"],
                    "{point}: recovery created another lease operation"
                );
                let status = endpoint.call("views.status", json!({"id":current["id"]}));
                assert_eq!(
                    status["leases"], 1,
                    "{point}: recovery left more than one lease: {status}"
                );
            }
            for failed in failures {
                assert_eq!(
                    endpoint.call(
                        "views.attach",
                        json!({
                            "token":failed["token"],"request":failed["request"]
                        })
                    ),
                    failed,
                    "{point}: a new attempt must not rewrite a failed receipt"
                );
            }
            return Ok(record);
        }
        if !allowance.permits(&record) {
            match allowance {
                RetryableAttach::CatalogIo => {
                    panic!("{point}: attach failed without safe retry authorization: {record}")
                }
                RetryableAttach::PreparationInvalidated => panic!(
                    "{point}: attach failed without safe retry authorization: {record}; view diagnostic: {}",
                    super::diagnostics::operation_context(&record, |view| {
                        endpoint.view_status_before(view, deadline)
                    })
                ),
            }
        }
        if allowance == RetryableAttach::PreparationInvalidated {
            assert!(
                record["progress"]["view"].is_string(),
                "{point}: preparation invalidation lacks progress.view: {record}"
            );
        }
        assert_eq!(
            endpoint.call("views.attach", attach.clone()),
            record,
            "{point}: a failed token must replay its original receipt"
        );
        if let Some(view) = view {
            let current = endpoint.call("views.recover", json!({"id":view["id"]}));
            assert_eq!(current["version"], view["version"], "{point}: {current}");
            assert_eq!(current["current"], view["current"], "{point}: {current}");
        } else {
            recover_initial_intent(endpoint, &attach, &mut pending, point);
            if record["progress"]["view"].is_string() {
                let pending = pending
                    .as_ref()
                    .expect("the recorded pending view must still exist");
                assert_eq!(record["progress"]["view"], pending["id"], "{point}");
                assert_eq!(
                    record["progress"]["resolved_commit"], pending["pending"]["commit"],
                    "{point}"
                );
            }
        }
        eprintln!(
            "{point}: attach attempt {sequence} had uncommitted {}",
            allowance.description()
        );
        failures.push(record);
        if attempt < 7 {
            thread::sleep(
                Duration::from_millis(25).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    Err(failures)
}

const INSTANCE: &str = "11111111111111111111111111111111";
const NAMESPACE: &str = "22222222222222222222222222222222";
const OWNER: &str = "33333333333333333333333333333333";
const VIEW: &str = "44444444444444444444444444444444";
const OTHER: &str = "77777777777777777777777777777777";
const REVISION: &str = "5555555555555555555555555555555555555555";
const ROOT: &str = "scripted-root";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tamper {
    None,
    ReplayRewritten,
    EnvelopeInstance,
    InspectFailure,
    LookupInstance,
    LookupMissing,
    LookupFailure,
    RecoverFailure,
    RecoverChanged(&'static str),
    RecoverCommitted,
    RecoverTarget,
    DuplicateLease,
    ExtraLease,
    CompletedIdentity,
    CompletedProfile,
}

enum Step {
    Fail(Vec<(&'static str, Value)>),
    FailAtDeadline,
    Complete,
    ErrorResponse,
}

fn invalidated() -> Step {
    Step::Fail(Vec::new())
}

/// In-process endpoint modelled on the documented attach receipt contract.
struct Scripted {
    marker: Value,
    tamper: Tamper,
    script: RefCell<VecDeque<Step>>,
    receipts: RefCell<BTreeMap<String, Value>>,
    operations: RefCell<BTreeMap<String, Value>>,
    stalled: RefCell<Vec<String>>,
    lease_operation: RefCell<Option<Value>>,
    resolved: Cell<bool>,
    submissions: Cell<u64>,
    recoveries: Cell<u64>,
    calls: RefCell<Vec<String>>,
    deadlines: RefCell<Vec<Instant>>,
}

impl Scripted {
    fn new(steps: impl IntoIterator<Item = Step>, tamper: Tamper) -> Self {
        Self {
            marker: json!({"protocol":2,"instance":INSTANCE,"namespace":NAMESPACE,"repository":"scripted"}),
            tamper,
            script: RefCell::new(steps.into_iter().collect()),
            receipts: RefCell::default(),
            operations: RefCell::default(),
            stalled: RefCell::default(),
            lease_operation: RefCell::default(),
            resolved: Cell::new(true),
            submissions: Cell::new(0),
            recoveries: Cell::new(0),
            calls: RefCell::default(),
            deadlines: RefCell::default(),
        }
    }

    fn envelope(&self, data: Value) -> Value {
        json!({"id":1,"jsonrpc":"2.0","result":{
            "data":data,"instance":INSTANCE,"namespace":NAMESPACE,"protocol":2,"repository":"scripted"
        }})
    }

    fn error(category: &str, reason: &str) -> Value {
        json!({"id":1,"jsonrpc":"2.0","error":{"code":-32000,"message":reason,"data":{
            "category":category,"reason_code":reason,"retryable":true,"committed_state":"not-committed"
        }}})
    }

    fn key() -> Value {
        json!({"index_format":1,"profile":"scripted-profile","repository":"scripted-repository",
            "schema":2,"tree":"scripted-tree"})
    }

    fn pending_view() -> Value {
        json!({"active":true,"checkpoint":null,"checkpoint_binding":null,"committed":false,
            "current":null,"id":VIEW,"input_epoch":0,"instance":INSTANCE,
            "pending":{"commit":REVISION,"key":Self::key()},"reconciled_epoch":null,"revision":1,
            "root":ROOT,"root_anchor":null,"root_identity":{"file":7,"volume":3},"version":1})
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn log(&self, call: impl Into<String>) {
        self.calls.borrow_mut().push(call.into());
    }

    fn unconsumed(&self) -> usize {
        self.script.borrow().len()
    }

    fn completed(&self, id: &str, params: &Value) -> Value {
        let request = &params["request"];
        let operation = if self.tamper == Tamper::DuplicateLease {
            json!(id)
        } else {
            self.lease_operation.borrow().clone().unwrap()
        };
        let mut current = Self::pending_view();
        current["committed"] = json!(true);
        current["current"] = json!({"commit":REVISION,"key":Self::key()});
        current["pending"] = Value::Null;
        current["checkpoint"] = json!("scripted-checkpoint");
        current["reconciled_epoch"] = json!(1);
        current["input_epoch"] = json!(1);
        if self.tamper == Tamper::CompletedIdentity {
            current["root_identity"] = json!({"file":8,"volume":3});
        }
        let mut original = json!({"root":ROOT,"revision":request["revision"],
            "profile":request["profile"],"accept_current":request["accept_current"],
            "migratable":request["migratable"]});
        if self.tamper == Tamper::CompletedProfile {
            original["profile"] = json!({"content":"changed"});
        }
        json!({"cancelled":false,"committed_state":"committed","error":null,"id":id,
            "instance":INSTANCE,"kind":"attach","progress":{"resolved_commit":REVISION,"view":VIEW},
            "request":request,"state":"completed","token":params["token"],
            "result":{"current":current,"lease":{"token":request["lease"],"owner":request["owner"],
                "instance":INSTANCE,"view":VIEW,"original":original,"exact_original_commit":REVISION,
                "original_version":1,"released":false,"operation":operation}}})
    }

    fn attach(&self, params: Value) -> Value {
        let token = params["token"]["token"].as_str().unwrap().to_owned();
        self.log(format!("views.attach {token}"));
        if let Some(receipt) = self.receipts.borrow().get(&token) {
            assert_eq!(receipt["request"], params["request"], "scripted replay");
            let mut replay = receipt.clone();
            if self.tamper == Tamper::ReplayRewritten {
                replay["error"]["detail"] = json!("rewritten");
            }
            return self.envelope(replay);
        }
        assert!(
            self.resolved.get(),
            "{token} was submitted before the previous terminal result was resolved"
        );
        let step = self
            .script
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| panic!("unscripted submission {token}"));
        let sequence = self.submissions.get() + 1;
        self.submissions.set(sequence);
        if let Step::ErrorResponse = step {
            return Self::error("busy", "view-work-active");
        }
        let id = format!("{sequence:032x}");
        // A documented preparation failure happens after its lease is inserted.
        self.lease_operation
            .borrow_mut()
            .get_or_insert_with(|| json!(id));
        let failure = json!({"cancelled":false,"committed_state":"not-committed",
            "error":{"category":"busy","committed_state":"not-committed",
                "detail":"view-preparation-invalidated","operation_id":id,
                "reason_code":"view-preparation-invalidated","retryable":true},
            "id":id,"instance":INSTANCE,"kind":"attach",
            "progress":{"resolved_commit":REVISION,"view":VIEW},
            "request":params["request"],"result":null,"state":"failed","token":params["token"]});
        let record = match step {
            Step::Fail(patches) => {
                let mut record = failure;
                for (pointer, value) in patches {
                    *record.pointer_mut(pointer).unwrap() = value;
                }
                record
            }
            Step::FailAtDeadline => {
                self.stalled.borrow_mut().push(id.clone());
                failure
            }
            Step::Complete => self.completed(&id, &params),
            Step::ErrorResponse => unreachable!(),
        };
        self.resolved.set(false);
        self.operations
            .borrow_mut()
            .insert(id.clone(), record.clone());
        self.receipts.borrow_mut().insert(token, record);
        let mut response = self.envelope(json!({"id":id,"kind":"attach","state":"accepted",
            "token":params["token"],"request":params["request"]}));
        if self.tamper == Tamper::EnvelopeInstance {
            response["result"]["instance"] = json!(OTHER);
        }
        response
    }
}

impl AttachEndpoint for Scripted {
    fn marker(&self) -> &Value {
        &self.marker
    }

    fn raw_call(&self, method: &str, params: Value) -> Value {
        match method {
            "views.attach" => self.attach(params),
            "operations.inspect" => {
                let id = params["id"].as_str().unwrap().to_owned();
                self.log(format!(
                    "operations.inspect {}",
                    u64::from_str_radix(&id, 16).unwrap()
                ));
                if self.tamper == Tamper::InspectFailure {
                    return Self::error("io", "operation-unreadable");
                }
                let record = self.operations.borrow()[&id].clone();
                self.resolved.set(true);
                self.envelope(record)
            }
            "lookup" => {
                self.log("lookup");
                assert_eq!(params["root"], ROOT);
                match self.tamper {
                    Tamper::LookupMissing => Self::error("busy", "worktree-not-attached"),
                    Tamper::LookupFailure => Self::error("io", "catalog-unreadable"),
                    tamper => {
                        let mut response = self.envelope(json!({"root":ROOT,"view":VIEW,
                            "generation":null,"version":1,"incarnation":null,"commit":null,
                            "ready":false}));
                        if tamper == Tamper::LookupInstance {
                            response["result"]["instance"] = json!(OTHER);
                        }
                        response
                    }
                }
            }
            "views.recover" => {
                self.log("views.recover");
                assert_eq!(params["id"], VIEW);
                let recovery = self.recoveries.get() + 1;
                self.recoveries.set(recovery);
                let mut view = Self::pending_view();
                match self.tamper {
                    Tamper::RecoverFailure => return Self::error("busy", "view-work-active"),
                    Tamper::RecoverChanged(field) if recovery > 1 => view[field] = json!("changed"),
                    Tamper::RecoverCommitted => view["committed"] = json!(true),
                    Tamper::RecoverTarget => view["pending"]["commit"] = json!(OTHER),
                    _ => {}
                }
                self.envelope(view)
            }
            "views.status" => {
                self.log("views.status");
                let leases = if self.tamper == Tamper::ExtraLease {
                    2
                } else {
                    1
                };
                self.envelope(json!({"id":params["id"],"leases":leases}))
            }
            other => panic!("unscripted method {other}"),
        }
    }

    fn terminal_before(&self, operation: &Value, deadline: Instant) -> Value {
        self.deadlines.borrow_mut().push(deadline);
        let id = operation["id"].as_str().unwrap().to_owned();
        if self.stalled.borrow().contains(&id) {
            // This terminal result becomes observable only after the shared deadline.
            while Instant::now() <= deadline {
                thread::sleep(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
                );
            }
        }
        let record = self.call("operations.inspect", json!({"id":operation["id"]}));
        match record["state"].as_str().unwrap() {
            "completed" | "failed" | "cancelled" => record,
            state => panic!("scripted operation is not terminal: {state}"),
        }
    }

    fn view_status_before(&self, view: &Value, _: Instant) -> io::Result<Value> {
        self.log("diagnostic views.status");
        Ok(json!({"scripted_view_status":view}))
    }
}

fn scripted_claim() -> OwnerClaim {
    serde_json::from_value(
        json!({"namespace":NAMESPACE,"instance":INSTANCE,"owner":OWNER,
        "challenge":"66666666666666666666666666666666","storage":"scripted-storage",
        "guard_identity":{"volume":1,"file":2}}),
    )
    .unwrap()
}

fn scripted_input(claim: &OwnerClaim) -> Value {
    json!({"token":token(claim,1),"request":{"root":ROOT,"revision":REVISION,
        "profile":serde_json::from_str::<Value>(PROFILE).unwrap(),"lease":"managed-client",
        "owner":claim.owner,"accept_current":null,"migratable":true,"allocation_version":1}})
}

fn watcher(scripted: &Scripted, deadline: Instant) -> Value {
    let claim = scripted_claim();
    super::watch::attach_watched_root(scripted, &claim, scripted_input(&claim), deadline)
}

fn watcher_panic(scripted: &Scripted, deadline: Instant) -> String {
    super::diagnostics::panic_text(AssertUnwindSafe(|| {
        watcher(scripted, deadline);
    }))
}

fn retried(
    scripted: &Scripted,
    allowance: RetryableAttach,
    deadline: Instant,
) -> Result<Value, Vec<Value>> {
    let claim = scripted_claim();
    retry_attach(
        scripted,
        &claim,
        None,
        scripted_input(&claim),
        Retry {
            point: "scripted",
            deadline,
            allowance,
        },
        |_| {},
    )
}

fn operation(sequence: u64) -> Value {
    json!(format!("{sequence:032x}"))
}

fn later() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

#[test]
fn watcher_attach_recovers_exact_preparation_invalidation_with_a_fresh_token() {
    let deadline = later();
    let scripted = Scripted::new([invalidated(), Step::Complete], Tamper::None);
    let record = watcher(&scripted, deadline);
    assert_eq!(record["state"], "completed");
    assert_eq!(record["token"]["sequence"], 2);
    assert_eq!(record["token"]["token"], "operation-2");
    assert_eq!(record["result"]["lease"]["operation"], operation(1));
    assert_eq!(
        scripted.calls(),
        [
            "views.attach operation-1",
            "operations.inspect 1",
            "views.attach operation-1",
            "lookup",
            "views.recover",
            "lookup",
            "views.recover",
            "views.attach operation-2",
            "operations.inspect 2",
            "views.status",
            "views.attach operation-1",
        ]
    );
    assert_eq!(scripted.submissions.get(), 2);
    assert_eq!(*scripted.deadlines.borrow(), [deadline, deadline]);
    assert_eq!(scripted.unconsumed(), 0);

    let scripted = Scripted::new(
        (0..7).map(|_| invalidated()).chain([Step::Complete]),
        Tamper::None,
    );
    let record = watcher(&scripted, deadline);
    assert_eq!(record["token"]["sequence"], 8);
    assert_eq!(record["result"]["lease"]["operation"], operation(1));
    assert_eq!(scripted.submissions.get(), 8);
    let replays = scripted
        .calls()
        .iter()
        .filter(|call| *call == "views.attach operation-1")
        .count();
    assert_eq!(replays, 3, "submission, immediate replay, final replay");
}

#[test]
fn watcher_attach_rejects_rewritten_receipts_and_additional_leases() {
    for (tamper, submissions, message) in [
        (
            Tamper::ReplayRewritten,
            1,
            "a failed token must replay its original receipt",
        ),
        (
            Tamper::DuplicateLease,
            2,
            "recovery created another lease operation",
        ),
        (Tamper::ExtraLease, 2, "recovery left more than one lease"),
    ] {
        let scripted = Scripted::new([invalidated(), Step::Complete], tamper);
        let text = watcher_panic(&scripted, later());
        assert!(text.contains(message), "{tamper:?}: {text}");
        assert_eq!(scripted.submissions.get(), submissions, "{tamper:?}");
    }
}

#[test]
fn watcher_attach_exhausts_eight_attempts_on_one_shared_deadline() {
    let deadline = later();
    let scripted = Scripted::new(
        (0..8).map(|_| invalidated()).chain([Step::Complete]),
        Tamper::None,
    );
    let failures = retried(&scripted, RetryableAttach::PreparationInvalidated, deadline)
        .expect_err("eight invalidations exhaust the attempt ceiling");
    assert_eq!(failures.len(), 8);
    for (index, failure) in failures.iter().enumerate() {
        let sequence = index as u64 + 1;
        assert_eq!(failure["token"]["sequence"], sequence);
        assert_eq!(failure["id"], operation(sequence));
    }
    assert_eq!(scripted.unconsumed(), 1, "no ninth submission");
    assert_eq!(*scripted.deadlines.borrow(), vec![deadline; 8]);

    let scripted = Scripted::new(
        (0..8).map(|_| invalidated()).chain([Step::Complete]),
        Tamper::None,
    );
    let text = watcher_panic(&scripted, deadline);
    assert!(text.contains("exhausted the shared attach bound"), "{text}");
    assert_eq!(scripted.submissions.get(), 8);
}

#[test]
fn watcher_attach_stops_at_the_original_deadline_without_another_submission() {
    let deadline = Instant::now() + Duration::from_secs(3);
    let scripted = Scripted::new(
        [invalidated(), Step::FailAtDeadline, Step::Complete],
        Tamper::None,
    );
    let failures = retried(&scripted, RetryableAttach::PreparationInvalidated, deadline)
        .expect_err("the shared deadline expired");
    assert!(Instant::now() > deadline);
    assert_eq!(failures.len(), 2);
    assert_eq!(scripted.submissions.get(), 2);
    assert_eq!(scripted.unconsumed(), 1);
    assert_eq!(*scripted.deadlines.borrow(), [deadline, deadline]);

    let expired = Instant::now();
    let scripted = Scripted::new([Step::Complete], Tamper::None);
    let text = watcher_panic(&scripted, expired);
    assert!(text.contains("exhausted the shared attach bound"), "{text}");
    assert!(scripted.calls().is_empty(), "{:?}", scripted.calls());
}

#[test]
fn watcher_attach_rejects_outcomes_outside_the_exact_allowance() {
    let unauthorized = "without safe retry authorization";
    for (patch, message) in [
        (("/kind", json!("migrate")), unauthorized),
        (("/state", json!("cancelled")), unauthorized),
        (("/cancelled", json!(true)), unauthorized),
        (("/committed_state", json!("committed")), unauthorized),
        (("/committed_state", json!("unknown")), unauthorized),
        (("/committed_state", Value::Null), unauthorized),
        (("/error/committed_state", json!("committed")), unauthorized),
        (("/error/committed_state", json!("unknown")), unauthorized),
        (("/error/committed_state", Value::Null), unauthorized),
        (("/error/category", json!("io")), unauthorized),
        (("/error/reason_code", json!("catalog-io")), unauthorized),
        (
            ("/error/reason_code", json!("view-attachment-preparing")),
            unauthorized,
        ),
        (
            ("/error/reason_code", json!("view-work-active")),
            unauthorized,
        ),
        (
            ("/error/reason_code", json!("publication-epoch-changed")),
            unauthorized,
        ),
        (("/error/retryable", json!(false)), unauthorized),
        (("/error/retryable", Value::Null), unauthorized),
        (("/error", Value::Null), unauthorized),
        (
            ("/error", json!("view-preparation-invalidated")),
            unauthorized,
        ),
        (("/state", json!("completed")), "native-watch-attach"),
        (("/state", Value::Null), "unwrap"),
        (
            ("/request/root", json!("other-root")),
            "native-watch-attach",
        ),
        (
            ("/token/token", json!("operation-9")),
            "native-watch-attach",
        ),
        (("/progress", Value::Null), "lacks progress.view"),
        (("/progress/view", json!(OTHER)), "native-watch-attach"),
        (
            ("/progress/resolved_commit", json!(OTHER)),
            "native-watch-attach",
        ),
    ] {
        let scripted = Scripted::new(
            [Step::Fail(vec![patch.clone()]), Step::Complete],
            Tamper::None,
        );
        let text = watcher_panic(&scripted, later());
        assert!(text.contains(message), "{patch:?}: {text}");
        if message == unauthorized {
            assert!(text.contains("scripted_view_status"), "{patch:?}: {text}");
        }
        assert_eq!(scripted.submissions.get(), 1, "{patch:?}");
        assert_eq!(scripted.unconsumed(), 1, "{patch:?}: no fresh token");
    }
}

#[test]
fn watcher_attach_rejects_changed_or_unavailable_recovery_identity() {
    let mut cases = vec![
        (Tamper::RecoverCommitted, 1),
        (Tamper::RecoverTarget, 1),
        (Tamper::LookupInstance, 1),
        (Tamper::LookupMissing, 1),
        (Tamper::LookupFailure, 1),
        (Tamper::CompletedIdentity, 2),
        (Tamper::CompletedProfile, 2),
    ];
    for field in [
        "id",
        "version",
        "root",
        "root_identity",
        "root_anchor",
        "pending",
        "checkpoint",
        "checkpoint_binding",
        "input_epoch",
        "reconciled_epoch",
        "instance",
    ] {
        cases.push((Tamper::RecoverChanged(field), 1));
    }
    for (tamper, submissions) in cases {
        let scripted = Scripted::new([invalidated(), Step::Complete], tamper);
        let text = watcher_panic(&scripted, later());
        assert!(
            !text.contains("unscripted") && !text.contains("was submitted before"),
            "{tamper:?}: {text}"
        );
        assert_eq!(
            scripted.submissions.get(),
            submissions,
            "{tamper:?}: {text}"
        );
        assert_eq!(scripted.unconsumed() as u64, 2 - submissions, "{tamper:?}");
    }
}

#[test]
fn watcher_attach_rejects_error_shaped_and_misaddressed_responses() {
    for (steps, tamper, message) in [
        (
            vec![Step::ErrorResponse, Step::Complete],
            Tamper::None,
            "views.attach",
        ),
        (
            vec![invalidated(), Step::Complete],
            Tamper::InspectFailure,
            "operations.inspect",
        ),
        (
            vec![invalidated(), Step::Complete],
            Tamper::RecoverFailure,
            "views.recover",
        ),
        (
            vec![invalidated(), Step::Complete],
            Tamper::EnvelopeInstance,
            OTHER,
        ),
    ] {
        let scripted = Scripted::new(steps, tamper);
        let text = watcher_panic(&scripted, later());
        assert!(text.contains(message), "{tamper:?}: {text}");
        assert_eq!(scripted.submissions.get(), 1, "{tamper:?}");
        assert_eq!(scripted.unconsumed(), 1, "{tamper:?}");
    }
}

#[test]
fn catalog_allowance_is_unchanged_and_rejects_preparation_invalidation() {
    let scripted = Scripted::new([invalidated(), Step::Complete], Tamper::None);
    let text = super::diagnostics::panic_text(AssertUnwindSafe(|| {
        let _ = retried(&scripted, RetryableAttach::CatalogIo, later());
    }));
    assert!(text.contains("without safe retry authorization"), "{text}");
    assert!(!text.contains("view diagnostic"), "{text}");
    assert_eq!(
        scripted.calls(),
        ["views.attach operation-1", "operations.inspect 1"]
    );

    // Catalog callers keep their pre-lease contention path and gain no watcher-only checks.
    let catalog = vec![
        ("/error/reason_code", json!("catalog-io")),
        ("/error/detail", json!("catalog-io")),
        ("/progress", Value::Null),
    ];
    let scripted = Scripted::new([Step::Fail(catalog), Step::Complete], Tamper::LookupMissing);
    let record = retried(&scripted, RetryableAttach::CatalogIo, later()).unwrap();
    assert_eq!(record["token"]["sequence"], 2);
    assert!(!scripted.calls().contains(&"views.status".to_owned()));

    let claim = scripted_claim();
    let view = json!({"id":VIEW,"version":1,"current":null});
    let text = super::diagnostics::panic_text(AssertUnwindSafe(|| {
        let _ = retry_attach(
            &Scripted::new([], Tamper::None),
            &claim,
            Some(&view),
            scripted_input(&claim),
            Retry {
                point: "scripted",
                deadline: later(),
                allowance: RetryableAttach::PreparationInvalidated,
            },
            |_| {},
        );
    }));
    assert!(text.contains("only for exact initial attachment"), "{text}");
}

#[test]
fn retryable_attach_classifier_is_exact_per_allowance() {
    for allowance in [
        RetryableAttach::CatalogIo,
        RetryableAttach::PreparationInvalidated,
    ] {
        let permitted = json!({
            "kind":"attach","state":"failed","cancelled":false,"committed_state":"not-committed",
            "error":{"category":"busy","reason_code":allowance.reason(),"retryable":true,
                "committed_state":"not-committed"}
        });
        assert!(allowance.permits(&permitted), "{allowance:?}");
        let other = if allowance == RetryableAttach::CatalogIo {
            "view-preparation-invalidated"
        } else {
            "catalog-io"
        };
        for (pointer, value) in [
            ("/kind", json!("migrate")),
            ("/state", json!("completed")),
            ("/state", json!("cancelled")),
            ("/cancelled", json!(true)),
            ("/committed_state", json!("committed")),
            ("/committed_state", json!("unknown")),
            ("/committed_state", Value::Null),
            ("/error/category", json!("io")),
            ("/error/reason_code", json!(other)),
            ("/error/retryable", json!(false)),
            ("/error/committed_state", json!("committed")),
            ("/error/committed_state", json!("unknown")),
            ("/error", Value::Null),
        ] {
            let mut rejected = permitted.clone();
            *rejected.pointer_mut(pointer).unwrap() = value;
            assert!(
                !allowance.permits(&rejected),
                "{allowance:?} {pointer}: {rejected}"
            );
        }
    }
}
