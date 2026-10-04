use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tgrep_core::generations::{GenerationKey, IndexingProfile, Repository};

pub const PROTOCOL: u32 = 1;
pub const MARKER: &str = "tgrep-daemon-v1.json";
pub(super) const VIEW_MARKER: &str = "tgrep-view-v1.json";
pub(super) const MAX_REQUEST: u64 = 1024 * 1024;
pub(super) const MAX_RESPONSE: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Registration {
    pub protocol: u32,
    pub instance: String,
    pub repository: String,
    pub pid: u32,
    pub port: u16,
    pub storage: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    pub jsonrpc: String,
    pub protocol: u32,
    pub instance: String,
    pub repository: String,
    pub method: String,
    pub params: Value,
    pub id: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub struct View {
    pub root: PathBuf,
    pub view: String,
    pub generation: GenerationKey,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ViewRegistration {
    pub daemon: Registration,
    pub root: PathBuf,
    pub view: String,
    pub generation: GenerationKey,
}

pub struct Client {
    registration: Registration,
    root: PathBuf,
}

impl Client {
    /// A marker is the opt-in. Its mere presence selects the safe shared
    /// fallback even if its payload, daemon or pinned generation is unavailable.
    pub fn selected(path: &Path, force: bool, index_path: Option<&Path>) -> Result<bool> {
        if force {
            return Ok(true);
        }
        if index_path.is_some() {
            return Ok(false);
        }
        let path = fs::canonicalize(path)?;
        let Some(root) = path.ancestors().find(|parent| parent.join(".git").exists()) else {
            return Ok(false);
        };
        if let Some(git_dir) = tgrep_core::git_index::git_dir(root) {
            // This only selects shared discovery, never establishes authority.
            // Unattached legacy queries should not spawn Git subprocesses.
            return marker_present(&git_dir.join(VIEW_MARKER));
        }
        let repo = Repository::discover(root)?;
        marker_present(&repo.git_dir().join(VIEW_MARKER))
    }

    pub fn registered(path: &Path) -> Result<(Self, View)> {
        let root = super::worktree_root(path)?;
        let repo = Repository::discover(&root)?;
        let marker: ViewRegistration = serde_json::from_reader(
            File::open(repo.git_dir().join(VIEW_MARKER))
                .context("worktree has no shared attachment")?
                .take(MAX_REQUEST),
        )?;
        let client = Self::discover(root.clone(), &repo)?;
        ensure!(
            marker.daemon.protocol == PROTOCOL
                && marker.daemon.instance == client.registration.instance
                && marker.daemon.repository == repo.identity()
                && marker.daemon.port == client.registration.port
                && marker.root == root,
            "stale or incompatible worktree registration; reattach"
        );
        let view = client.lookup()?;
        ensure!(
            view.view == marker.view && view.generation == marker.generation,
            "stale shared view registration"
        );
        Ok((client, view))
    }

    fn discover(root: PathBuf, repo: &Repository) -> Result<Self> {
        let registration: Registration = serde_json::from_reader(
            fs::File::open(repo.common_dir().join(MARKER))
                .context("no shared daemon registration; start serve --shared")?
                .take(MAX_REQUEST),
        )?;
        ensure!(
            registration.protocol == PROTOCOL
                && registration.repository == repo.identity()
                && !registration.instance.is_empty()
                && registration.port != 0,
            "incompatible shared daemon registration"
        );
        let client = Self { registration, root };
        let hello = client.request("hello", json!({}))?;
        ensure!(
            hello["capabilities"]
                == json!([
                    "leases",
                    "recoverable-attach",
                    "worktree-overlays",
                    "refresh",
                    "search",
                    "files"
                ])
                && hello["profile"] == json!(IndexingProfile::default()),
            "shared daemon capabilities/profile are incompatible"
        );
        Ok(client)
    }

    pub fn lookup(&self) -> Result<View> {
        let result = self.request("lookup", json!({"root": self.root}))?;
        let view: View = serde_json::from_value(result)?;
        ensure!(
            view.root == self.root
                && view.generation.repository_identity() == self.registration.repository
                && *view.generation.profile() == IndexingProfile::default(),
            "shared view identity/profile mismatch"
        );
        Ok(view)
    }

    pub fn view_request(&self, method: &str, view: &View, query: Value) -> Result<Value> {
        let result = self.request(
            method,
            json!({"root": view.root, "view": view.view, "query": query}),
        )?;
        ensure!(
            result["root"] == json!(view.root)
                && result["view"] == view.view
                && result["generation"] == json!(view.generation),
            "shared response view/base mismatch"
        );
        if method != "status" {
            ensure!(
                result["ready"] == true && result["epoch"].is_u64(),
                "shared view is not ready"
            );
        }
        Ok(result)
    }

    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, self.registration.port));
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
            .context("shared daemon unreachable")?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.set_read_timeout(Some(Duration::from_secs(300)))?;
        let request = json!({
            "jsonrpc": "2.0", "protocol": PROTOCOL,
            "instance": self.registration.instance,
            "repository": self.registration.repository,
            "method": method, "params": params, "id": 1
        });
        let bytes = serde_json::to_vec(&request)?;
        ensure!(
            bytes.len() < MAX_REQUEST as usize,
            "shared request too large"
        );
        stream.write_all(&bytes)?;
        stream.write_all(b"\n")?;
        let line = read_line(&mut stream, MAX_RESPONSE)?;
        let response: Value = serde_json::from_slice(&line)?;
        ensure!(
            response.is_object() && response["jsonrpc"] == "2.0",
            "invalid shared RPC response"
        );
        if let Some(error) = response.get("error") {
            ensure!(
                response.get("id").is_some_and(|id| id == 1 || id.is_null())
                    && response.get("result").is_none()
                    && error.is_object()
                    && error["code"].is_i64()
                    && error["message"].is_string(),
                "invalid shared RPC error response"
            );
            bail!(
                "shared daemon: {}",
                error["message"].as_str().expect("validated message")
            );
        }
        ensure!(response["id"] == 1, "invalid shared RPC response");
        let result = response
            .get("result")
            .context("missing shared RPC result")?;
        ensure!(
            result["protocol"] == PROTOCOL
                && result["instance"] == self.registration.instance
                && result["repository"] == self.registration.repository,
            "stale or wrong-protocol shared daemon"
        );
        Ok(result.clone())
    }
}

