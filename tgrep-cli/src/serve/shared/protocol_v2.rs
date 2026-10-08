// Copyright (c) Microsoft Corporation. All rights reserved.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{BufRead, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tgrep_core::generations::Repository;
use tgrep_core::managed::{
    self, CommitState, Error, ErrorCategory, Id, OwnerClaim, OwnerGuard, Policy,
};

pub const MARKER: &str = "tgrep-daemon-v2.json";
pub const VIEW_MARKER: &str = "tgrep-view-v2.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Registration {
    pub protocol: u32,
    pub namespace: Id,
    pub instance: Id,
    pub repository: String,
    pub pid: u32,
    pub port: u16,
    pub storage: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ViewRegistration {
    pub daemon: Registration,
    pub root: PathBuf,
    pub view: Id,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    pub jsonrpc: String,
    pub protocol: u32,
    pub namespace: Id,
    pub instance: Id,
    pub repository: String,
    pub method: String,
    pub params: Value,
    pub id: Value,
}

pub(super) struct Client {
    pub registration: Registration,
}

impl Client {
    pub fn discover(root: &Path, repository: &Repository) -> Result<Self> {
        let registration: Registration = serde_json::from_reader(
            File::open(repository.common_dir().join(MARKER))?
                .take(managed::MAX_REQUEST_BYTES as u64),
        )?;
        ensure!(
            registration.protocol == managed::PROTOCOL_VERSION
                && registration.repository == repository.identity()
                && registration.port != 0,
            "incompatible managed daemon registration"
        );
        let client = Self { registration };
        let hello = client.request("hello", json!({}))?;
        ensure!(
            hello["storage_schema"] == managed::STORAGE_VERSION
                && hello["capabilities"]
                    .as_array()
                    .is_some_and(|capabilities| capabilities
                        .iter()
                        .any(|capability| capability == "versioned-views")),
            "daemon does not support managed view negotiation for {}",
            root.display()
        );
        Ok(client)
    }

    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        let registration = &self.registration;
        let request = json!({
            "jsonrpc":"2.0","protocol":managed::PROTOCOL_VERSION,
            "namespace":registration.namespace,"instance":registration.instance,"repository":registration.repository,
            "id":1,"method":method,"params":params
        });
        let bytes = serde_json::to_vec(&request)?;
        ensure!(
            bytes.len() < managed::MAX_REQUEST_BYTES,
            "managed request exceeds its encoded size limit"
        );
        let mut stream = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, registration.port)),
            Duration::from_secs(2),
        )
        .map_err(|error| {
            Error::new(ErrorCategory::Busy, "daemon-unreachable", error.to_string())
        })?;
        stream.set_read_timeout(Some(Duration::from_secs(300)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream
            .write_all(&bytes)
            .and_then(|_| stream.write_all(b"\n"))
            .map_err(|error| Error::io(error).committed(CommitState::Unknown))?;
        let line = super::protocol::read_line(&mut stream, managed::MAX_RESPONSE_BYTES as u64)
            .map_err(|error| {
                Error::new(
                    ErrorCategory::Deadline,
                    "response-unavailable",
                    format!("{error:#}"),
                )
                .committed(CommitState::Unknown)
            })?;
        let response: Value = serde_json::from_slice(&line)?;
        ensure!(
            response.is_object()
                && response["jsonrpc"] == "2.0"
                && (response["id"] == 1 || response["id"].is_null()),
            "invalid managed RPC envelope"
        );
        if let Some(error) = response.get("error") {
            ensure!(
                response.get("result").is_none() && error["code"].is_i64(),
                "invalid managed RPC error"
            );
            let error: Error = serde_json::from_value(error["data"].clone())
                .context("missing typed management failure")?;
            return Err(error.into());
        }
        ensure!(
            response["id"] == 1,
            "managed response has no matching request"
        );
        let result = response
            .get("result")
            .context("missing managed response result")?;
        ensure!(
            result["protocol"] == managed::PROTOCOL_VERSION
                && result["namespace"] == json!(registration.namespace)
                && result["instance"] == json!(registration.instance)
                && result["repository"] == registration.repository,
            "wrong managed namespace or daemon instance"
        );
        result
            .get("data")
            .cloned()
            .context("managed response has no data")
    }
}

pub(super) fn read_policy(path: &Path) -> Result<Policy> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(managed::MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= managed::MAX_REQUEST_BYTES,
        "managed policy exceeds its size limit"
    );
    let policy: Policy = serde_json::from_slice(&bytes)?;
    policy.validate()?;
    Ok(policy)
}

