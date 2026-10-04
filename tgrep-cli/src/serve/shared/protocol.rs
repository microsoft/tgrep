use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
        let repo = Repository::discover(root)?;
        Ok(repo.git_dir().join(VIEW_MARKER).try_exists()?)
    }

    pub fn registered(path: &Path) -> Result<(Self, View)> {
        let root = super::worktree_root(path)?;
        let repo = Repository::discover(&root)?;
        let marker: ViewRegistration = serde_json::from_reader(
            File::open(repo.git_dir().join(VIEW_MARKER))
                .context("worktree has no shared attachment")?
                .take(MAX_REQUEST),
        )?;
        let client = Self::discover(&root)?;
        ensure!(
            marker.daemon.protocol == PROTOCOL
                && marker.daemon.instance == client.registration.instance
                && marker.daemon.repository == repo.identity()
                && marker.daemon.port == client.registration.port
                && marker.root == root,
            "stale or incompatible worktree registration; reattach"
        );
        let view = client.lookup(&root)?;
        ensure!(
            view.view == marker.view && view.generation == marker.generation,
            "stale shared view registration"
        );
        Ok((client, view))
    }

    pub fn discover(path: &Path) -> Result<Self> {
        let root = super::worktree_root(path)?;
        let repo = Repository::discover(&root)?;
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
        let client = Self { registration };
        let hello = client.request("hello", json!({}))?;
        ensure!(
            hello["capabilities"]
                == json!(["leases", "worktree-overlays", "refresh", "search", "files"])
                && hello["profile"] == json!(IndexingProfile::default()),
            "shared daemon capabilities/profile are incompatible"
        );
        Ok(client)
    }

    pub fn lookup(&self, path: &Path) -> Result<View> {
        let root = super::worktree_root(path)?;
        let result = self.request("lookup", json!({"root": root}))?;
        let view: View = serde_json::from_value(result)?;
        ensure!(
            view.root == root
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
            response["jsonrpc"] == "2.0" && response["id"] == 1,
            "invalid shared RPC response"
        );
        if let Some(error) = response.get("error") {
            bail!("shared daemon: {}", error["message"]);
        }
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
        Lifecycle::Attach { root, revision } => (
            root,
            "attach",
            json!({"revision": revision, "profile": IndexingProfile::default()}),
        ),
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
    let root = fs::canonicalize(root)?;
    ensure!(
        super::worktree_root(&root)? == root,
        "lifecycle requires a worktree root"
    );
    let client = Client::discover(&root)?;
    params["root"] = json!(root);
    if method != "attach" {
        params["view"] = json!(client.lookup(&root)?.view);
    }
    println!("{}", client.request(method, params)?);
    Ok(())
}
