// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::{connect, object_row, save_object, text, touch_object};
use super::lifetime::ObjectGuard;
use super::storage::Directory;
use super::work::ensure_admission;
use super::{
    Error, ErrorCategory, Id, Namespace, NamespaceHeader, ObjectKind, ObjectState, Result,
    WorkPermit,
};
use crate::generations::{
    BuildStats, Generation, GenerationKey, GenerationManager, IndexingProfile, Repository,
};
use crate::output::Output;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, Weak};

type Cache = HashMap<(Id, Id), Weak<Generation>>;
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationDescriptor {
    pub namespace: Id,
    pub incarnation: Id,
    pub key: GenerationKey,
    pub requested_commit: String,
    pub fingerprint: [u8; 32],
    pub stats: BuildStats,
}

pub struct Materialization {
    pub descriptor: GenerationDescriptor,
    pub generation: Arc<Generation>,
}

fn load(
    pin: Arc<ObjectGuard>,
    key: &GenerationKey,
    manifest_limit: u64,
    permit: Option<&Arc<WorkPermit>>,
) -> Result<Arc<Generation>> {
    let cache = CACHE.get_or_init(Default::default);
    let identity = (pin.namespace.clone(), pin.id.clone());
    {
        let cache = cache
            .lock()
            .map_err(|_| Error::corrupt("managed generation cache poisoned"))?;
        if let Some(generation) = cache.get(&identity).and_then(Weak::upgrade) {
            return Ok(generation);
        }
    }
    let size = |name| -> Result<u64> {
        Ok(pin
            .directory
            .open_file(name, false)?
            .seek(SeekFrom::End(0))?)
    };
    let limits =
        crate::reader::SnapshotLimits::measure(crate::ondisk::IndexLayout::Managed, |name| {
            Ok(pin.directory.open_file(name, false)?)
        })?;
    let manifest_size = size("generation.tgm")?;
    if manifest_size > manifest_limit {
        return Err(Error::pressure("generation-manifest-byte-limit"));
    }
    let base_private = limits.private_estimate()?;
    let private = manifest_size
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(base_private))
        .ok_or_else(|| Error::pressure("generation-private-account-overflow"))?;
    let mapped = limits.mapped_bytes()?;
    let (memory, retained) = if let Some(permit) = permit {
        (Some(permit.memory(private)?), None)
    } else {
        let connection = connect(pin.namespace_directory(), false)?;
        let policy: String =
            connection.query_row("SELECT policy FROM state WHERE singleton=1", [], |row| {
                row.get(0)
            })?;
        let policy: super::Policy = serde_json::from_str(&policy)?;
        policy.validate()?;
        let limit = policy
            .work
            .private_work_bytes
            .min(super::work::allocation_row(&connection)?.private_work_bytes);
        let account = super::memory::MemoryAccount::for_namespace(&pin.namespace)?;
        (
            None,
            Some(account.retain_unreserved(&connection, private, mapped, limit)?),
        )
    };
    let generation = Arc::new(Generation::load_managed(
        Arc::clone(&pin),
        key,
        manifest_size,
        permit,
        &limits,
    )?);
    let retained = match memory {
        Some(memory) => memory.retain(mapped)?,
        None => retained.ok_or_else(|| Error::corrupt("generation memory protection missing"))?,
    };
    pin.retain_memory(retained)?;
    let mut cache = cache
        .lock()
        .map_err(|_| Error::corrupt("managed generation cache poisoned"))?;
    if let Some(generation) = cache.get(&identity).and_then(Weak::upgrade) {
        return Ok(generation);
    }
    cache.retain(|_, generation| generation.strong_count() != 0);
    cache.insert(identity, Arc::downgrade(&generation));
    Ok(generation)
}

