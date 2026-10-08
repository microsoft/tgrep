// Copyright (c) Microsoft Corporation. All rights reserved.

use super::lifetime::{ActivityGuard, ObjectGuard, OwnerProof, OwnerSeal, lock_error, owner_proof};
use super::policy::{Policy, StorageMode};
use super::storage::{Directory, allocated_bytes};
use super::{
    CommitState, Error, ErrorCategory, FileIdentity, Id, Measurement, ObjectKind, ObjectState,
    OperationState, OperationToken, OwnerClaim, ReferenceKind, Result, STORAGE_VERSION,
    STORE_DIRECTORY,
};
use crate::generations::{GenerationKey, Repository};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

const SCHEMA: &str = "
CREATE TABLE state (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 namespace TEXT NOT NULL,
 schema INTEGER NOT NULL,
 revision INTEGER NOT NULL CHECK(revision>=0),
 policy_version INTEGER NOT NULL CHECK(policy_version>0),
 policy TEXT NOT NULL,
 object_sequence INTEGER NOT NULL DEFAULT 0 CHECK(object_sequence>=0),
 admission TEXT NOT NULL CHECK(admission IN ('open','closed','recovering'))
);
CREATE TABLE objects (
 id TEXT PRIMARY KEY,
 kind TEXT NOT NULL,
 logical_key TEXT,
 state TEXT NOT NULL,
 use_sequence INTEGER NOT NULL DEFAULT 0,
 created_revision INTEGER NOT NULL,
 revision INTEGER NOT NULL,
 record TEXT NOT NULL
);
CREATE UNIQUE INDEX published_generation ON objects(logical_key)
 WHERE kind='generation' AND state IN ('preparing','published');
CREATE INDEX object_candidates ON objects(state,kind,id);
CREATE INDEX object_lru ON objects(state,kind,use_sequence,id);
CREATE TABLE object_history (
 object_id TEXT NOT NULL REFERENCES objects(id),
 revision INTEGER NOT NULL,
 record TEXT NOT NULL,
 PRIMARY KEY(object_id,revision)
);
CREATE TABLE members (
 object_id TEXT NOT NULL REFERENCES objects(id),
 name TEXT NOT NULL,
 record TEXT NOT NULL,
 PRIMARY KEY(object_id,name)
);
CREATE TABLE member_creations (
 object_id TEXT NOT NULL REFERENCES objects(id),
 name TEXT NOT NULL,
 PRIMARY KEY(object_id,name)
);
CREATE TABLE refs (
 id TEXT PRIMARY KEY,
 source_kind TEXT NOT NULL,
 source_id TEXT NOT NULL,
 target TEXT NOT NULL REFERENCES objects(id),
 owner TEXT
);
CREATE INDEX refs_target ON refs(target);
CREATE INDEX refs_source ON refs(source_kind,source_id);
CREATE TABLE owners (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE control_files (
 area TEXT NOT NULL, name TEXT NOT NULL, source TEXT NOT NULL, record TEXT NOT NULL,
 PRIMARY KEY(area,name)
);
CREATE INDEX control_source ON control_files(source);
CREATE TABLE scopes (
 id TEXT PRIMARY KEY, floor INTEGER NOT NULL DEFAULT 0 CHECK(floor>=0),
 closed INTEGER NOT NULL DEFAULT 0 CHECK(closed IN (0,1))
);
CREATE TABLE operations (
 id TEXT PRIMARY KEY,
 scope TEXT NOT NULL REFERENCES scopes(id),
 sequence INTEGER NOT NULL,
 token TEXT NOT NULL,
 digest TEXT NOT NULL,
 state TEXT NOT NULL,
 record TEXT NOT NULL,
 UNIQUE(scope,sequence),
 UNIQUE(scope,token)
);
CREATE TABLE records (
 kind TEXT NOT NULL, id TEXT NOT NULL, version INTEGER NOT NULL, record TEXT NOT NULL,
 PRIMARY KEY(kind,id)
);
CREATE INDEX operations_pending ON operations(id) WHERE state IN ('accepted','preparing','cancelling');
CREATE TABLE reservations (
 id TEXT PRIMARY KEY, operation_id TEXT NOT NULL, owner TEXT,
 bytes INTEGER NOT NULL CHECK(bytes>=0), slots INTEGER NOT NULL CHECK(slots>=0),
 record TEXT NOT NULL
);
";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceHeader {
    pub schema: u32,
    pub namespace: Id,
    pub repository: String,
    pub storage: StorageMode,
    pub directory_identity: FileIdentity,
    pub owner_identity: FileIdentity,
    pub activity_identity: FileIdentity,
    pub catalog_identity: FileIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionedPolicy {
    pub version: u64,
    pub policy: Policy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectRecord {
    pub id: Id,
    pub kind: ObjectKind,
    pub state: ObjectState,
    pub generation: Option<GenerationKey>,
    pub directory_identity: Option<FileIdentity>,
    pub guard_identity: Option<FileIdentity>,
    pub fingerprint: Option<[u8; 32]>,
    pub binding: Option<serde_json::Value>,
    pub logical_bytes: u64,
    pub accounting_floor: u64,
    pub allocated_bytes: Measurement<u64>,
    pub use_sequence: u64,
    pub idle_evidence: Option<serde_json::Value>,
    pub owner: Option<Id>,
    pub operation: Option<Id>,
    pub revision: u64,
    pub error: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    pub name: String,
    pub identity: FileIdentity,
    pub change: [i64; 4],
    pub producer_open: bool,
    pub logical_bytes: u64,
    pub allocated_bytes: Measurement<u64>,
    pub pending_length: Option<u64>,
    pub removed: bool,
    pub credited_logical_bytes: u64,
    pub credited_allocated_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerRecord {
    pub claim: OwnerClaim,
    pub registered: bool,
    pub released: bool,
    pub last_proof: OwnerProof,
    pub last_error: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRecord {
    pub id: Id,
    pub instance: Id,
    pub token: OperationToken,
    pub kind: String,
    pub request: serde_json::Value,
    pub state: OperationState,
    pub committed_state: CommitState,
    pub cancelled: bool,
    pub progress: serde_json::Value,
    pub result: Option<serde_json::Value>,
    pub error: Option<serde_json::Value>,
    pub(crate) accepted_at: super::clock::Stamp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogCursor {
    pub namespace: Id,
    pub instance: Id,
    pub ticket: Id,
    pub revision: u64,
    pub after: Option<Id>,
}

#[derive(Debug, Serialize)]
pub struct CatalogPage {
    pub revision: u64,
    pub examined: usize,
    pub objects: Vec<ObjectRecord>,
    pub next: Option<CatalogCursor>,
}

pub(super) struct CursorLifetime {
    pub(super) revision: u64,
    pub(super) expires: Instant,
    _activity: ActivityGuard,
}

pub struct Namespace {
    pub(crate) directory: Arc<Directory>,
    pub(crate) objects: Arc<Directory>,
    pub(crate) guards: Arc<Directory>,
    pub(crate) owners: Arc<Directory>,
    header: NamespaceHeader,
    instance: Id,
    database: Mutex<Connection>,
    pub(super) cursors: Mutex<HashMap<Id, CursorLifetime>>,
    pub(super) inventories: super::inventory::Inventories,
    pub(super) observations: Mutex<super::diagnostics::Observations>,
    pub(crate) maintenance: Mutex<()>,
    pub(crate) metadata_operations: Mutex<()>,
    pub(crate) system_operations: Mutex<()>,
    pub(crate) operation_readers: Arc<Mutex<HashMap<Id, u32>>>,
    pub(super) live_views: Mutex<HashMap<Id, Vec<Weak<super::ViewSlot>>>>,
    pub(crate) memory: Arc<super::memory::MemoryAccount>,
    pub(crate) clock: Arc<dyn super::clock::Clock>,
    #[cfg(any(test, feature = "managed-test-hooks"))]
    pub(super) faults: super::faults::Faults,
    _owner: File,
}

pub(crate) fn text<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

pub(crate) fn sql_integer(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::invalid("integer exceeds catalog range"))
}

pub(crate) fn ensure_object_sealed(connection: &Connection, id: &Id) -> Result<()> {
    let incomplete: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM member_creations WHERE object_id=?1)
         OR EXISTS(SELECT 1 FROM members WHERE object_id=?1 AND json_extract(record,'$.producer_open')=1)",
        [id.as_str()], |row| row.get(0),
    )?;
    if incomplete {
        return Err(Error::busy("object-producers-not-sealed"));
    }
    Ok(())
}

pub(crate) fn unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn tag<T: Serialize>(value: &T) -> Result<String> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(value) => Ok(value),
        _ => Err(Error::corrupt("catalog tag is not a string")),
    }
}

pub(crate) fn next_revision(transaction: &Transaction<'_>) -> Result<u64> {
    Ok(transaction.query_row(
        "UPDATE state SET revision=revision+1 WHERE singleton=1 AND revision<9223372036854775807 RETURNING revision",
        [],
        |row| unsigned(row, 0),
    )?)
}

pub(crate) fn object_row(connection: &Connection, id: &Id) -> Result<ObjectRecord> {
    let record: Option<String> = connection
        .query_row(
            "SELECT record FROM objects WHERE id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let record = match record {
        Some(record) => record,
        None => {
            let (namespace, through): (String, u64) = connection.query_row(
                "SELECT namespace,object_sequence FROM state WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, unsigned(row, 1)?)),
            )?;
            return Err(if id.allocated_object(&namespace, through) {
                Error::new(
                    ErrorCategory::CacheEvicted,
                    "object-collected",
                    "the physical incarnation was collected",
                )
            } else {
                Error::new(
                    ErrorCategory::CacheMissing,
                    "object-missing",
                    "no such managed incarnation",
                )
            });
        }
    };
    if record.len() > super::MAX_REQUEST_BYTES {
        return Err(Error::corrupt("object metadata exceeds its record bound"));
    }
    let record: ObjectRecord = serde_json::from_str(&record)?;
    if record.id != *id {
        return Err(Error::corrupt("object row identity differs from its key"));
    }
    Ok(record)
}