fn output(result: Result<Value>) -> Result<()> {
    match result {
        Ok(value) => {
            println!("{}", json!({"ok":true,"result":value}));
            Ok(())
        }
        Err(error) => {
            let failure = error
                .downcast_ref::<Error>()
                .map(serde_json::to_value)
                .transpose()?
                .unwrap_or(serde_json::to_value(Error::invalid(format!("{error:#}")))?);
            println!("{}", json!({"ok":false,"error":failure}));
            Err(error)
        }
    }
}

pub(super) fn discover(cache_parent: &Path) -> Result<()> {
    let mut discovery = match managed::NamespaceDiscovery::open(cache_parent) {
        Ok(discovery) => discovery,
        Err(error) => return output(Err(error.into())),
    };
    loop {
        let page = match discovery.next_page(256) {
            Ok(page) => page,
            Err(error) => return output(Err(error.into())),
        };
        let complete = page.complete;
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(&mut stdout, &json!({"ok":true,"result":page}))?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
        if complete {
            return Ok(());
        }
    }
}

pub(super) fn manage(root: &Path, method: &str, params: &str) -> Result<()> {
    output((|| {
        let root = super::canonical_root(root)?;
        let repository = Repository::discover(&root)?;
        let client = Client::discover(&root, &repository)?;
        client.request(method, serde_json::from_str(params)?)
    })())
}

pub(super) fn owner_hold(path: &Path) -> Result<()> {
    let claim: OwnerClaim =
        serde_json::from_reader(File::open(path)?.take(managed::MAX_REQUEST_BYTES as u64))?;
    let guard = OwnerGuard::claim(claim)?;
    println!("{}", json!({"holding":true,"claim":guard.registration()}));
    std::io::stdout().flush()?;
    let mut byte = [0_u8; 1];
    while std::io::stdin().read(&mut byte)? != 0 {}
    drop(guard);
    Ok(())
}

pub(super) fn maintenance(namespace: &Path, method: &str, params: &str, apply: bool) -> Result<()> {
    if method == "session" {
        ensure!(
            params == "{}",
            "maintenance session parameters must be empty"
        );
        return maintenance_session(namespace, apply);
    }
    output((|| {
        let namespace = managed::Namespace::open(namespace)?;
        if apply {
            namespace.activate()?;
        }
        super::managed::storage_request(
            &namespace,
            method,
            serde_json::from_str(params)?,
            if apply {
                super::managed::StorageContext::Maintenance
            } else {
                super::managed::StorageContext::Inspect
            },
        )
    })())
}

fn maintenance_session(path: &Path, apply: bool) -> Result<()> {
    let namespace = managed::Namespace::open(path)?;
    if apply {
        namespace.activate()?;
    }
    println!(
        "{}",
        json!({"ready":true,"namespace":namespace.header().namespace,"instance":namespace.instance()})
    );
    std::io::stdout().flush()?;
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    loop {
        let mut bytes = Vec::new();
        let size = Read::by_ref(&mut reader)
            .take(managed::MAX_REQUEST_BYTES as u64 + 1)
            .read_until(b'\n', &mut bytes)?;
        if size == 0 {
            return Ok(());
        }
        if size > managed::MAX_REQUEST_BYTES {
            return Err(Error::invalid("maintenance frame exceeds request limit").into());
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Frame {
            id: Value,
            method: String,
            params: Value,
        }
        let frame: Frame =
            serde_json::from_slice(&bytes).map_err(|error| Error::invalid(error.to_string()))?;
        ensure!(
            frame.id.is_string() || frame.id.is_number() || frame.id.is_null(),
            "invalid maintenance request id"
        );
        let response = match super::managed::storage_request(
            &namespace,
            &frame.method,
            frame.params,
            if apply {
                super::managed::StorageContext::Maintenance
            } else {
                super::managed::StorageContext::Inspect
            },
        ) {
            Ok(result) => json!({"id":frame.id,"ok":true,"result":result}),
            Err(error) => {
                let value = match error.downcast::<Error>() {
                    Ok(error) => serde_json::to_value(error)?,
                    Err(error) => serde_json::to_value(Error::invalid(format!("{error:#}")))?,
                };
                json!({"id":frame.id,"ok":false,"error":value})
            }
        };
        let encoded = serde_json::to_vec(&response)?;
        if encoded.len() > managed::MAX_RESPONSE_BYTES {
            return Err(Error::pressure("response-byte-limit").into());
        }
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&encoded)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
}
