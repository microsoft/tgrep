// Copyright (c) Microsoft Corporation. All rights reserved.

use super::*;
use std::io::{self, Read};
use std::net::SocketAddr;

pub(super) fn completed_record(
    record: Value,
    status: impl FnOnce(&Value) -> io::Result<Value>,
) -> Value {
    assert_eq!(
        record["state"],
        "completed",
        "operation did not succeed: {record}; view diagnostic: {}",
        operation_context(&record, status)
    );
    record
}

pub(super) fn operation_context(
    record: &Value,
    status: impl FnOnce(&Value) -> io::Result<Value>,
) -> Value {
    let view = &record["progress"]["view"];
    if view.is_null() {
        return json!({"unavailable":"operation has no progress.view"});
    }
    // This is a later observation, not an atomic snapshot of the failed stage.
    match status(view) {
        Ok(response) => json!({"view":view,"view_status_response":response}),
        Err(error) => json!({
            "view":view,"view_status_error":error.to_string(),
            "error_kind":format!("{:?}",error.kind())
        }),
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "original operation wait budget exhausted; view diagnostic unavailable",
            )
        })
}

pub(super) fn view_status_before(
    daemon: &Daemon,
    view: &Value,
    deadline: Instant,
) -> io::Result<Value> {
    let budget = remaining(deadline)?;
    let port = daemon.marker["port"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid daemon port"))?;
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, budget)?;
    diagnostic_response_before(
        &mut stream,
        &daemon.request("views.status", json!({"id":view})),
        deadline,
    )
}

// One request, with every socket operation charged to the original wait budget.
fn diagnostic_response_before(
    stream: &mut TcpStream,
    request: &Value,
    deadline: Instant,
) -> io::Result<Value> {
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    let mut pending = bytes.as_slice();
    while !pending.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        let written = stream.write(pending)?;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        pending = &pending[written..];
    }
    let mut response = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if let Some(end) = buffer[..read].iter().position(|byte| *byte == b'\n') {
            response.extend_from_slice(&buffer[..end]);
            remaining(deadline)?;
            return Ok(serde_json::from_slice(&response)?);
        }
        response.extend_from_slice(&buffer[..read]);
    }
}

pub(super) struct StopTiming {
    started: Instant,
    accepted: Option<Instant>,
}

impl StopTiming {
    pub(super) fn new(started: Instant) -> Self {
        Self {
            started,
            accepted: None,
        }
    }

    pub(super) fn accepted(&mut self, observed: Instant) {
        self.accepted = Some(observed);
    }

    pub(super) fn snapshot(&self, now: Instant) -> Value {
        json!({
            "elapsed_ms":now.duration_since(self.started).as_millis(),
            "stop_request_busy_ms":self.accepted.unwrap_or(now).duration_since(self.started).as_millis(),
            "stopping_true_observed_after_ms":self.accepted.map(|at|at.duration_since(self.started).as_millis()),
            "post_accept_exit_wait_ms":self.accepted.map(|at|now.duration_since(at).as_millis())
        })
    }

    pub(super) fn assert_within_budget(&self, now: Instant, response: Option<&Value>) {
        assert!(
            now.duration_since(self.started) < Duration::from_secs(10),
            "{}; timing: {}; last_response: {response:?}",
            if self.accepted.is_some() {
                "stopped daemon did not exit"
            } else {
                "idle shutdown is still busy"
            },
            self.snapshot(now)
        );
    }
}

pub(super) fn panic_text(action: impl FnOnce() + std::panic::UnwindSafe) -> String {
    let panic = std::panic::catch_unwind(action).expect_err("expected the original assertion");
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| text.to_string()))
        .expect("text panic")
}