pub(super) fn marker_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn read_line(stream: &mut TcpStream, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    BufReader::new(stream.take(limit + 1)).read_until(b'\n', &mut bytes)?;
    ensure!(
        bytes.len() <= limit as usize && bytes.last() == Some(&b'\n'),
        "RPC line missing newline or exceeds {limit} bytes"
    );
    Ok(bytes)
}

#[derive(Subcommand)]
pub enum Lifecycle {
    /// Pin a committed tree and lease a private view. Emits JSON; inspect ready.
    Attach {
        root: PathBuf,
        #[arg(long)]
        revision: String,
        /// Caller-owned unique token. Reuse it to recover an interrupted attach.
        #[arg(long)]
        lease: Option<String>,
    },
    /// Acknowledge known changes, or verify all bytes with --full (the default).
    Refresh {
        root: PathBuf,
        #[arg(long)]
        lease: String,
        #[arg(long, action = clap::ArgAction::Append, conflicts_with = "full")]
        changed: Vec<PathBuf>,
        #[arg(long)]
        full: bool,
    },
    /// Release only this lease. Other leases keep their view and daemon alive.
    Detach {
        root: PathBuf,
        #[arg(long)]
        lease: String,
    },
}

pub fn run_lifecycle(command: Lifecycle) -> Result<()> {
    let (root, method, mut params) = match command {
        Lifecycle::Attach {
            root,
            revision,
            lease,
        } => {
            let lease = lease.unwrap_or_else(|| {
                blake3::hash(format!("{}:{:?}", std::process::id(), SystemTime::now()).as_bytes())
                    .to_hex()
                    .to_string()
            });
            eprintln!("shared attach lease: {lease} (retry with --lease {lease})");
            (
                root,
                "attach",
                json!({"revision": revision, "profile": IndexingProfile::default(), "lease": lease}),
            )
        }
        Lifecycle::Refresh {
            root,
            lease,
            changed,
            full,
        } => (
            root,
            "refresh",
            json!({"lease": lease, "full": full || changed.is_empty(), "changed": changed}),
        ),
        Lifecycle::Detach { root, lease } => (root, "detach", json!({"lease": lease})),
    };
    let root = super::canonical_root(&root)?;
    ensure!(
        super::worktree_root(&root)? == root,
        "lifecycle requires a worktree root"
    );
    let repo = Repository::discover(&root)?;
    let client = Client::discover(root.clone(), &repo)?;
    params["root"] = json!(root);
    if method != "attach" {
        params["view"] = json!(client.lookup()?.view);
    }
    println!("{}", client.request(method, params)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;
    use std::time::Instant;

    fn exchange(response: Value) -> Result<Value> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = Client {
            registration: Registration {
                protocol: PROTOCOL,
                instance: "test-instance".into(),
                repository: "test-repository".into(),
                pid: std::process::id(),
                port: listener.local_addr().unwrap().port(),
                storage: PathBuf::new(),
            },
            root: PathBuf::new(),
        };
        let worker = thread::spawn(move || {
            let started = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(started.elapsed() < Duration::from_secs(5));
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            // Accepted sockets inherit nonblocking mode on macOS.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request: Value =
                serde_json::from_slice(&read_line(&mut stream, MAX_REQUEST).unwrap()).unwrap();
            assert_eq!(request["id"], 1);
            assert_eq!(request["method"], "hello");
            writeln!(stream, "{response}").unwrap();
        });
        let result = client.request("hello", json!({}));
        worker.join().unwrap();
        result
    }

    #[test]
    fn rpc_response_envelopes_preserve_queue_diagnostics() {
        let success = json!({
            "protocol":PROTOCOL,"instance":"test-instance","repository":"test-repository"
        });
        assert_eq!(
            exchange(json!({"jsonrpc":"2.0","id":1,"result":success})).unwrap(),
            success
        );
        let error = json!({"code":-32001,"message":"shared connection queue full"});
        for id in [json!(null), json!(1)] {
            let failure = exchange(json!({"jsonrpc":"2.0","id":id,"error":error})).unwrap_err();
            assert!(
                failure.to_string().contains("shared connection queue full"),
                "{failure:#}"
            );
        }
        for response in [
            json!(null),
            json!([]),
            json!("not an envelope"),
            json!({"jsonrpc":"1.0","id":null,"error":error}),
            json!({"id":null,"error":error}),
            json!({"jsonrpc":"2.0","error":error}),
            json!({"jsonrpc":"2.0","id":2,"error":error}),
            json!({"jsonrpc":"2.0","id":"1","error":error}),
            json!({"jsonrpc":"2.0","id":null,"error":null}),
            json!({"jsonrpc":"2.0","id":null,"error":[]}),
            json!({"jsonrpc":"2.0","id":null,"error":{"message":"shared connection queue full"}}),
            json!({"jsonrpc":"2.0","id":null,"error":{"code":"-32001","message":"shared connection queue full"}}),
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-1.5,"message":"shared connection queue full"}}),
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32001}}),
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":{}}}),
            json!({"jsonrpc":"2.0","id":1,"error":error,"result":success}),
            json!({"jsonrpc":"2.0","id":null,"error":error,"result":null}),
            json!({"jsonrpc":"2.0","id":null,"result":success}),
            json!({"jsonrpc":"2.0","id":2,"result":success}),
            json!({"jsonrpc":"2.0","id":"1","result":success}),
            json!({"jsonrpc":"2.0","result":success}),
            json!({"jsonrpc":"2.0","id":1}),
        ] {
            let failure = exchange(response.clone()).unwrap_err();
            assert!(
                !failure.to_string().contains("shared connection queue full"),
                "{response}: {failure:#}"
            );
        }
    }
}
