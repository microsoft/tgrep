//! Test-only runtime supervision: the caller persists leases and owns cleanup.
use super::*;
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};

const DEADLINE: Duration = Duration::from_secs(30);

// File-backed output cannot deadlock on a full pipe while waiting for a child.
struct Process {
    child: Child,
    stdout: fs::File,
    stderr: fs::File,
}

impl Process {
    fn start(command: &mut Command) -> Self {
        let stdout = tempfile::tempfile().unwrap();
        let stderr = tempfile::tempfile().unwrap();
        let child = command
            .stdin(Stdio::piped())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .unwrap();
        Self {
            child,
            stdout,
            stderr,
        }
    }

    fn cancel(&mut self) {
        if self.child.try_wait().unwrap().is_none()
            && let Err(error) = self.child.kill()
        {
            assert!(
                self.child.try_wait().unwrap().is_some(),
                "cannot cancel owned PID {}: {error}",
                self.child.id()
            );
        }
        self.child.wait().unwrap();
    }

    fn finish(&mut self, timeout: Duration) -> Result<Output, String> {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Ok(Output {
                    status,
                    stdout: Self::read(&mut self.stdout),
                    stderr: Self::read(&mut self.stderr),
                });
            }
            if started.elapsed() >= timeout {
                self.cancel();
                return Err(format!(
                    "owned PID {} exceeded {timeout:?}; killed and reaped\nstdout: {}\nstderr: {}",
                    self.child.id(),
                    String::from_utf8_lossy(&Self::read(&mut self.stdout)),
                    String::from_utf8_lossy(&Self::read(&mut self.stderr)),
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn read(file: &mut fs::File) -> Vec<u8> {
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        bytes
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Only this guard's child PID, never a process-name/global kill.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(super) fn output(command: &mut Command) -> Output {
    Process::start(command)
        .finish(Duration::from_secs(60))
        .unwrap_or_else(|error| panic!("{command:?}: {error}"))
}

#[derive(Serialize, Deserialize)]
struct Session {
    root: PathBuf,
    revision: String,
    token: String,
    marker: Value,
    lose_response: bool,
}

impl Session {
    fn persist(f: &Fixture, d: &Daemon, name: &str, root: &Path, lose_response: bool) -> PathBuf {
        let path = f.temp.path().join(format!("{name}.session.json"));
        let record = Self {
            root: root.to_path_buf(),
            revision: f.revision.clone(),
            token: format!(
                "runtime-{name}-{}",
                LEASE_SEQUENCE.fetch_add(1, Ordering::SeqCst)
            ),
            marker: d.marker.clone(),
            lose_response,
        };
        // The journal precedes transmission, so a missing response loses no token.
        fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        path
    }

    fn load(path: &Path) -> Self {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn params(&self) -> Value {
        json!({"root":self.root,"revision":self.revision,"lease":self.token,
            "profile":serde_json::from_str::<Value>(PROFILE).unwrap()})
    }

    fn request(&self, method: &str, params: Value, read_response: bool) -> Option<Value> {
        let address = format!("127.0.0.1:{}", self.marker["port"].as_u64().unwrap());
        let mut stream = TcpStream::connect_timeout(&address.parse().unwrap(), DEADLINE).unwrap();
        stream.set_read_timeout(Some(DEADLINE)).unwrap();
        stream.set_write_timeout(Some(DEADLINE)).unwrap();
        writeln!(
            stream,
            "{}",
            json!({
                "jsonrpc":"2.0","protocol":1,"instance":self.marker["instance"],
                "repository":self.marker["repository"],"id":1,"method":method,"params":params
            })
        )
        .unwrap();
        if !read_response {
            return None;
        }
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"]["instance"], self.marker["instance"]);
        Some(response["result"].clone())
    }

    fn detach(&self) -> Value {
        // Recovery needs only the persisted token/root, not the lost attach result.
        serde_json::from_str(&success(cli(
            &self.root,
            &["shared", "detach", ".", "--lease", &self.token],
        )))
        .unwrap()
    }
}

struct Client {
    process: Process,
    journal: PathBuf,
}

impl Client {
    fn start(journal: &Path) -> Self {
        let ready = journal.with_extension("ready");
        if ready.exists() {
            fs::remove_file(ready).unwrap();
        }
        let process = Process::start(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::runtime_client_process",
                    "--ignored",
                    "--nocapture",
                ])
                .env("TGREP_TEST_SESSION", journal),
        );
        Self {
            process,
            journal: journal.to_path_buf(),
        }
    }

    fn ready(&mut self) {
        let started = Instant::now();
        while !self.journal.with_extension("ready").exists() {
            if self.process.child.try_wait().unwrap().is_some() {
                panic!(
                    "client exited before ready: {:?}",
                    self.process.finish(DEADLINE).unwrap()
                );
            }
            assert!(
                started.elapsed() < DEADLINE,
                "client attach deadline: {:?}",
                self.journal
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn end(&mut self) {
        writeln!(self.process.child.stdin.as_mut().unwrap(), "detach").unwrap();
        success(self.process.finish(DEADLINE).unwrap());
    }
}

#[test]
#[ignore = "subprocess entry point; requires a caller-owned TGREP_TEST_SESSION journal"]
fn runtime_client_process() {
    let journal = PathBuf::from(std::env::var_os("TGREP_TEST_SESSION").expect("session journal"));
    let session = Session::load(&journal);
    session.request("attach", session.params(), !session.lose_response);
    fs::write(journal.with_extension("ready"), b"attach sent").unwrap();
    let mut action = String::new();
    std::io::stdin().read_line(&mut action).unwrap();
    if action.trim() == "detach" {
        let view = session
            .request("lookup", json!({"root":session.root}), true)
            .unwrap();
        session.request(
            "detach",
            json!({
                "root":session.root,"view":view["view"],"lease":session.token
            }),
            true,
        );
    } else {
        panic!("client abandoned without detach: {action:?}");
    }
}

#[test]
fn runtime_sessions_recover_abandonment_restart_and_release_budgets() {
    let f = Fixture::new();
    let options = [
        "--no-watch",
        "--shared-max-views",
        "2",
        "--shared-max-leases",
        "3",
    ];
    let mut d = f.start(&options);
    let sibling_journal = Session::persist(&f, &d, "sibling", &f.a, false);
    let lost_journal = Session::persist(&f, &d, "lost", &f.b, true);
    let second_journal = Session::persist(&f, &d, "second", &f.b, false);
    let mut sibling = Client::start(&sibling_journal);
    let mut lost = Client::start(&lost_journal);
    let mut second = Client::start(&second_journal);
    for client in [&mut sibling, &mut lost, &mut second] {
        client.ready();
    }
    d.ready(&f.a);
    let original = d.ready(&f.b);
    // Lookup of an already-existing view does not prove both concurrent attaches completed.
    let lost_session = Session::load(&lost_journal);
    let replay = d.rpc("attach", lost_session.params());
    assert_eq!(replay["view"], original["view"]);
    assert_eq!(d.lookup(&f.b)["leases"], 2);
    assert_eq!(d.search(&f.a, "shared_term")["backend"], "shared-v1");
    let excess_journal = Session::persist(&f, &d, "excess", &f.a, false);
    let excess = d
        .try_rpc("attach", Session::load(&excess_journal).params())
        .unwrap();
    assert!(
        excess["error"]["message"]
            .as_str()
            .unwrap()
            .contains("lease limit"),
        "{excess}"
    );

    // Repeat the same persisted caller in another real process at the lease limit.
    let mut retried = Client::start(&lost_journal);
    retried.ready();
    d.rpc("attach", lost_session.params());
    assert_eq!(d.lookup(&f.b)["leases"], 2);
    lost.process.cancel();
    second.end();
    assert_eq!(d.lookup(&f.b)["leases"], 1);
    assert_eq!(d.search(&f.a, "shared_term")["backend"], "shared-v1");

    // A timed-out/crashed caller has no expiry: the runtime must reload its journal.
    let timeout = retried
        .process
        .finish(Duration::from_millis(100))
        .unwrap_err();
    assert!(timeout.contains("killed and reaped"), "{timeout}");
    assert!(retried.process.child.try_wait().unwrap().is_some());
    retried.process.cancel();
    assert_eq!(d.lookup(&f.b)["leases"], 1);
    let c = f.third(&f.revision);
    let c_journal = Session::persist(&f, &d, "replacement", &c, false);
    let refused = d
        .try_rpc("attach", Session::load(&c_journal).params())
        .unwrap();
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("view limit"),
        "{refused}"
    );
    assert_eq!(Session::load(&lost_journal).detach()["remaining_leases"], 0);
    let mut replacement = Client::start(&c_journal);
    replacement.ready();
    d.ready(&c);

    let old = d.lookup(&f.a);
    let old_instance = d.marker["instance"].clone();
    sibling.process.cancel();
    replacement.process.cancel();
    d.stop();
    fs::write(f.a.join("offline.txt"), "offline_runtime_needle\n").unwrap();
    let d = f.start(&options);
    assert_ne!(d.marker["instance"], old_instance);
    let stale = d
        .try_rpc(
            "detach",
            json!({
                "root":f.a,"view":old["view"],"lease":Session::load(&sibling_journal).token
            }),
        )
        .unwrap();
    assert!(stale.get("error").is_some(), "{stale}");
    let fresh_a = Session::persist(&f, &d, "fresh-a", &f.a, false);
    let fresh_c = Session::persist(&f, &d, "fresh-c", &c, false);
    assert_ne!(
        Session::load(&fresh_a).token,
        Session::load(&sibling_journal).token
    );
    let mut sibling = Client::start(&fresh_a);
    let mut disposable = Client::start(&fresh_c);
    sibling.ready();
    disposable.ready();
    let restored = d.ready(&f.a);
    assert_eq!(restored["generation"], old["generation"]);
    assert_eq!(
        d.search(&f.a, "offline_runtime_needle")["backend"],
        "shared-v1"
    );
    assert_eq!(
        d.search(&f.a, "offline_runtime_needle")["matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    d.ready(&c);
    disposable.end();
    let phase = AtomicU64::new(0);
    thread::scope(|scope| {
        let phase = &phase;
        let daemon = &d;
        let root = &f.a;
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let (progress, receipts) = std::sync::mpsc::channel();
        let queries = scope.spawn(move || {
            let started = Instant::now();
            let mut reported = None;
            while matches!(
                stopped.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ) {
                assert!(started.elapsed() < DEADLINE, "sibling query loop deadline");
                let observed = phase.load(Ordering::SeqCst);
                assert_eq!(
                    daemon.search(root, "offline_runtime_needle")["backend"],
                    "shared-v1"
                );
                if reported != Some(observed) {
                    progress.send(observed).unwrap();
                    reported = Some(observed);
                }
            }
        });
        let checkpoint = |next| {
            phase.store(next, Ordering::SeqCst);
            loop {
                if receipts.recv_timeout(DEADLINE).unwrap() == next {
                    break;
                }
            }
        };
        checkpoint(1);
        // All operations hold no view/root handles for either disposable worktree.
        let renamed = f.temp.path().join("renamed-c");
        git(
            &f.a,
            &[
                "worktree",
                "move",
                c.to_str().unwrap(),
                renamed.to_str().unwrap(),
            ],
        );
        checkpoint(2);
        git(
            &f.a,
            &["worktree", "remove", "--force", renamed.to_str().unwrap()],
        );
        checkpoint(3);
        git(
            &f.a,
            &["worktree", "remove", "--force", f.b.to_str().unwrap()],
        );
        assert!(!c.exists() && !renamed.exists() && !f.b.exists());
        checkpoint(4);
        // Disconnection also cancels the loop if an operation panics.
        drop(stop);
        queries.join().unwrap();
    });
    sibling.end();
    assert!(
        d.try_rpc("lookup", json!({"root":f.a}))
            .unwrap()
            .get("error")
            .is_some()
    );
    assert!(d.rpc("hello", json!({})).get("limits").is_some());
    // The runtime owns daemon shutdown (Drop), not the last session.
}