/// A read-only, OS-protected open does not take the namespace writer/daemon lock.
/// The returned generation, bases and escaped readers all retain the guard.
pub fn open_generation(namespace: &Path, incarnation: &Id) -> Result<Arc<Generation>> {
    let directory = Directory::open(namespace)?;
    let header: NamespaceHeader = directory.read_json("namespace.json", 64 * 1024)?;
    if header.schema != super::STORAGE_VERSION
        || header.directory_identity != directory.identity()?
    {
        return Err(Error::incompatible("namespace schema or identity differs"));
    }
    let mut connection = connect(&directory, false)?;
    if directory.observe_file("catalog.sqlite")?.identity != header.catalog_identity {
        return Err(Error::corrupt("reader catalog identity changed"));
    }
    let (catalog_namespace, schema): (String, u32) = connection.query_row(
        "SELECT namespace,schema FROM state WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if catalog_namespace != header.namespace.as_str() || schema != header.schema {
        return Err(Error::corrupt(
            "reader catalog belongs to another namespace",
        ));
    }
    ensure_admission(&connection)?;
    let object = object_row(&connection, incarnation)?;
    if object.state != ObjectState::Published || object.kind != ObjectKind::Generation {
        return Err(Error::new(
            ErrorCategory::CacheEvicted,
            "generation-unavailable",
            "generation is not published",
        ));
    }
    let pin = ObjectGuard::acquire(
        &header.namespace,
        &directory,
        &directory.child("guards")?,
        &directory.child("objects")?,
        incarnation,
        object
            .guard_identity
            .as_ref()
            .ok_or_else(|| Error::corrupt("object guard is unsealed"))?,
    )
    .map_err(|error| match object_row(&connection, incarnation) {
        Ok(current)
            if matches!(
                current.state,
                ObjectState::Retired | ObjectState::PendingDeletion | ObjectState::Removed
            ) =>
        {
            Error::new(
                ErrorCategory::CacheEvicted,
                "generation-retired",
                "generation was withdrawn before guard acquisition",
            )
        }
        Err(current) if current.category == ErrorCategory::CacheEvicted => current,
        Ok(_) => error,
        Err(current) => current,
    })?;
    ensure_admission(&connection)?;
    let object = object_row(&connection, incarnation)?;
    if object.state != ObjectState::Published
        || object.directory_identity != Some(pin.directory.identity()?)
    {
        return Err(Error::new(
            ErrorCategory::CacheEvicted,
            "generation-retired",
            "generation changed during protected open",
        ));
    }
    let (transaction, _) = super::catalog_io::begin(&mut connection, &directory, None)?;
    ensure_admission(&transaction)?;
    touch_object(&transaction, incarnation)?;
    transaction.commit()?;
    let policy: String =
        connection.query_row("SELECT policy FROM state WHERE singleton=1", [], |row| {
            row.get(0)
        })?;
    let policy: super::Policy = serde_json::from_str(&policy)?;
    policy.validate()?;
    let key = object
        .generation
        .ok_or_else(|| Error::corrupt("generation object has no logical key"))?;
    if key.repository_identity() != header.repository {
        return Err(Error::corrupt("generation belongs to another repository"));
    }
    load(pin, &key, policy.work.private_work_bytes, None)
}

impl Namespace {
    pub fn open_generation(&self, incarnation: &Id) -> Result<Arc<Generation>> {
        self.open_generation_controlled(incarnation, None)
    }

    pub(crate) fn open_generation_controlled(
        &self,
        incarnation: &Id,
        permit: Option<&Arc<WorkPermit>>,
    ) -> Result<Arc<Generation>> {
        let pin = self.pin(incarnation)?;
        let object = self.object(incarnation)?;
        if object.kind != ObjectKind::Generation {
            return Err(Error::invalid("incarnation is not a generation"));
        }
        let key = object
            .generation
            .ok_or_else(|| Error::corrupt("generation has no logical key"))?;
        if key.repository_identity() != self.header().repository {
            return Err(Error::corrupt("generation belongs to another repository"));
        }
        load(
            pin,
            &key,
            self.policy()?.policy.work.private_work_bytes,
            permit,
        )
    }

    pub fn generation_key(
        &self,
        repository: &Repository,
        commit: &str,
        profile: IndexingProfile,
    ) -> Result<(String, GenerationKey)> {
        self.generation_key_controlled(
            repository,
            commit,
            profile,
            &super::process::Control::bootstrap(),
        )
    }