pub(crate) fn save_object(transaction: &Transaction<'_>, object: &mut ObjectRecord) -> Result<()> {
    object.revision = next_revision(transaction)?;
    let record = text(object)?;
    let key = object.generation.as_ref().map(text).transpose()?;
    transaction.execute(
        "UPDATE objects SET kind=?2,logical_key=?3,state=?4,revision=?5,record=?6,use_sequence=?7 WHERE id=?1",
        params![object.id.as_str(), tag(&object.kind)?, key, tag(&object.state)?, sql_integer(object.revision)?, record, sql_integer(object.use_sequence)?],
    )?;
    transaction.execute(
        "INSERT INTO object_history(object_id,revision,record) VALUES(?1,?2,?3)",
        params![object.id.as_str(), sql_integer(object.revision)?, record],
    )?;
    Ok(())
}

pub(crate) fn touch_object(transaction: &Transaction<'_>, id: &Id) -> Result<()> {
    let mut object = object_row(transaction, id)?;
    if object.state != ObjectState::Published {
        return Err(Error::new(
            ErrorCategory::CacheEvicted,
            "object-retired",
            "object was withdrawn before access registration",
        ));
    }
    object.use_sequence = next_revision(transaction)?;
    object.idle_evidence = None;
    save_object(transaction, &mut object)
}

pub(crate) fn connect(directory: &Directory, read_only: bool) -> Result<Connection> {
    directory.verify()?;
    let identity = directory.observe_file("catalog.sqlite")?.identity;
    let flags = (if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    }) | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let connection = Connection::open_with_flags(directory.path().join("catalog.sqlite"), flags)?;
    super::catalog_io::configure(&connection, read_only)?;
    if directory.observe_file("catalog.sqlite")?.identity != identity {
        return Err(Error::corrupt("catalog file was replaced during open"));
    }
    directory.verify()?;
    Ok(connection)
}

impl Namespace {
    pub fn initialize(
        repository: &Repository,
        storage: &Path,
        policy: Policy,
    ) -> Result<Arc<Self>> {
        policy.validate()?;
        let storage = std::fs::canonicalize(storage)?;
        repository.validate_managed_storage(&storage)?;
        Self::initialize_identity(repository.identity(), &storage, policy)
    }