#[test]
fn completed_failure_retains_record_and_best_effort_view_status() {
    let record = json!({
        "state":"failed","kind":"attach","progress":{"view":"owned-view","resolved_commit":"fixture"},
        "error":{"category":"deadline","reason_code":"operation-deadline","retryable":false,
                 "committed_state":"not-committed","detail":"operation time budget exhausted"}
    });
    let response = json!({"result":{"data":{
        "ready":false,"last_reconcile":{"timings":{"checkpoint_us":42},"outcome":"failed"}
    }}});
    let message = panic_text(|| {
        completed_record(record.clone(), |view| {
            assert_eq!(view, "owned-view");
            Ok(response.clone())
        });
    });
    assert!(message.contains(&format!("operation did not succeed: {record}")));
    assert!(message.contains(&response.to_string()), "{message}");
    for error in [
        io::ErrorKind::TimedOut,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::InvalidData,
    ] {
        let message = panic_text(|| {
            completed_record(record.clone(), |_| Err(error.into()));
        });
        assert!(message.contains(&record.to_string()), "{message}");
        assert!(message.contains(&format!("{error:?}")), "{message}");
    }
    let response = json!({"error":{"data":{"reason_code":"view-not-found"}}});
    let message = panic_text(|| {
        completed_record(record.clone(), |_| Ok(response.clone()));
    });
    assert!(message.contains(&record.to_string()), "{message}");
    assert!(message.contains(&response.to_string()), "{message}");
    let record = json!({"state":"completed"});
    assert_eq!(
        completed_record(record.clone(), |_| panic!(
            "must not query a successful operation"
        )),
        record
    );
    assert_eq!(
        operation_context(&json!({"state":"failed"}), |_| panic!("no view to query")),
        json!({"unavailable":"operation has no progress.view"})
    );
}

fn socket_pair() -> (TcpStream, TcpStream) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    (client, server)
}

#[test]
fn diagnostic_transport_keeps_response_and_does_not_retry_or_extend_expired_budget() {
    let request = json!({"method":"views.status","params":{"id":"owned-view"}});
    for line in [
        "{\"result\":{\"data\":{\"last_reconcile\":{\"files_read\":3}}}}\n",
        "{\"error\":{\"data\":{\"reason_code\":\"view-not-found\"}}}\n",
        "invalid-json\n",
        "",
    ] {
        let (mut client, mut server) = socket_pair();
        server.write_all(line.as_bytes()).unwrap();
        server.shutdown(std::net::Shutdown::Write).unwrap();
        let response = diagnostic_response_before(
            &mut client,
            &request,
            Instant::now() + Duration::from_secs(30),
        );
        if line.starts_with('{') {
            assert_eq!(
                response.unwrap(),
                serde_json::from_str::<Value>(line).unwrap()
            );
        } else {
            assert!(response.is_err());
        }
        drop(client);
        let mut sent = String::new();
        server.read_to_string(&mut sent).unwrap();
        assert_eq!(sent, format!("{request}\n"), "exactly one request");
    }
    let (mut client, mut server) = socket_pair();
    let error = diagnostic_response_before(&mut client, &request, Instant::now()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    drop(client);
    let mut sent = String::new();
    server.read_to_string(&mut sent).unwrap();
    assert!(sent.is_empty(), "expired budget must not send a request");
}

#[test]
fn shutdown_failure_reports_phases_without_resetting_the_shared_budget() {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(10);
    let mut timing = StopTiming::new(started);
    timing.assert_within_budget(deadline - Duration::from_nanos(1), None);
    let busy = json!({"stopping":false,"owners":1});
    let message = panic_text(|| timing.assert_within_budget(deadline, Some(&busy)));
    assert!(message.contains("idle shutdown is still busy"), "{message}");
    assert!(
        message.contains("\"stop_request_busy_ms\":10000"),
        "{message}"
    );
    assert!(
        message.contains("\"post_accept_exit_wait_ms\":null"),
        "{message}"
    );
    assert!(message.contains(&format!("{busy:?}")), "{message}");

    timing.accepted(started + Duration::from_secs(9));
    timing.assert_within_budget(deadline - Duration::from_nanos(1), None);
    let message = panic_text(|| timing.assert_within_budget(deadline, None));
    assert!(message.contains("stopped daemon did not exit"), "{message}");
    assert!(
        message.contains("\"stopping_true_observed_after_ms\":9000"),
        "{message}"
    );
    assert!(
        message.contains("\"post_accept_exit_wait_ms\":1000"),
        "{message}"
    );

    // The original helper accepts a successful stop response before checking elapsed time.
    timing.accepted(started + Duration::from_secs(12));
    let message = panic_text(|| {
        timing.assert_within_budget(started + Duration::from_secs(12), None);
    });
    assert!(message.contains("\"elapsed_ms\":12000"), "{message}");
    assert!(
        message.contains("\"stopping_true_observed_after_ms\":12000"),
        "{message}"
    );
    assert!(
        message.contains("\"post_accept_exit_wait_ms\":0"),
        "{message}"
    );
}