    pub(crate) fn generation_key_controlled(
        &self,
        repository: &Repository,
        commit: &str,
        profile: IndexingProfile,
        control: &super::process::Control,
    ) -> Result<(String, GenerationKey)> {
        if repository.identity() != self.header().repository {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "repository-mismatch",
                "repository does not own this namespace",
            ));
        }
        let (commit, tree) = repository.resolve_controlled(commit, control)?;
        Ok((commit, GenerationKey::managed(repository, tree, profile)?))
    }

    pub fn find_generation(&self, key: &GenerationKey) -> Result<Option<Id>> {
        if key.repository_identity() != self.header().repository {
            return Err(Error::invalid(
                "generation key belongs to another repository",
            ));
        }
        self.read(|connection| {
            let id: Option<String> = connection.query_row(
                "SELECT id FROM objects WHERE kind='generation' AND logical_key=?1 AND state='published'",
                [text(key)?], |row| row.get(0),
            ).optional()?;
            id.map(Id::parse).transpose()
        })
    }

    pub fn ensure_generation(
        self: &Arc<Self>,
        repository: &Repository,
        exact_commit: &str,
        profile: IndexingProfile,
        predecessor: Option<&Arc<Generation>>,
        permit: &Arc<WorkPermit>,
    ) -> Result<Materialization> {
        if !Arc::ptr_eq(self, &permit.namespace) {
            return Err(Error::invalid(
                "reservation belongs to another namespace owner",
            ));
        }
        permit.check()?;
        let (commit, key) = self.generation_key_controlled(
            repository,
            exact_commit,
            profile,
            &super::process::Control::work(permit),
        )?;
        if commit != exact_commit {
            return Err(Error::invalid(
                "managed generation construction requires an exact commit",
            ));
        }
        if let Some(previous) = predecessor
            && (previous.key().repository_identity() != key.repository_identity()
                || previous.key().profile() != key.profile()
                || previous.key().index_format() != key.index_format())
        {
            return Err(Error::incompatible(
                "predecessor repository/profile/format differs",
            ));
        }
        if let Some(id) = self.find_generation(&key)? {
            let generation = self.open_generation_controlled(&id, Some(permit))?;
            return Ok(Materialization {
                descriptor: GenerationDescriptor {
                    namespace: self.header().namespace.clone(),
                    incarnation: id,
                    key,
                    requested_commit: commit,
                    fingerprint: generation.base().snapshot_id(),
                    stats: BuildStats {
                        reused_generation: true,
                        tracked_entries: generation.entries().len(),
                        ..BuildStats::default()
                    },
                },
                generation,
            });
        }
        let (record, pin) =
            self.create_object(ObjectKind::Generation, Some(key.clone()), permit)?;
        let result = (|| {
            let output = Output::managed(Arc::clone(&pin), Arc::clone(permit));
            let mut stats =
                GenerationManager::build_into(repository, &key, &commit, predecessor, &output)?;
            self.fault(
                super::faults::Point::GenerationBuilt,
                Some(permit.operation_id()),
            )?;
            let generation = load(Arc::clone(&pin), &key, permit.private_limit(), Some(permit))?;
            for name in [
                "paths.tgm",
                "lookup.tgm",
                "postings.tgm",
                "meta.tgm",
                "generation.tgm",
            ] {
                pin.directory.open_file(name, true)?.sync_all()?;
                self.record_file(&record.id, name)?;
            }
            pin.directory.sync()?;
            permit.check()?;
            let fingerprint = generation.base().snapshot_id();
            self.publish_object(&record.id, Some(fingerprint), None)?;
            self.fault(
                super::faults::Point::GenerationPublished,
                Some(permit.operation_id()),
            )
            .map_err(|error| error.committed(super::CommitState::Committed))?;
            stats.published = true;
            Ok(Materialization {
                descriptor: GenerationDescriptor {
                    namespace: self.header().namespace.clone(),
                    incarnation: record.id.clone(),
                    key,
                    requested_commit: commit,
                    fingerprint,
                    stats,
                },
                generation,
            })
        })();
        if let Err(error) = &result {
            // A committed publication is never rolled back, even if the response is lost.
            self.transaction(|transaction| {
                let mut object = object_row(transaction, &record.id)?;
                if object.state == ObjectState::Preparing {
                    object.state = ObjectState::Retired;
                    object.error = Some(serde_json::to_value(error)?);
                    save_object(transaction, &mut object)?;
                }
                Ok(())
            })
            .map_err(|cleanup| cleanup.operation(permit.operation_id().to_string()))?;
        }
        result
    }
}
