// Copyright (c) Microsoft Corporation. All rights reserved.

use super::storage::Directory;
use super::{Error, ErrorCategory, FileIdentity, Id, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

pub(crate) struct ActivityGuard {
    _file: File,
    _directory: Arc<Directory>,
}

impl ActivityGuard {
    pub(crate) fn acquire(directory: &Arc<Directory>) -> Result<Self> {
        let header: super::NamespaceHeader = directory.read_json("namespace.json", 64 * 1024)?;
        let file = directory.open_file("activity.lock", true)?;
        if FileIdentity::of(&file)? != header.activity_identity {
            return Err(Error::corrupt("namespace activity lock was replaced"));
        }
        fs2::FileExt::try_lock_shared(&file)
            .map_err(|error| lock_error(error, "namespace-admission-closed"))?;
        Ok(Self {
            _file: file,
            _directory: Arc::clone(directory),
        })
    }
}

/// The last owner of a mapped reader drops this only after unmapping its files.
pub(crate) struct ObjectGuard {
    pub(crate) id: Id,
    pub(crate) namespace: Id,
    pub(crate) directory: Arc<Directory>,
    _file: File,
    _activity: ActivityGuard,
    memory: OnceLock<super::memory::RetainedMemory>,
}

impl ObjectGuard {
    pub(crate) fn retain_memory(&self, memory: super::memory::RetainedMemory) -> Result<()> {
        self.memory
            .set(memory)
            .map_err(|_| Error::corrupt("object reader memory already registered"))
    }

    pub(crate) fn namespace_directory(&self) -> &Arc<Directory> {
        &self._activity._directory
    }

    pub(crate) fn acquire(
        namespace: &Id,
        root: &Arc<Directory>,
        guards: &Arc<Directory>,
        objects: &Arc<Directory>,
        id: &Id,
        expected: &FileIdentity,
    ) -> Result<Arc<Self>> {
        let activity = ActivityGuard::acquire(root)?;
        let file = guards.open_file(&format!("{id}.lock"), true)?;
        if &FileIdentity::of(&file)? != expected {
            return Err(Error::corrupt("object guard lock was replaced"));
        }
        fs2::FileExt::try_lock_shared(&file)
            .map_err(|error| lock_error(error, "object-retirement-in-progress"))?;
        let directory = objects.child(id.as_str())?;
        Ok(Arc::new(Self {
            id: id.clone(),
            namespace: namespace.clone(),
            directory,
            _file: file,
            _activity: activity,
            memory: OnceLock::new(),
        }))
    }
}

pub(crate) fn lock_error(error: std::io::Error, reason: &str) -> Error {
    if error.kind() == std::io::ErrorKind::WouldBlock
        || error
            .raw_os_error()
            .is_some_and(|code| Some(code) == fs2::lock_contended_error().raw_os_error())
    {
        Error::busy(reason)
    } else {
        Error::io(error)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerClaim {
    pub namespace: Id,
    pub instance: Id,
    pub owner: Id,
    pub challenge: Id,
    pub storage: PathBuf,
    pub guard_identity: FileIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnerSeal {
    pub(crate) namespace: Id,
    pub(crate) instance: Id,
    pub(crate) owner: Id,
    pub(crate) challenge: Id,
}

/// Hold in the client process until all owned leases are explicitly released.
/// Dropping this guard ends ownership; an idle connection does not.
pub struct OwnerGuard {
    claim: OwnerClaim,
    _guard: File,
    _directory: Arc<Directory>,
}

impl OwnerGuard {
    pub fn claim(claim: OwnerClaim) -> Result<Self> {
        let namespace = Directory::open(&claim.storage)?;
        let header: super::NamespaceHeader = namespace.read_json("namespace.json", 64 * 1024)?;
        if header.namespace != claim.namespace
            || header.directory_identity != namespace.identity()?
        {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "owner-namespace-replaced",
                "owner claim belongs to another namespace",
            ));
        }
        let owners = namespace.child("owners")?;
        let file = owners.open_file(&format!("{}.lock", claim.owner), true)?;
        if FileIdentity::of(&file)? != claim.guard_identity {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "owner-guard-replaced",
                "owner guard identity differs from the issued claim",
            ));
        }
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|error| lock_error(error, "owner-guard-already-held"))?;
        if namespace.observe_file("catalog.sqlite")?.identity != header.catalog_identity {
            return Err(Error::corrupt(
                "owner catalog identity differs from its namespace",
            ));
        }
        let connection = super::catalog::connect(&namespace, true)?;
        let service: Option<String> = connection
            .query_row(
                "SELECT record FROM records WHERE kind='service' AND id='current'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(service) = service {
            let service: serde_json::Value = serde_json::from_str(&service)?;
            if service["instance"] != serde_json::to_value(&claim.instance)?
                || service["stopping"] != false
            {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "owner-instance-ended",
                    "the owner claim belongs to a stopped or replaced daemon instance",
                ));
            }
        }
        let record: Option<String> = connection
            .query_row(
                "SELECT record FROM owners WHERE id=?1",
                [claim.owner.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let record: super::OwnerRecord = serde_json::from_str(&record.ok_or_else(|| {
            Error::new(
                ErrorCategory::StaleIdentity,
                "owner-retired",
                "the owner claim is no longer registered",
            )
        })?)?;
        if record.released
            || record.registered
            || super::catalog::text(&record.claim)? != super::catalog::text(&claim)?
        {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "owner-lifetime-ended",
                "a released or previously registered lifetime cannot be acquired again",
            ));
        }
        let seal: OwnerSeal = owners.read_json(&format!("{}.json", claim.owner), 4096)?;
        if seal.namespace != claim.namespace
            || seal.instance != claim.instance
            || seal.owner != claim.owner
            || seal.challenge != claim.challenge
        {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "owner-challenge-mismatch",
                "owner guard does not match the issued namespace/instance challenge",
            ));
        }
        namespace.verify()?;
        if namespace.observe_file("catalog.sqlite")?.identity != header.catalog_identity {
            return Err(Error::corrupt("owner catalog changed during claim"));
        }
        Ok(Self {
            claim,
            _guard: file,
            _directory: namespace,
        })
    }

    pub fn registration(&self) -> &OwnerClaim {
        &self.claim
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OwnerProof {
    Held,
    Ended,
    Unknown,
}

pub(crate) fn owner_proof(directory: &Arc<Directory>, claim: &OwnerClaim) -> Result<OwnerProof> {
    let owners = directory.child("owners")?;
    let file = owners.open_file(&format!("{}.lock", claim.owner), true)?;
    if FileIdentity::of(&file)? != claim.guard_identity {
        return Ok(OwnerProof::Unknown);
    }
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(OwnerProof::Ended),
        Err(error) => {
            let error = lock_error(error, "owner-lifetime-held");
            if error.category == ErrorCategory::Busy {
                Ok(OwnerProof::Held)
            } else {
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_owner_proof_requires_the_same_lifetime_guard() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = super::super::Namespace::initialize_identity(
            &"a".repeat(64),
            temp.path(),
            super::super::policy::fixture_policy(),
        )
        .unwrap();
        let directory = &namespace.directory;
        let claim = namespace.prepare_owner().unwrap().claim;
        assert_eq!(owner_proof(directory, &claim).unwrap(), OwnerProof::Ended);
        let guard = OwnerGuard::claim(claim.clone()).unwrap();
        namespace.register_owner(&claim).unwrap();
        assert_eq!(owner_proof(directory, &claim).unwrap(), OwnerProof::Held);
        drop(guard);
        assert_eq!(owner_proof(directory, &claim).unwrap(), OwnerProof::Ended);
        assert_eq!(
            OwnerGuard::claim(claim.clone()).err().unwrap().category,
            ErrorCategory::StaleIdentity
        );
        let mut wrong = claim.clone();
        wrong.guard_identity.file ^= 1;
        assert_eq!(owner_proof(directory, &wrong).unwrap(), OwnerProof::Unknown);
    }
}