    pub(super) fn initialize_identity(
        repository: &str,
        storage: &Path,
        policy: Policy,
    ) -> Result<Arc<Self>> {
        if repository.len() != 64 || !repository.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::invalid("invalid repository identity"));
        }
        let parent = Directory::open(storage)?;
        let area = parent.open_or_create_child(STORE_DIRECTORY)?;
        let directory = match area.create_child(repository) {
            Ok(directory) => directory,
            Err(error) if error.source_io_kind() == Some(std::io::ErrorKind::AlreadyExists) => {
                let existing = Self::open(area.child(repository)?.path())?;
                if existing.header.repository != repository || existing.policy()?.policy != policy {
                    return Err(Error::incompatible(
                        "existing namespace identity/policy differs; use a version-checked policy update",
                    ));
                }
                return Ok(existing);
            }
            Err(error) => return Err(error),
        };
        let owner = directory.create_file("owner.lock")?;
        fs2::FileExt::try_lock_exclusive(&owner)
            .map_err(|error| lock_error(error, "namespace-owned"))?;
        for name in ["activity.lock", "catalog.sqlite"] {
            directory.create_file(name)?.sync_all()?;
        }
        let objects = directory.create_child("objects")?;
        let guards = directory.create_child("guards")?;
        let owners = directory.create_child("owners")?;
        let header = NamespaceHeader {
            schema: STORAGE_VERSION,
            namespace: Id::new()?,
            repository: repository.into(),
            storage: policy.storage,
            directory_identity: directory.identity()?,
            owner_identity: FileIdentity::of(&owner)?,
            activity_identity: directory.observe_file("activity.lock")?.identity,
            catalog_identity: directory.observe_file("catalog.sqlite")?.identity,
        };
        let mut connection = connect(&directory, false)?;
        Self::configure_database(&connection)?;
        let transaction = super::catalog_io::begin_initial(&mut connection, &directory, &policy)?;
        transaction.execute_batch(SCHEMA)?;
        super::accounting::install_counters(&transaction)?;
        transaction.execute(
            "INSERT INTO state(singleton,namespace,schema,revision,policy_version,policy,admission) VALUES(1,?1,?2,0,1,?3,'open')",
            params![header.namespace.as_str(), STORAGE_VERSION, text(&policy)?],
        )?;
        transaction.execute(
            "INSERT INTO records VALUES('allocation','namespace',1,?1)",
            [text(&super::Allocation::local(&policy))?],
        )?;
        transaction.commit()?;
        directory.create_json("namespace.json", &header)?;
        Ok(Arc::new(Self {
            memory: super::memory::MemoryAccount::for_namespace(&header.namespace)?,
            directory,
            objects,
            guards,
            owners,
            header,
            instance: Id::new()?,
            database: Mutex::new(connection),
            cursors: Mutex::new(HashMap::new()),
            inventories: Mutex::new(HashMap::new()),
            observations: Mutex::new(super::diagnostics::Observations::default()),
            maintenance: Mutex::new(()),
            metadata_operations: Mutex::new(()),
            system_operations: Mutex::new(()),
            operation_readers: Arc::new(Mutex::new(HashMap::new())),
            live_views: Mutex::new(HashMap::new()),
            clock: Arc::new(super::clock::SystemClock),
            #[cfg(any(test, feature = "managed-test-hooks"))]
            faults: super::faults::Faults::default(),
            _owner: owner,
        }))
    }

    fn configure_database(connection: &Connection) -> Result<()> {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "fullfsync", true)?;
        connection.pragma_update(None, "checkpoint_fullfsync", true)?;
        connection.pragma_update(None, "wal_autocheckpoint", 0)?;
        let journal: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let sync: u32 = connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        let foreign: bool =
            connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
        if journal != "wal" || sync != 2 || !foreign {
            return Err(Error::incompatible(
                "required SQLite durability/foreign-key settings are unavailable",
            ));
        }
        Ok(())
    }

    /// Acquire authoritative storage ownership without discovering or opening Git.
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let directory = Directory::open(path)?;
        let header: NamespaceHeader = directory.read_json("namespace.json", 64 * 1024)?;
        if header.schema != STORAGE_VERSION || header.directory_identity != directory.identity()? {
            return Err(Error::incompatible(
                "namespace schema or physical identity differs",
            ));
        }
        let owner = directory.open_file("owner.lock", true)?;
        if FileIdentity::of(&owner)? != header.owner_identity {
            return Err(Error::corrupt("namespace ownership lock was replaced"));
        }
        fs2::FileExt::try_lock_exclusive(&owner)
            .map_err(|error| lock_error(error, "namespace-owned"))?;
        let connection = connect(&directory, false)?;
        if directory.observe_file("catalog.sqlite")?.identity != header.catalog_identity {
            return Err(Error::corrupt("namespace catalog was replaced"));
        }
        Self::configure_database(&connection)?;
        let (namespace, schema): (String, u32) = connection.query_row(
            "SELECT namespace,schema FROM state WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if namespace != header.namespace.as_str() || schema != header.schema {
            return Err(Error::corrupt("namespace and catalog identities differ"));
        }
        super::accounting::counters(&connection)?;
        let result = Arc::new(Self {
            memory: super::memory::MemoryAccount::for_namespace(&header.namespace)?,
            objects: directory.child("objects")?,
            guards: directory.child("guards")?,
            owners: directory.child("owners")?,
            directory,
            header,
            instance: Id::new()?,
            database: Mutex::new(connection),
            cursors: Mutex::new(HashMap::new()),
            inventories: Mutex::new(HashMap::new()),
            observations: Mutex::new(super::diagnostics::Observations::default()),
            maintenance: Mutex::new(()),
            metadata_operations: Mutex::new(()),
            system_operations: Mutex::new(()),
            operation_readers: Arc::new(Mutex::new(HashMap::new())),
            live_views: Mutex::new(HashMap::new()),
            clock: Arc::new(super::clock::SystemClock),
            #[cfg(any(test, feature = "managed-test-hooks"))]
            faults: super::faults::Faults::default(),
            _owner: owner,
        });
        result.policy()?.policy.validate()?;
        result.allocation()?;
        Ok(result)
    }

    pub fn header(&self) -> &NamespaceHeader {
        &self.header
    }
    pub fn instance(&self) -> &Id {
        &self.instance
    }
    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    pub(crate) fn read<T>(&self, action: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.verify_identity()?;
        let connection = self
            .database
            .lock()
            .map_err(|_| Error::corrupt("catalog lock poisoned"))?;
        let result = action(&connection)?;
        self.directory.verify()?;
        Ok(result)
    }

    pub(crate) fn transaction<T>(
        &self,
        action: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.transaction_with_policy(None, action)
    }

    pub(crate) fn transaction_with_policy<T>(
        &self,
        change: Option<VersionedPolicy>,
        action: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.verify_identity()?;
        let mut connection = self
            .database
            .lock()
            .map_err(|_| Error::corrupt("catalog lock poisoned"))?;
        let (transaction, mut budget) =
            match super::catalog_io::begin(&mut connection, &self.directory, change.as_ref()) {
                Ok(result) => result,
                Err(error) => {
                    self.observe_catalog_error(&error)?;
                    return Err(error);
                }
            };
        self.observe_catalog_write(&budget)?;
        let result = action(&transaction)?;
        budget.cache_used_before_commit_bytes = super::catalog_io::cache_used(&transaction);
        self.observe_catalog_write(&budget)?;
        self.fault(super::faults::Point::CatalogBeforeCommit, None)?;
        transaction
            .commit()
            .map_err(|error| Error::from(error).committed(CommitState::Unknown))?;
        self.fault(super::faults::Point::CatalogAfterCommit, None)
            .map_err(|error| error.committed(CommitState::Committed))?;
        self.directory
            .verify()
            .map_err(|error| error.committed(CommitState::Committed))?;
        Ok(result)
    }

    fn verify_identity(&self) -> Result<()> {
        self.directory.verify()?;
        for (name, expected) in [
            ("owner.lock", &self.header.owner_identity),
            ("activity.lock", &self.header.activity_identity),
            ("catalog.sqlite", &self.header.catalog_identity),
        ] {
            if &self.directory.observe_file(name)?.identity != expected {
                return Err(Error::corrupt(format!(
                    "namespace anchor {name} was replaced"
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn admit_metadata(&self) -> Result<()> {
        let limit = self.policy()?.policy.work.metadata_bytes;
        self.read(|connection| super::catalog_io::admit_control(connection, limit, 0))?;
        if self
            .catalog_file_bytes()?
            .checked_add(self.control_logical_bytes()?)
            .is_none_or(|bytes| bytes > limit)
        {
            return Err(Error::pressure("catalog-metadata-allocation"));
        }
        Ok(())
    }

    pub fn policy(&self) -> Result<VersionedPolicy> {
        self.read(|connection| {
            let (version, policy): (u64, String) = connection.query_row(
                "SELECT policy_version,policy FROM state WHERE singleton=1",
                [],
                |row| Ok((unsigned(row, 0)?, row.get(1)?)),
            )?;
            Ok(VersionedPolicy {
                version,
                policy: serde_json::from_str(&policy)?,
            })
        })
    }

    pub fn update_policy(&self, expected: u64, policy: Policy) -> Result<VersionedPolicy> {
        policy.validate()?;
        if policy.storage != self.header.storage {
            return Err(Error::incompatible("namespace storage mode is immutable"));
        }
        self.transaction_with_policy(
            Some(VersionedPolicy {
                version: expected,
                policy: policy.clone(),
            }),
            |transaction| {
                let current: u64 = transaction.query_row(
                    "SELECT policy_version FROM state WHERE singleton=1",
                    [],
                    |row| unsigned(row, 0),
                )?;
                if expected != current {
                    return Err(Error::stale_version(current));
                }
                let version = current
                    .checked_add(1)
                    .filter(|version| *version <= i64::MAX as u64)
                    .ok_or_else(|| Error::corrupt("policy version exhausted"))?;
                transaction.execute(
                    "UPDATE state SET policy_version=?1,policy=?2 WHERE singleton=1",
                    params![sql_integer(version)?, text(&policy)?],
                )?;
                next_revision(transaction)?;
                Ok(VersionedPolicy { version, policy })
            },
        )
    }

    pub fn object(&self, id: &Id) -> Result<ObjectRecord> {
        self.read(|connection| object_row(connection, id))
    }

    pub(crate) fn create_object(
        &self,
        kind: ObjectKind,
        generation: Option<GenerationKey>,
        permit: &Arc<super::WorkPermit>,
    ) -> Result<(ObjectRecord, Arc<ObjectGuard>)> {
        if !std::ptr::eq(self, Arc::as_ptr(&permit.namespace)) {
            return Err(Error::invalid(
                "object reservation belongs to another namespace",
            ));
        }
        permit.check()?;
        self.create_object_inner(
            kind,
            generation,
            permit.record.owner.clone(),
            Some(permit.operation_id().clone()),
            Some(permit),
        )
    }

    fn create_object_inner(
        &self,
        kind: ObjectKind,
        generation: Option<GenerationKey>,
        owner: Option<Id>,
        operation: Option<Id>,
        permit: Option<&Arc<super::WorkPermit>>,
    ) -> Result<(ObjectRecord, Arc<ObjectGuard>)> {
        let _activity = ActivityGuard::acquire(&self.directory)?;
        self.admit_metadata()?;
        let mut id = Id::object(self.header.namespace.as_str(), 0);
        let mut object = ObjectRecord {
            id: id.clone(),
            kind,
            state: ObjectState::Preparing,
            generation,
            directory_identity: None,
            guard_identity: None,
            fingerprint: None,
            binding: None,
            logical_bytes: 0,
            accounting_floor: permit.map_or(0, |permit| permit.staging_limit()),
            allocated_bytes: Measurement::Observed { value: 0 },
            use_sequence: 0,
            idle_evidence: None,
            owner,
            operation,
            revision: 0,
            error: None,
        };
        self.transaction(|transaction| {
            super::work::ensure_admission(transaction)?;
            if kind == ObjectKind::Generation {
                let pending: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM objects WHERE kind='generation' AND logical_key=?1 AND state IN ('preparing','published'))",
                    [object.generation.as_ref().map(text).transpose()?], |row| row.get(0),
                )?;
                if pending { return Err(Error::busy("generation-already-materializing")); }
            }
            let sequence = transaction.query_row(
                "UPDATE state SET object_sequence=object_sequence+1
                 WHERE singleton=1 AND object_sequence<9223372036854775807 RETURNING object_sequence",
                [], |row| unsigned(row, 0),
            )?;
            id = Id::object(self.header.namespace.as_str(), sequence);
            object.id = id.clone();
            let revision = next_revision(transaction)?;
            object.revision = revision;
            object.use_sequence = revision;
            transaction.execute(
                "INSERT INTO objects(id,kind,logical_key,state,created_revision,revision,record,use_sequence) VALUES(?1,?2,?3,'preparing',?4,?4,?5,?4)",
                params![id.as_str(), tag(&kind)?, object.generation.as_ref().map(text).transpose()?, sql_integer(revision)?, text(&object)?],
            )?;
            transaction.execute("INSERT INTO object_history VALUES(?1,?2,?3)", params![id.as_str(), sql_integer(revision)?, text(&object)?])?;
            Ok(())
        })?;
        let result = (|| {
            self.fault(
                super::faults::Point::ObjectIntentSaved,
                object.operation.as_ref(),
            )?;
            let directory = self.objects.create_child(id.as_str())?;
            self.fault(
                super::faults::Point::ObjectDirectoryCreated,
                object.operation.as_ref(),
            )?;
            object.directory_identity = Some(directory.identity()?);
            self.transaction(|transaction| save_object(transaction, &mut object))?;
            self.fault(
                super::faults::Point::ObjectDirectorySealed,
                object.operation.as_ref(),
            )?;
            let guard = self.guards.create_file(&format!("{id}.lock"))?;
            guard.sync_all()?;
            self.fault(
                super::faults::Point::ObjectGuardCreated,
                object.operation.as_ref(),
            )?;
            let guard_identity = FileIdentity::of(&guard)?;
            let pin = ObjectGuard::acquire(
                &self.header.namespace,
                &self.directory,
                &self.guards,
                &self.objects,
                &id,
                &guard_identity,
            )?;
            object.guard_identity = Some(guard_identity);
            object.accounting_floor = 0;
            self.transaction(|transaction| {
                super::housekeeping::register_control(
                    transaction,
                    &self.guards,
                    "guards",
                    &format!("{id}.lock"),
                    id.as_str(),
                )?;
                save_object(transaction, &mut object)
            })?;
            self.fault(
                super::faults::Point::ObjectGuardSealed,
                object.operation.as_ref(),
            )?;
            let seal = serde_json::json!({
                "namespace":self.header.namespace, "object":id, "identity":object.directory_identity,
                "guard":object.guard_identity
            });
            if let Some(permit) = permit {
                let mut writer = super::work::ChargedWriter::new(
                    Arc::clone(&pin),
                    "object.json",
                    Arc::clone(permit),
                )?;
                serde_json::to_writer(&mut writer, &seal)?;
                writer.sync_all()?;
            } else {
                self.begin_member(&id, "object.json")?;
                directory.create_json("object.json", &seal)?;
                self.record_file(&id, "object.json")?;
            }
            Ok((self.object(&id)?, pin))
        })();
        if let Err(error) = &result {
            self.transaction(|transaction| {
                let mut object = object_row(transaction, &id)?;
                object.state =
                    if object.guard_identity.is_some() && object.directory_identity.is_some() {
                        ObjectState::Retired
                    } else {
                        ObjectState::Quarantined
                    };
                object.error = Some(serde_json::to_value(error)?);
                save_object(transaction, &mut object)
            })?;
        }
        result
    }

    pub(crate) fn pin(&self, id: &Id) -> Result<Arc<ObjectGuard>> {
        self.pin_inner(id, false)
    }

    fn pin_inner(&self, id: &Id, preparing: bool) -> Result<Arc<ObjectGuard>> {
        let object = self.object(id)?;
        if object.state != ObjectState::Published
            && !(preparing && object.state == ObjectState::Preparing)
        {
            return Err(Error::new(
                ErrorCategory::CacheEvicted,
                "object-not-published",
                "incarnation is not published",
            ));
        }
        let expected = object
            .guard_identity
            .as_ref()
            .ok_or_else(|| Error::corrupt("object guard is unsealed"))?;
        let pin = ObjectGuard::acquire(
            &self.header.namespace,
            &self.directory,
            &self.guards,
            &self.objects,
            id,
            expected,
        )
        .map_err(|error| match self.object(id) {
            Ok(current)
                if matches!(
                    current.state,
                    ObjectState::Retired | ObjectState::PendingDeletion | ObjectState::Removed
                ) =>
            {
                Error::new(
                    ErrorCategory::CacheEvicted,
                    "object-retired",
                    "the incarnation was withdrawn before open",
                )
            }
            Err(current) if current.category == ErrorCategory::CacheEvicted => current,
            Ok(_) => error,
            Err(current) => current,
        })?;
        self.read(super::work::ensure_admission)?;
        let object = self.object(id)?;
        let admissible = object.state == ObjectState::Published
            || (preparing && object.state == ObjectState::Preparing);
        if !admissible {
            return Err(Error::new(
                ErrorCategory::CacheEvicted,
                "object-not-published",
                "the selected incarnation is retired, removed or not ready",
            ));
        }
        if object.directory_identity.as_ref() != Some(&pin.directory.identity()?) {
            return Err(Error::corrupt("object directory identity changed"));
        }
        if object.state == ObjectState::Published {
            self.transaction(|transaction| {
                super::work::ensure_admission(transaction)?;
                touch_object(transaction, id)
            })?;
        }
        Ok(pin)
    }

    pub(crate) fn begin_member(&self, id: &Id, name: &str) -> Result<()> {
        self.admit_metadata()?;
        self.transaction(|transaction| {
            if object_row(transaction, id)?.state != ObjectState::Preparing {
                return Err(Error::busy("object-is-not-preparing"));
            }
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM members WHERE object_id=?1 AND name=?2)",
                params![id.as_str(), name],
                |row| row.get(0),
            )?;
            if exists {
                return Err(Error::busy("member-already-exists"));
            }
            transaction.execute(
                "INSERT INTO member_creations VALUES(?1,?2)",
                params![id.as_str(), name],
            )?;
            Ok(())
        })
    }

    pub(crate) fn record_file(&self, id: &Id, name: &str) -> Result<FileRecord> {
        self.record_file_state(id, name, false)
    }

    pub(crate) fn record_file_state(
        &self,
        id: &Id,
        name: &str,
        producer_open: bool,
    ) -> Result<FileRecord> {
        let directory = self.objects.child(id.as_str())?;
        let file = directory.open_file(name, false)?;
        let record = FileRecord {
            name: name.into(),
            identity: FileIdentity::of(&file)?,
            change: super::storage::file_change(&file)?,
            producer_open,
            logical_bytes: file.metadata()?.len(),
            allocated_bytes: allocated_bytes(&file)?,
            pending_length: None,
            removed: false,
            credited_logical_bytes: 0,
            credited_allocated_bytes: 0,
        };
        self.transaction(|transaction| {
            let previous: Option<String> = transaction.query_row(
                "SELECT record FROM members WHERE object_id=?1 AND name=?2",
                params![id.as_str(), name], |row| row.get(0),
            ).optional()?;
            if let Some(previous) = previous {
                let previous: FileRecord = serde_json::from_str(&previous)?;
                if previous.identity != record.identity || previous.removed || previous.pending_length.is_some()
                    || previous.credited_logical_bytes != 0
                    || (!previous.producer_open && (previous.change != record.change
                        || previous.logical_bytes != record.logical_bytes || producer_open))
                {
                    return Err(Error::new(ErrorCategory::StaleIdentity, "producer-member-modified",
                        "sealed or withdrawn output differs from its producer inventory"));
                }
            } else {
                let creating: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM member_creations WHERE object_id=?1 AND name=?2)",
                    params![id.as_str(), name], |row| row.get(0),
                )?;
                if !creating { return Err(Error::corrupt("producer file has no creation intent")); }
            }
            let mut object = object_row(transaction, id)?;
            if matches!(object.state, ObjectState::PendingDeletion | ObjectState::Removed | ObjectState::Quarantined) {
                return Err(Error::busy("producer-object-withdrawn"));
            }
            transaction.execute(
                "INSERT INTO members VALUES(?1,?2,?3) ON CONFLICT(object_id,name) DO UPDATE SET record=excluded.record",
                params![id.as_str(), name, text(&record)?],
            )?;
            transaction.execute("DELETE FROM member_creations WHERE object_id=?1 AND name=?2", params![id.as_str(), name])?;
            let mut statement = transaction.prepare("SELECT record FROM members WHERE object_id=?1")?;
            let rows = statement.query_map([id.as_str()], |row| row.get::<_, String>(0))?;
            let mut logical = 0_u64;
            let mut allocated = 0_u64;
            let mut unavailable = None;
            for row in rows {
                let member: FileRecord = serde_json::from_str(&row?)?;
                logical = logical.checked_add(member.logical_bytes).ok_or_else(|| Error::corrupt("logical accounting overflow"))?;
                match member.allocated_bytes {
                    Measurement::Observed { value } => {
                        allocated = allocated.checked_add(value).ok_or_else(|| Error::corrupt("allocation accounting overflow"))?;
                    }
                    Measurement::Unavailable { reason } => unavailable = Some(reason),
                }
            }
            drop(statement);
            object.logical_bytes = logical;
            object.allocated_bytes = match unavailable {
                Some(reason) => Measurement::Unavailable { reason },
                None => Measurement::Observed { value: allocated },
            };
            save_object(transaction, &mut object)?;
            Ok(record)
        })
    }

    pub(crate) fn publish_object(
        &self,
        id: &Id,
        fingerprint: Option<[u8; 32]>,
        binding: Option<serde_json::Value>,
    ) -> Result<ObjectRecord> {
        self.transaction(|transaction| {
            let mut object = object_row(transaction, id)?;
            if object.state != ObjectState::Preparing || object.directory_identity.is_none() {
                return Err(Error::corrupt(
                    "object cannot be published from its current state",
                ));
            }
            ensure_object_sealed(transaction, id)?;
            object.state = ObjectState::Published;
            object.fingerprint = fingerprint;
            object.binding = binding;
            save_object(transaction, &mut object)?;
            Ok(object)
        })
    }

    pub(crate) fn add_reference(
        transaction: &Transaction<'_>,
        id: &Id,
        kind: ReferenceKind,
        source: &str,
        target: &Id,
        owner: Option<&Id>,
    ) -> Result<()> {
        if object_row(transaction, target)?.state != ObjectState::Published {
            return Err(Error::new(
                ErrorCategory::CacheEvicted,
                "reference-target-retired",
                "cannot reference an unpublished object",
            ));
        }
        transaction.execute(
            "INSERT INTO refs VALUES(?1,?2,?3,?4,?5)",
            params![
                id.as_str(),
                tag(&kind)?,
                source,
                target.as_str(),
                owner.map(Id::as_str)
            ],
        )?;
        Ok(())
    }

    pub fn retain(&self, target: &Id) -> Result<Id> {
        let _pin = self.pin(target)?;
        let id = Id::new()?;
        self.transaction(|transaction| {
            Self::add_reference(
                transaction,
                &id,
                ReferenceKind::Persistent,
                id.as_str(),
                target,
                None,
            )?;
            Ok(id)
        })
    }

    pub fn release_reference(&self, id: &Id) -> Result<bool> {
        self.transaction(|transaction| {
            Ok(transaction.execute(
                "DELETE FROM refs WHERE id=?1 AND source_kind='persistent'",
                [id.as_str()],
            )? != 0)
        })
    }

    pub fn prepare_owner(&self) -> Result<OwnerRecord> {
        self.prepare_owner_with_token(&super::Token::parse(Id::new()?.to_string())?)
    }

    pub fn prepare_owner_with_token(&self, token: &super::Token) -> Result<OwnerRecord> {
        let _activity = ActivityGuard::acquire(&self.directory)?;
        let metadata_admission = self.admit_metadata();
        let key = format!("{}:{}", self.instance, token.as_str());
        self.transaction(|transaction| {
            let previous: Option<String> = transaction
                .query_row(
                    "SELECT record FROM records WHERE kind='owner-bootstrap' AND id=?1",
                    [&key],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(previous) = previous {
                let id: Id = serde_json::from_str(&previous)?;
                let record: Option<String> = transaction
                    .query_row(
                        "SELECT record FROM owners WHERE id=?1",
                        [id.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?;
                return record
                    .map(|record| Ok(serde_json::from_str(&record)?))
                    .unwrap_or_else(|| {
                        Err(Error::new(
                            ErrorCategory::ReceiptExpired,
                            "owner-bootstrap-retired",
                            "this bootstrap token belongs to a released owner",
                        )
                        .committed(CommitState::Committed))
                    });
            }
            metadata_admission?;
            super::work::ensure_admission(transaction)?;
            let configured: String =
                transaction.query_row("SELECT policy FROM state WHERE singleton=1", [], |row| {
                    row.get(0)
                })?;
            let limits = serde_json::from_str::<Policy>(&configured)?.work;
            let count: u64 = transaction.query_row(
                "SELECT count(*) FROM owners WHERE json_extract(record,'$.released')=0",
                [],
                |row| unsigned(row, 0),
            )?;
            let receipts: u64 = transaction.query_row(
                "SELECT count(*) FROM records WHERE kind='owner-bootstrap'",
                [],
                |row| unsigned(row, 0),
            )?;
            if count >= u64::from(limits.max_leases) || receipts >= u64::from(limits.max_receipts) {
                return Err(Error::pressure("owner-registration-limit"));
            }
            super::catalog_io::admit_control(transaction, limits.metadata_bytes, 4096)?;
            let id = Id::new()?;
            let file = self.owners.create_file(&format!("{id}.lock"))?;
            file.sync_all()?;
            let claim = OwnerClaim {
                namespace: self.header.namespace.clone(),
                instance: self.instance.clone(),
                owner: id.clone(),
                challenge: Id::new()?,
                storage: self.path().to_path_buf(),
                guard_identity: FileIdentity::of(&file)?,
            };
            self.owners.create_json(
                &format!("{id}.json"),
                &OwnerSeal {
                    namespace: claim.namespace.clone(),
                    instance: claim.instance.clone(),
                    owner: id.clone(),
                    challenge: claim.challenge.clone(),
                },
            )?;
            for name in [format!("{id}.lock"), format!("{id}.json")] {
                super::housekeeping::register_control(
                    transaction,
                    &self.owners,
                    "owners",
                    &name,
                    id.as_str(),
                )?;
            }
            let owner = OwnerRecord {
                claim,
                registered: false,
                released: false,
                last_proof: OwnerProof::Unknown,
                last_error: None,
            };
            transaction.execute(
                "INSERT INTO owners VALUES(?1,?2)",
                params![id.as_str(), text(&owner)?],
            )?;
            transaction.execute("INSERT INTO scopes(id) VALUES(?1)", [id.as_str()])?;
            transaction.execute(
                "INSERT INTO records VALUES('owner-bootstrap',?1,1,?2)",
                params![key, text(&id)?],
            )?;
            Ok(owner)
        })
    }

    pub fn owner(&self, id: &Id) -> Result<OwnerRecord> {
        self.read(|connection| {
            let record: Option<String> = connection
                .query_row(
                    "SELECT record FROM owners WHERE id=?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            let record = record.ok_or_else(|| {
                Error::new(
                    ErrorCategory::ReceiptExpired,
                    "owner-unavailable",
                    "owner is unknown or has been retired",
                )
            })?;
            Ok(serde_json::from_str(&record)?)
        })
    }

    pub fn register_owner(&self, claim: &OwnerClaim) -> Result<OwnerRecord> {
        let mut record = self.owner(&claim.owner)?;
        if claim.namespace != self.header.namespace
            || claim.instance != self.instance
            || text(claim)? != text(&record.claim)?
            || record.released
        {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "stale-owner",
                "owner claim is not valid for this daemon instance",
            ));
        }
        if owner_proof(&self.directory, claim)? != OwnerProof::Held {
            return Err(Error::busy("owner-guard-not-held"));
        }
        record.registered = true;
        record.last_proof = OwnerProof::Held;
        self.transaction(|transaction| {
            super::work::ensure_admission(transaction)?;
            let current: String = transaction.query_row(
                "SELECT record FROM owners WHERE id=?1",
                [claim.owner.as_str()],
                |row| row.get(0),
            )?;
            if serde_json::from_str::<OwnerRecord>(&current)?.released {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "owner-released",
                    "owner was released before registration",
                ));
            }
            transaction.execute(
                "UPDATE owners SET record=?2 WHERE id=?1",
                params![claim.owner.as_str(), text(&record)?],
            )?;
            Ok(record)
        })
    }

    pub fn inspect_owner(&self, id: &Id) -> Result<OwnerRecord> {
        let mut record = self.owner(id)?;
        match owner_proof(&self.directory, &record.claim) {
            Ok(proof) => {
                record.last_proof = proof;
                record.last_error = None;
            }
            Err(error) => {
                record.last_proof = OwnerProof::Unknown;
                record.last_error = Some(serde_json::to_value(error)?);
            }
        }
        Ok(record)
    }

    pub fn accept_operation(
        &self,
        token: OperationToken,
        kind: &str,
        request: serde_json::Value,
    ) -> Result<OperationRecord> {
        token.validate()?;
        let limits = self.policy()?.policy.work;
        let metadata_admission =
            if matches!(kind, "collection" | "recovery" | "metadata" | "idle-stop") {
                Ok(())
            } else {
                self.admit_metadata()
            };
        let accepted_at = self.clock.now()?;
        let encoded = text(&serde_json::json!({"kind":kind,"request":request}))?;
        if encoded.len() > super::MAX_REQUEST_BYTES || kind.is_empty() || kind.len() > 64 {
            return Err(Error::invalid(
                "operation request exceeds its encoded size limit",
            ));
        }
        let digest = blake3::hash(encoded.as_bytes()).to_hex().to_string();
        self.transaction(|transaction| {
            let scope: Option<(u64, bool)> = transaction.query_row("SELECT floor,closed FROM scopes WHERE id=?1",
                [token.scope.as_str()], |row| Ok((unsigned(row, 0)?, row.get(1)?))).optional()?;
            let Some((floor, closed)) = scope else {
                return Err(Error::new(ErrorCategory::ReceiptExpired, "unknown-operation-scope", "operation scope is unknown or was retired").committed(CommitState::Unknown));
            };
            if token.sequence <= floor {
                return Err(Error::new(ErrorCategory::ReceiptExpired, "receipt-expired", "operation scope/sequence cannot be replayed as new work").committed(CommitState::Unknown));
            }
            let prior: Option<(String, String)> = transaction.query_row(
                "SELECT digest,record FROM operations WHERE scope=?1 AND (sequence=?2 OR token=?3)",
                params![token.scope.as_str(), sql_integer(token.sequence)?, token.token.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            if let Some((previous_digest, record)) = prior {
                let operation: OperationRecord = serde_json::from_str(&record)?;
                if previous_digest != digest || operation.token != token {
                    return Err(Error::new(ErrorCategory::StaleVersion, "operation-token-conflict", "token/sequence already identifies different work"));
                }
                return Ok(operation);
            }
            if closed {
                return Err(Error::new(ErrorCategory::ReceiptExpired, "scope-closed", "closed scopes cannot accept new operations").committed(CommitState::Unknown));
            }
            metadata_admission?;
            if token.scope != self.instance && token.scope != self.header.namespace {
                let owner: String = transaction.query_row("SELECT record FROM owners WHERE id=?1",
                    [token.scope.as_str()], |row| row.get(0))?;
                let owner: OwnerRecord = serde_json::from_str(&owner)?;
                if owner.claim.instance != self.instance || owner.released || !owner.registered {
                    return Err(Error::new(ErrorCategory::StaleIdentity, "inactive-operation-owner", "new operations require a registered current-instance owner"));
                }
            }
            super::work::ensure_admission(transaction)?;
            let count: u64 = transaction.query_row("SELECT count(*) FROM operations", [], |row| unsigned(row, 0))?;
            if count >= u64::from(limits.max_receipts) { return Err(Error::pressure("operation-receipt-limit")); }
            let queued: u64 = transaction.query_row(
                "SELECT count(*) FROM operations WHERE state IN ('accepted','preparing','cancelling')",
                [], |row| unsigned(row, 0),
            )?;
            if queued >= u64::from(limits.queue_items) + u64::from(limits.workers) {
                return Err(Error::pressure("operation-queue-limit"));
            }
            let operation = OperationRecord {
                id: Id::new()?, instance: self.instance.clone(), token, kind: kind.into(), request, state: OperationState::Accepted,
                committed_state: CommitState::NotCommitted, cancelled: false,
                progress: serde_json::json!({}), result: None, error: None,
                accepted_at,
            };
            transaction.execute("INSERT INTO operations VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![operation.id.as_str(), operation.token.scope.as_str(), sql_integer(operation.token.sequence)?,
                    operation.token.token.as_str(), digest, tag(&operation.state)?, text(&operation)?])?;
            Ok(operation)
        })
    }

    pub fn operation(&self, id: &Id) -> Result<OperationRecord> {
        self.read(|connection| {
            let record: Option<String> = connection
                .query_row(
                    "SELECT record FROM operations WHERE id=?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            let record = record.ok_or_else(|| {
                Error::new(
                    ErrorCategory::ReceiptExpired,
                    "operation-receipt-unavailable",
                    "the operation is unknown or its receipt was acknowledged",
                )
                .committed(CommitState::Unknown)
            })?;
            Ok(serde_json::from_str(&record)?)
        })
    }

    pub(crate) fn save_operation(
        transaction: &Transaction<'_>,
        operation: &OperationRecord,
    ) -> Result<()> {
        transaction.execute(
            "UPDATE operations SET state=?2,record=?3 WHERE id=?1",
            params![
                operation.id.as_str(),
                tag(&operation.state)?,
                text(operation)?
            ],
        )?;
        Ok(())
    }

    pub fn cancel_operation(&self, id: &Id) -> Result<OperationRecord> {
        self.transaction(|transaction| {
            let record: String = transaction.query_row(
                "SELECT record FROM operations WHERE id=?1",
                [id.as_str()],
                |row| row.get(0),
            )?;
            let mut operation: OperationRecord = serde_json::from_str(&record)?;
            if operation.committed_state == CommitState::NotCommitted
                || (matches!(operation.kind.as_str(), "collection" | "recovery")
                    && !matches!(
                        operation.state,
                        OperationState::Completed
                            | OperationState::Cancelled
                            | OperationState::Failed
                    ))
            {
                operation.cancelled = true;
                if !matches!(
                    operation.state,
                    OperationState::Completed | OperationState::Cancelled | OperationState::Failed
                ) {
                    operation.state = OperationState::Cancelling;
                }
                Self::save_operation(transaction, &operation)?;
            }
            Ok(operation)
        })
    }

    pub fn page(&self, cursor: Option<CatalogCursor>) -> Result<CatalogPage> {
        let activity = ActivityGuard::acquire(&self.directory)?;
        self.check_inspection_admission()?;
        self.read(|connection| {
            if let Err(error) = super::work::ensure_admission(connection) {
                let service: Option<String> = connection
                    .query_row(
                        "SELECT record FROM records WHERE kind='service' AND id='current'",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;
                let service: Option<serde_json::Value> = service
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?;
                if service
                    .as_ref()
                    .is_none_or(|service| service["instance"] == serde_json::json!(self.instance()))
                {
                    return Err(error);
                }
            }
            Ok(())
        })?;
        let work = self.policy()?.policy.work;
        let mut cursors = self
            .cursors
            .lock()
            .map_err(|_| Error::corrupt("cursor lock poisoned"))?;
        cursors.retain(|_, lifetime| lifetime.expires > Instant::now());
        let mut cursor = match cursor {
            Some(cursor) => {
                if cursor.namespace != self.header.namespace
                    || cursor.instance != self.instance
                    || cursors
                        .get(&cursor.ticket)
                        .is_none_or(|lifetime| lifetime.revision != cursor.revision)
                {
                    return Err(Error::new(
                        ErrorCategory::StaleIdentity,
                        "cursor-expired",
                        "start a new bounded catalog snapshot",
                    ));
                }
                cursor
            }
            None => {
                self.expire_inventories()?;
                let inventory_count = self
                    .inventories
                    .lock()
                    .map_err(|_| Error::corrupt("inventory registry poisoned"))?
                    .len();
                if cursors.len() + inventory_count >= work.max_cursors as usize {
                    return Err(Error::pressure("catalog-cursor-limit"));
                }
                let revision = self.read(|connection| {
                    Ok(connection.query_row(
                        "SELECT revision FROM state WHERE singleton=1",
                        [],
                        |row| unsigned(row, 0),
                    )?)
                })?;
                let cursor = CatalogCursor {
                    namespace: self.header.namespace.clone(),
                    instance: self.instance.clone(),
                    ticket: Id::new()?,
                    revision,
                    after: None,
                };
                cursors.insert(
                    cursor.ticket.clone(),
                    CursorLifetime {
                        revision,
                        expires: Instant::now()
                            .checked_add(Duration::from_millis(work.cursor_lifetime_ms))
                            .ok_or_else(|| Error::invalid("cursor deadline overflow"))?,
                        _activity: activity,
                    },
                );
                cursor
            }
        };
        let page = self.read(|connection| {
            let mut statement = connection.prepare("SELECT id,created_revision FROM objects WHERE id>?1 ORDER BY id LIMIT ?2")?;
            let rows = statement.query_map(params![cursor.after.as_ref().map_or("", Id::as_str), work.page_objects],
                |row| Ok((row.get::<_, String>(0)?, unsigned(row, 1)?)))?;
            let mut objects = Vec::new();
            let mut examined = 0;
            for row in rows {
                let (id, created) = row?;
                examined += 1;
                cursor.after = Some(Id::parse(id.clone())?);
                if created <= cursor.revision {
                    let record: String = connection.query_row(
                        "SELECT record FROM object_history WHERE object_id=?1 AND revision<=?2 ORDER BY revision DESC LIMIT 1",
                        params![id, sql_integer(cursor.revision)?], |row| row.get(0),
                    )?;
                    let object: ObjectRecord = serde_json::from_str(&record)?;
                    if object.state != ObjectState::Removed { objects.push(object); }
                }
            }
            let more: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM objects WHERE id>?1)",
                [cursor.after.as_ref().map_or("", Id::as_str)], |row| row.get(0))?;
            Ok(CatalogPage { revision: cursor.revision, examined, objects, next: more.then(|| cursor.clone()) })
        })?;
        if page.next.is_none() {
            cursors.remove(&cursor.ticket);
        }
        Ok(page)
    }

    pub fn close_cursor(&self, cursor: &CatalogCursor) -> Result<bool> {
        if cursor.namespace != self.header.namespace || cursor.instance != self.instance {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "cursor-identity-mismatch",
                "cursor belongs to another namespace or instance",
            ));
        }
        let mut cursors = self
            .cursors
            .lock()
            .map_err(|_| Error::corrupt("cursor lock poisoned"))?;
        Ok(cursors.remove(&cursor.ticket).is_some())
    }

    pub(crate) fn expire_cursors(&self) -> Result<()> {
        self.cursors
            .lock()
            .map_err(|_| Error::corrupt("cursor lock poisoned"))?
            .retain(|_, cursor| cursor.expires > Instant::now());
        self.expire_inventories()
    }

    pub(super) fn check_inspection_admission(&self) -> Result<()> {
        self.read(|connection| {
            let stopped: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM records WHERE kind='service' AND id='current'
                 AND json_extract(record,'$.instance')=?1 AND json_extract(record,'$.stopping')=1)",
                [self.instance.as_str()],
                |row| row.get(0),
            )?;
            if stopped {
                Err(Error::busy("stopped-instance"))
            } else {
                Ok(())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{OwnerGuard, Token};
    use super::*;
    #[cfg(unix)]
    use rusqlite::TransactionBehavior;

    fn namespace() -> (tempfile::TempDir, Arc<Namespace>) {
        let temp = tempfile::tempdir().unwrap();
        let namespace = Namespace::initialize_identity(
            &"a".repeat(64),
            temp.path(),
            super::super::policy::fixture_policy(),
        )
        .unwrap();
        (temp, namespace)
    }

    #[test]
    fn independent_concurrent_policy_replay_preserves_original_receipt() {
        let (_temp, namespace) = namespace();
        let owner = namespace.prepare_owner().unwrap();
        let guard = OwnerGuard::claim(owner.claim).unwrap();
        namespace.register_owner(guard.registration()).unwrap();
        for sequence in 1..=16 {
            let configured = namespace.policy().unwrap();
            let operation = namespace
                .accept_metadata_mutation(
                    OperationToken {
                        scope: guard.registration().owner.clone(),
                        sequence,
                        token: Token::parse(format!("policy-replay-{sequence}")).unwrap(),
                    },
                    super::super::MetadataMutation::Policy {
                        expected_version: configured.version,
                        policy: configured.policy,
                    },
                )
                .unwrap();
            let start = std::sync::Barrier::new(9);
            let results = std::thread::scope(|scope| {
                let workers: Vec<_> = (0..8)
                    .map(|_| {
                        scope.spawn(|| {
                            start.wait();
                            namespace.execute_metadata_mutation(&operation.id)
                        })
                    })
                    .collect();
                start.wait();
                workers
                    .into_iter()
                    .map(|worker| worker.join().unwrap().unwrap())
                    .collect::<Vec<_>>()
            });
            let authoritative = namespace.operation(&operation.id).unwrap();
            assert_eq!(authoritative.state, OperationState::Completed);
            assert!(authoritative.error.is_none(), "{:?}", authoritative.error);
            for receipt in results {
                assert_eq!(
                    serde_json::to_value(receipt).unwrap(),
                    serde_json::to_value(&authoritative).unwrap(),
                );
            }
            assert_eq!(namespace.policy().unwrap().version, configured.version + 1);
        }
    }

    #[test]
    fn independent_queued_policy_replay_ignores_obsolete_headroom() {
        let (_temp, namespace) = namespace();
        let owner = namespace.prepare_owner().unwrap();
        let guard = OwnerGuard::claim(owner.claim).unwrap();
        namespace.register_owner(guard.registration()).unwrap();
        let mut configured = namespace.policy().unwrap();
        configured.policy.work.metadata_bytes = 1024 * 1024;
        let operation = namespace
            .accept_metadata_mutation(
                OperationToken {
                    scope: guard.registration().owner.clone(),
                    sequence: 1,
                    token: Token::parse("small-policy").unwrap(),
                },
                super::super::MetadataMutation::Policy {
                    expected_version: configured.version,
                    policy: configured.policy.clone(),
                },
            )
            .unwrap();
        let original = namespace.execute_metadata_mutation(&operation.id).unwrap();
        assert_eq!(original.state, OperationState::Completed);
        assert!(original.error.is_none());
        std::thread::scope(|scope| {
            let serial = namespace.metadata_operations.lock().unwrap();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (finished_tx, finished_rx) = std::sync::mpsc::channel();
            let namespace = &namespace;
            let id = &operation.id;
            let worker = scope.spawn(move || {
                started_tx.send(()).unwrap();
                finished_tx
                    .send(namespace.execute_metadata_mutation(id))
                    .unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !namespace.operation_readers.lock().unwrap().contains_key(id) {
                assert!(
                    Instant::now() < deadline,
                    "queued replay did not retain its receipt"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut current = namespace.policy().unwrap();
            current.policy.work.metadata_bytes = 4 * 1024 * 1024;
            let raised = namespace
                .update_policy(current.version, current.policy)
                .unwrap();
            let reader = connect(&namespace.directory, true).unwrap();
            reader
                .execute_batch("BEGIN; SELECT namespace FROM state")
                .unwrap();
            let old_limit = configured.policy.work.metadata_bytes;
            let mut crossed = false;
            for sequence in 0..512 {
                namespace
                    .transaction(|transaction| {
                        transaction.execute(
                            "INSERT INTO records VALUES('probe','replay-pressure',1,?1)
                             ON CONFLICT(kind,id) DO UPDATE SET version=version+1,record=excluded.record",
                            [sequence.to_string()],
                        )?;
                        Ok(())
                    })
                    .unwrap();
                if namespace
                    .directory
                    .observe_file("catalog.sqlite-wal")
                    .unwrap()
                    .logical_bytes
                    > old_limit
                {
                    crossed = true;
                    break;
                }
            }
            assert!(crossed);
            let mut entered = false;
            let rejected = namespace
                .transaction_with_policy(
                    Some(VersionedPolicy {
                        version: raised.version,
                        policy: configured.policy,
                    }),
                    |_| {
                        entered = true;
                        Ok(())
                    },
                )
                .unwrap_err();
            assert!(!entered);
            assert_eq!(rejected.reason_code, "catalog-checkpoint-readers-active");
            assert_eq!(rejected.committed_state, CommitState::NotCommitted);
            let forgotten = namespace
                .acknowledge_operations(&operation.token.scope, operation.token.sequence)
                .unwrap_err();
            assert_eq!(forgotten.reason_code, "operation-result-still-in-use");
            assert!(!worker.is_finished(), "replay bypassed the execution gate");
            drop(serial);
            let replay = finished_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            worker.join().unwrap();
            assert_eq!(
                serde_json::to_value(replay).unwrap(),
                serde_json::to_value(original).unwrap(),
            );
            assert!(!reader.is_autocommit());
            reader.execute_batch("ROLLBACK").unwrap();
            reader.close().unwrap();
        });
        assert_eq!(
            namespace
                .acknowledge_operations(&operation.token.scope, operation.token.sequence)
                .unwrap(),
            operation.token.sequence,
        );
    }

    #[test]
    fn independent_distinct_policy_operations_preserve_real_cas_failure() {
        let (_temp, namespace) = namespace();
        let owner = namespace.prepare_owner().unwrap();
        let guard = OwnerGuard::claim(owner.claim).unwrap();
        namespace.register_owner(guard.registration()).unwrap();
        let configured = namespace.policy().unwrap();
        let operations: Vec<_> = (1..=2)
            .map(|sequence| {
                namespace
                    .accept_metadata_mutation(
                        OperationToken {
                            scope: guard.registration().owner.clone(),
                            sequence,
                            token: Token::parse(format!("conflicting-policy-{sequence}")).unwrap(),
                        },
                        super::super::MetadataMutation::Policy {
                            expected_version: configured.version,
                            policy: configured.policy.clone(),
                        },
                    )
                    .unwrap()
            })
            .collect();
        let start = Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = operations
            .iter()
            .map(|operation| {
                let namespace = Arc::clone(&namespace);
                let start = Arc::clone(&start);
                let id = operation.id.clone();
                std::thread::spawn(move || {
                    start.wait();
                    namespace.execute_metadata_mutation(&id)
                })
            })
            .collect();
        start.wait();
        let results: Vec<_> = workers.into_iter().map(|worker| worker.join()).collect();
        let receipts: Vec<_> = results
            .into_iter()
            .map(|result| result.unwrap().unwrap())
            .collect();
        assert_eq!(
            receipts
                .iter()
                .filter(|receipt| receipt.state == OperationState::Completed)
                .count(),
            1,
        );
        let failed = receipts
            .iter()
            .find(|receipt| receipt.state == OperationState::Failed)
            .unwrap();
        assert_eq!(failed.committed_state, CommitState::NotCommitted);
        let error = failed.error.as_ref().unwrap();
        assert_eq!(error["reason_code"], "stale-version");
        assert_eq!(error["current_version"], configured.version + 1);
        for receipt in &receipts {
            let replay = namespace.execute_metadata_mutation(&receipt.id).unwrap();
            assert_eq!(
                serde_json::to_value(replay).unwrap(),
                serde_json::to_value(receipt).unwrap(),
            );
        }
        assert_eq!(namespace.policy().unwrap().version, configured.version + 1);
    }

    #[test]
    fn independent_metadata_execution_reports_real_commit_boundary_errors() {
        use super::super::faults::{Action, Point, Specification};
        for point in [Point::CatalogBeforeCommit, Point::CatalogAfterCommit] {
            let (_temp, namespace) = namespace();
            let owner = namespace.prepare_owner().unwrap();
            let guard = OwnerGuard::claim(owner.claim).unwrap();
            namespace.register_owner(guard.registration()).unwrap();
            let configured = namespace.policy().unwrap();
            let operation = namespace
                .accept_metadata_mutation(
                    OperationToken {
                        scope: guard.registration().owner.clone(),
                        sequence: 1,
                        token: Token::parse("metadata-commit-boundary").unwrap(),
                    },
                    super::super::MetadataMutation::Policy {
                        expected_version: configured.version,
                        policy: configured.policy,
                    },
                )
                .unwrap();
            namespace
                .install_test_fault(Specification {
                    point,
                    operation: None,
                    skip_hits: 0,
                    action: Action::Error {
                        category: ErrorCategory::Io,
                    },
                })
                .unwrap();
            let response = namespace.execute_metadata_mutation(&operation.id);
            let committed = point == Point::CatalogAfterCommit;
            let original = if committed {
                let error = response.unwrap_err();
                assert_eq!(error.category, ErrorCategory::Io);
                assert_eq!(error.committed_state, CommitState::Committed);
                assert_eq!(error.operation_id.as_deref(), Some(operation.id.as_str()));
                let receipt = namespace.operation(&operation.id).unwrap();
                assert_eq!(receipt.state, OperationState::Completed);
                assert_eq!(receipt.committed_state, CommitState::Committed);
                assert!(receipt.error.is_none());
                receipt
            } else {
                let receipt = response.unwrap();
                assert_eq!(receipt.state, OperationState::Failed);
                assert_eq!(receipt.committed_state, CommitState::NotCommitted);
                let error = receipt.error.as_ref().unwrap();
                assert_eq!(error["reason_code"], "injected-test-failure");
                assert_eq!(error["operation_id"], operation.id.as_str());
                receipt
            };
            let replay = namespace.execute_metadata_mutation(&operation.id).unwrap();
            assert_eq!(
                serde_json::to_value(replay).unwrap(),
                serde_json::to_value(original).unwrap(),
            );
            assert_eq!(
                namespace.policy().unwrap().version,
                configured.version + u64::from(committed),
            );
        }
    }

    #[test]
    fn wal_checkpoint_contention_precedes_mutation_not_a_successful_commit() {
        let (_temp, namespace) = namespace();
        let mut configured = namespace.policy().unwrap();
        configured.policy.work.metadata_bytes = 1024 * 1024;
        let limit = configured.policy.work.metadata_bytes;
        namespace
            .update_policy(configured.version, configured.policy)
            .unwrap();
        namespace
            .read(|connection| {
                let busy: u32 =
                    connection
                        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
                assert_eq!(busy, 0);
                Ok(())
            })
            .unwrap();
        let reader = connect(&namespace.directory, true).unwrap();
        reader
            .execute_batch("BEGIN; SELECT namespace FROM state")
            .unwrap();
        let mut crossed = false;
        for sequence in 0..256 {
            let bytes = namespace
                .directory
                .observe_file("catalog.sqlite-wal")
                .unwrap()
                .logical_bytes;
            crossed |= bytes >= limit / 4;
            let written = namespace
                .transaction(|transaction| {
                    transaction.execute(
                        "INSERT INTO records VALUES('probe','wal',1,?1)
                         ON CONFLICT(kind,id) DO UPDATE SET version=version+1,record=excluded.record",
                        [text(&serde_json::json!({"sequence":sequence,"padding":"x".repeat(8192)}))?],
                    )?;
                    Ok(())
                });
            if let Err(error) = written {
                assert_eq!(error.reason_code, "catalog-checkpoint-readers-active");
                assert_eq!(error.committed_state, CommitState::NotCommitted);
                break;
            }
        }
        assert!(crossed, "fixture did not reach the bounded WAL threshold");
        let mutation = |transaction: &Transaction<'_>| -> Result<()> {
            transaction.execute("INSERT INTO records VALUES('probe','next',1,'{}')", [])?;
            Ok(())
        };
        let mut entered = false;
        let blocked = namespace
            .transaction(|transaction| {
                entered = true;
                mutation(transaction)
            })
            .unwrap_err();
        assert!(!entered, "headroom rejection must precede the mutation");
        assert_eq!(blocked.reason_code, "catalog-checkpoint-readers-active");
        assert_eq!(blocked.committed_state, CommitState::NotCommitted);
        let usage = namespace.work_usage().unwrap();
        assert!(usage.catalog_file_bytes + usage.control_logical_bytes <= limit);
        let diagnostics = namespace.maintenance_diagnostics().unwrap();
        let budget = &diagnostics.details["last_catalog_write"];
        assert!(budget["pager_payload_limit_bytes"].as_u64().unwrap() <= limit / 3);
        assert_eq!(
            budget["cache_used_before_commit_bytes"]["status"],
            "observed"
        );
        assert!(
            !namespace
                .read(|connection| Ok(connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM records WHERE kind='probe' AND id='next')",
                    [],
                    |row| row.get::<_, bool>(0),
                )?))
                .unwrap()
        );
        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);
        namespace.transaction(mutation).unwrap();
        assert!(
            namespace
                .directory
                .observe_file("catalog.sqlite-wal")
                .unwrap()
                .logical_bytes
                < limit / 4
        );
    }

    #[cfg(unix)]
    #[test]
    fn catalog_probes_preserve_sqlite_posix_locks() {
        use super::super::process::{Control, PipedProcess};
        use std::os::fd::AsRawFd;
        use std::process::Command;

        const HELPER: &str = "managed::catalog::tests::catalog_probes_preserve_sqlite_posix_locks";
        if let Some(path) = std::env::var_os("TGREP_SQLITE_LOCK_PROBE") {
            let file = File::open(path).unwrap();
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = libc::F_WRLCK as _;
            lock.l_whence = libc::SEEK_SET as _;
            lock.l_start = std::env::var("TGREP_SQLITE_LOCK_START")
                .unwrap()
                .parse()
                .unwrap();
            lock.l_len = std::env::var("TGREP_SQLITE_LOCK_LENGTH")
                .unwrap()
                .parse()
                .unwrap();
            // SAFETY: the file is live and the output has the native flock layout.
            assert_ne!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) },
                -1,
                "{}",
                std::io::Error::last_os_error()
            );
            assert_ne!(
                i64::from(lock.l_type),
                i64::from(libc::F_UNLCK),
                "SQLite lock was released"
            );
            assert_eq!(
                lock.l_pid.to_string(),
                std::env::var("TGREP_SQLITE_LOCK_OWNER").unwrap(),
                "the lock must belong to the parent catalog connection"
            );
            return;
        }

        let (_temp, namespace) = namespace();
        let probe = |phase: &str, name: &str, start: u64, length: u64| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", HELPER, "--nocapture"])
                .env("TGREP_SQLITE_LOCK_PROBE", namespace.path().join(name))
                .env("TGREP_SQLITE_LOCK_START", start.to_string())
                .env("TGREP_SQLITE_LOCK_LENGTH", length.to_string())
                .env("TGREP_SQLITE_LOCK_OWNER", std::process::id().to_string());
            let mut child = PipedProcess::spawn(&mut command, Control::bootstrap(), false).unwrap();
            let output = child.read_output(64 * 1024).unwrap();
            let (status, diagnostics) = child.finish().unwrap();
            assert!(
                status.success(),
                "{phase}: {diagnostics}\n{}",
                String::from_utf8_lossy(&output)
            );
        };
        // SQLite's Unix VFS uses the database shared-byte range and WAL writer byte.
        let database_probe = |phase: &str| probe(phase, "catalog.sqlite", 0x4000_0002, 510);
        database_probe("initial publication");
        namespace.verify_identity().unwrap();
        database_probe("identity verification");
        namespace.work_usage().unwrap();
        database_probe("accounting");
        drop(connect(&namespace.directory, true).unwrap());
        database_probe("independent connection close");
        let owner = namespace.prepare_owner().unwrap();
        let guard = OwnerGuard::claim(owner.claim.clone()).unwrap();
        namespace.register_owner(guard.registration()).unwrap();
        database_probe("owner claim and registration");
        let mut cursor = None;
        loop {
            let page = namespace.inventory_page(cursor).unwrap();
            assert!(
                page.entries.iter().all(|entry| entry.error.is_none()),
                "{:?}",
                serde_json::to_value(&page).unwrap()
            );
            database_probe("inventory");
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        let mut connection = namespace.database.lock().unwrap();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        probe("active WAL writer", "catalog.sqlite-shm", 120, 1);
        namespace.catalog_file_bytes().unwrap();
        probe("accounting during WAL write", "catalog.sqlite-shm", 120, 1);
        transaction.rollback().unwrap();
    }

    #[test]
    fn catalog_owner_survives_without_a_repository() {
        let (_temp, namespace) = namespace();
        assert_eq!(
            Namespace::open(namespace.path()).err().unwrap().category,
            ErrorCategory::Busy
        );
        let path = namespace.path().to_path_buf();
        let identity = namespace.header.namespace.clone();
        drop(namespace);
        let reopened = Namespace::open(&path).unwrap();
        assert_eq!(reopened.header.namespace, identity);
    }

    #[test]
    fn catalog_versions_and_operation_replay_are_authoritative() {
        let (_temp, namespace) = namespace();
        let policy = namespace.policy().unwrap();
        namespace
            .update_policy(policy.version, policy.policy.clone())
            .unwrap();
        assert_eq!(
            namespace
                .update_policy(policy.version, policy.policy)
                .unwrap_err()
                .category,
            ErrorCategory::StaleVersion
        );
        let owner = namespace.prepare_owner().unwrap();
        let guard = OwnerGuard::claim(owner.claim.clone()).unwrap();
        namespace.register_owner(guard.registration()).unwrap();
        let token = OperationToken {
            scope: owner.claim.owner,
            sequence: 1,
            token: Token::parse("retained-before-send").unwrap(),
        };
        let first = namespace
            .accept_operation(
                token.clone(),
                "collect",
                serde_json::json!({"bounded":true}),
            )
            .unwrap();
        let replay = namespace
            .accept_operation(
                token.clone(),
                "collect",
                serde_json::json!({"bounded":true}),
            )
            .unwrap();
        assert_eq!(first.id, replay.id);
        assert!(
            namespace
                .accept_operation(token, "migrate", serde_json::json!({}))
                .is_err()
        );
    }

    #[test]
    fn catalog_failures_distinguish_rollback_from_committed_policy() {
        use super::super::faults::{Action, Point, Specification};
        for point in [Point::CatalogBeforeCommit, Point::CatalogAfterCommit] {
            let (_temp, namespace) = namespace();
            let previous = namespace.policy().unwrap();
            namespace
                .install_test_fault(Specification {
                    point,
                    operation: None,
                    skip_hits: 0,
                    action: Action::Error {
                        category: ErrorCategory::Io,
                    },
                })
                .unwrap();
            let error = namespace
                .update_policy(previous.version, previous.policy)
                .unwrap_err();
            assert_eq!(error.category, ErrorCategory::Io);
            let committed = point == Point::CatalogAfterCommit;
            assert_eq!(
                error.committed_state,
                if committed {
                    CommitState::Committed
                } else {
                    CommitState::NotCommitted
                }
            );
            assert_eq!(
                namespace.policy().unwrap().version,
                1 + u64::from(committed)
            );
        }
    }

    #[test]
    fn policy_reduction_reserves_its_own_publication_and_rejection_does_not_trap_updates() {
        let (_temp, namespace) = namespace();
        let owner = namespace.prepare_owner().unwrap();
        let guard = OwnerGuard::claim(owner.claim).unwrap();
        namespace.register_owner(guard.registration()).unwrap();
        let mut configured = namespace.policy().unwrap();
        configured.policy.work.metadata_bytes = 2 * 1024 * 1024;
        namespace
            .update_policy(configured.version, configured.policy)
            .unwrap();
        namespace
            .read(|connection| {
                assert_eq!(
                    connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row
                        .get::<_, u32>(0))?,
                    0,
                );
                Ok(())
            })
            .unwrap();
        let reader = connect(&namespace.directory, true).unwrap();
        reader
            .execute_batch("BEGIN; SELECT namespace FROM state")
            .unwrap();
        for sequence in 0..256 {
            namespace
                .transaction(|transaction| {
                    transaction.execute(
                        "INSERT INTO records VALUES('probe','resize',1,?1)
                     ON CONFLICT(kind,id) DO UPDATE SET version=version+1,record=excluded.record",
                        [text(&serde_json::json!({"sequence":sequence}))?],
                    )?;
                    Ok(())
                })
                .unwrap();
            if namespace
                .directory
                .observe_file("catalog.sqlite-wal")
                .unwrap()
                .logical_bytes
                >= 680 * 1024
            {
                break;
            }
        }
        let usage = namespace.work_usage().unwrap();
        assert!(usage.catalog_file_bytes + usage.control_logical_bytes < 1024 * 1024);
        let original = namespace.policy().unwrap();
        let mut reduced = original.policy.clone();
        reduced.work.metadata_bytes = 1024 * 1024;
        let rejected = namespace
            .update_policy(original.version, reduced.clone())
            .unwrap_err();
        assert_eq!(rejected.committed_state, CommitState::NotCommitted);
        assert_eq!(rejected.reason_code, "catalog-checkpoint-readers-active");
        assert_eq!(namespace.policy().unwrap().version, original.version);
        let operation = namespace
            .accept_metadata_mutation(
                OperationToken {
                    scope: guard.registration().owner.clone(),
                    sequence: 1,
                    token: Token::parse("resize-receipt").unwrap(),
                },
                super::super::MetadataMutation::Policy {
                    expected_version: original.version,
                    policy: reduced,
                },
            )
            .unwrap();
        let failed = namespace.execute_metadata_mutation(&operation.id).unwrap();
        assert_eq!(failed.state, OperationState::Failed);
        assert_eq!(failed.committed_state, CommitState::NotCommitted);
        assert_eq!(namespace.policy().unwrap().version, original.version);
        let mut raised = original.policy;
        raised.work.metadata_bytes = 4 * 1024 * 1024;
        let updated = namespace
            .update_policy(original.version, raised.clone())
            .unwrap();
        assert_eq!(updated.policy, raised);
        assert!(!reader.is_autocommit());
        reader.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn database_page_exhaustion_rolls_back_without_spilling_unbounded_wal() {
        let (_temp, namespace) = namespace();
        let mut configured = namespace.policy().unwrap();
        configured.policy.work.metadata_bytes = 1024 * 1024;
        namespace
            .update_policy(configured.version, configured.policy)
            .unwrap();
        let before = namespace
            .directory
            .observe_file("catalog.sqlite-wal")
            .unwrap()
            .logical_bytes;
        let error = namespace
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO records VALUES('probe','too-large',1,?1)",
                    ["x".repeat(1024 * 1024)],
                )?;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::ResourcePressure);
        assert_eq!(error.committed_state, CommitState::NotCommitted);
        let after = namespace
            .directory
            .observe_file("catalog.sqlite-wal")
            .unwrap()
            .logical_bytes;
        assert!(after <= before);
        namespace
            .read(|connection| {
                assert!(!connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM records WHERE kind='probe' AND id='too-large')",
                    [],
                    |row| row.get::<_, bool>(0),
                )?);
                Ok(())
            })
            .unwrap();
        namespace
            .transaction(|transaction| {
                transaction.execute("INSERT INTO records VALUES('probe','small',1,'{}')", [])?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn catalog_pages_preserve_old_object_versions() {
        let (_temp, namespace) = namespace();
        let mut policy = namespace.policy().unwrap();
        policy.policy.work.page_objects = 1;
        namespace
            .update_policy(policy.version, policy.policy)
            .unwrap();
        let (a, a_pin) = namespace
            .create_object_inner(ObjectKind::CheckpointStage, None, None, None, None)
            .unwrap();
        let (b, b_pin) = namespace
            .create_object_inner(ObjectKind::CheckpointStage, None, None, None, None)
            .unwrap();
        let page = namespace.page(None).unwrap();
        assert_eq!(page.objects.len(), 1);
        let next = page.next.unwrap();
        namespace.publish_object(&a.id, None, None).unwrap();
        namespace.publish_object(&b.id, None, None).unwrap();
        let last = namespace.page(Some(next)).unwrap();
        assert_eq!(last.objects[0].state, ObjectState::Preparing);
        drop((a_pin, b_pin));
    }
}
