// Copyright (c) Microsoft Corporation. All rights reserved.

use super::lifetime::{ActivityGuard, ObjectGuard};
use super::storage::{Directory, allocated_bytes};
use super::{
    Error, ErrorCategory, FileRecord, Id, Measurement, Namespace, NamespaceHeader, NativePath,
    Result,
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::ReadDir;
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

const PAGE_LIMIT: u32 = 256;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryCursor {
    pub namespace: Id,
    pub instance: Id,
    pub ticket: Id,
    pub sequence: u64,
}

#[derive(Serialize)]
pub struct InventoryEntry {
    pub path: NativePath,
    pub classification: String,
    pub logical_bytes: Measurement<u64>,
    pub allocated_bytes: Measurement<u64>,
    pub content_authenticated: Measurement<bool>,
    pub error: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct InventoryPage {
    pub inventory_schema: u32,
    pub consistency: &'static str,
    pub examined: u32,
    pub unknown_entries: u32,
    pub known_logical_bytes: u64,
    pub entries: Vec<InventoryEntry>,
    pub elapsed_nanos: u64,
    pub elapsed_budget_exceeded: bool,
    pub next: Option<InventoryCursor>,
}

enum Area {
    Root,
    Objects,
    Controls(&'static str),
    Object(Id),
}

struct Frame {
    directory: Arc<Directory>,
    entries: ReadDir,
    area: Area,
    _pin: Option<Arc<ObjectGuard>>,
}

impl Frame {
    fn new(directory: Arc<Directory>, area: Area, pin: Option<Arc<ObjectGuard>>) -> Result<Self> {
        directory.verify()?;
        let entries = std::fs::read_dir(directory.path())?;
        directory.verify()?;
        Ok(Self {
            directory,
            entries,
            area,
            _pin: pin,
        })
    }
}

pub(super) struct Inventory {
    expires: Instant,
    sequence: u64,
    frames: Vec<Frame>,
    _activity: ActivityGuard,
}

pub(super) type Inventories = Mutex<HashMap<Id, Inventory>>;

impl Namespace {
    pub fn inventory_page(&self, cursor: Option<InventoryCursor>) -> Result<InventoryPage> {
        let activity = ActivityGuard::acquire(&self.directory)?;
        self.check_inspection_admission()?;
        let policy = self.policy()?.policy;
        let mut catalogs = self
            .cursors
            .lock()
            .map_err(|_| Error::corrupt("catalog cursor registry poisoned"))?;
        catalogs.retain(|_, cursor| cursor.expires > Instant::now());
        let mut inventories = self
            .inventories
            .lock()
            .map_err(|_| Error::corrupt("inventory registry poisoned"))?;
        inventories.retain(|_, inventory| inventory.expires > Instant::now());
        let mut cursor = match cursor {
            Some(cursor) => {
                if cursor.namespace != self.header().namespace
                    || cursor.instance != *self.instance()
                {
                    return Err(Error::new(
                        ErrorCategory::StaleIdentity,
                        "inventory-identity",
                        "inventory belongs to another namespace or instance",
                    ));
                }
                if inventories
                    .get(&cursor.ticket)
                    .is_none_or(|inventory| inventory.sequence != cursor.sequence)
                {
                    return Err(Error::new(
                        ErrorCategory::StaleVersion,
                        "inventory-cursor-expired",
                        "inventory cursor expired or advanced; restart read-only enumeration",
                    ));
                }
                cursor
            }
            None => {
                if inventories.len() + catalogs.len() >= policy.work.max_cursors as usize {
                    return Err(Error::pressure("namespace-cursor-limit"));
                }
                let cursor = InventoryCursor {
                    namespace: self.header().namespace.clone(),
                    instance: self.instance().clone(),
                    ticket: Id::new()?,
                    sequence: 0,
                };
                inventories.insert(
                    cursor.ticket.clone(),
                    Inventory {
                        expires: Instant::now()
                            .checked_add(Duration::from_millis(policy.work.cursor_lifetime_ms))
                            .ok_or_else(|| Error::invalid("inventory lifetime overflow"))?,
                        sequence: 0,
                        frames: vec![Frame::new(Arc::clone(&self.directory), Area::Root, None)?],
                        _activity: activity,
                    },
                );
                cursor
            }
        };
        drop(catalogs);
        let inventory = inventories
            .get_mut(&cursor.ticket)
            .ok_or_else(|| Error::corrupt("inventory disappeared"))?;
        let started = Instant::now();
        let deadline = started + Duration::from_millis(policy.collection.max_duration_ms.min(100));
        let mut page = InventoryPage {
            inventory_schema: 1,
            consistency: "live-enumeration-not-a-catalog-snapshot",
            examined: 0,
            unknown_entries: 0,
            known_logical_bytes: 0,
            entries: Vec::new(),
            elapsed_nanos: 0,
            elapsed_budget_exceeded: false,
            next: None,
        };
        while page.examined < policy.work.page_objects.min(PAGE_LIMIT) && Instant::now() < deadline
        {
            let Some(frame) = inventory.frames.last_mut() else {
                break;
            };
            frame.directory.verify()?;
            let Some(entry) = frame.entries.next() else {
                inventory.frames.pop();
                continue;
            };
            page.examined += 1;
            let entry = entry?;
            let path = NativePath::from_path(&entry.path())?;
            let result = self.inventory_entry(frame, &entry);
            match result {
                Ok((finding, child)) => {
                    if finding.classification == "unknown" {
                        page.unknown_entries += 1;
                    }
                    if finding.classification == "known"
                        && let Measurement::Observed { value } = finding.logical_bytes
                    {
                        page.known_logical_bytes = page
                            .known_logical_bytes
                            .checked_add(value)
                            .ok_or_else(|| Error::corrupt("inventory byte total overflow"))?;
                    }
                    page.entries.push(finding);
                    if let Some(child) = child {
                        inventory.frames.push(child);
                    }
                }
                Err(error) => {
                    page.unknown_entries += 1;
                    page.entries.push(InventoryEntry {
                        path,
                        classification: "unavailable-or-modified".into(),
                        logical_bytes: Measurement::Unavailable {
                            reason: "not-safely-observed".into(),
                        },
                        allocated_bytes: Measurement::Unavailable {
                            reason: "not-safely-observed".into(),
                        },
                        content_authenticated: Measurement::Unavailable {
                            reason: "inspection-does-not-authenticate-payload".into(),
                        },
                        error: Some(serde_json::to_value(error)?),
                    });
                }
            }
        }
        inventory.sequence = inventory
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::corrupt("inventory sequence overflow"))?;
        cursor.sequence = inventory.sequence;
        if inventory.frames.is_empty() {
            inventories.remove(&cursor.ticket);
        } else {
            page.next = Some(cursor);
        }
        page.elapsed_nanos = u64::try_from(started.elapsed().as_nanos())
            .map_err(|_| Error::corrupt("inventory duration overflow"))?;
        page.elapsed_budget_exceeded = Instant::now() > deadline;
        Ok(page)
    }

    fn inventory_entry(
        &self,
        frame: &Frame,
        entry: &std::fs::DirEntry,
    ) -> Result<(InventoryEntry, Option<Frame>)> {
        let path = NativePath::from_path(&entry.path())?;
        let native_name = entry.file_name();
        let mut finding = InventoryEntry {
            path,
            classification: "unknown".into(),
            logical_bytes: Measurement::Unavailable {
                reason: "unowned-entry-not-opened".into(),
            },
            allocated_bytes: Measurement::Unavailable {
                reason: "unowned-entry-not-opened".into(),
            },
            error: None,
            content_authenticated: Measurement::Unavailable {
                reason: "inspection-does-not-authenticate-payload".into(),
            },
        };
        let Some(name) = native_name.to_str() else {
            return Ok((finding, None));
        };
        let expected: Option<FileRecord>;
        match &frame.area {
            Area::Root => {
                let area = match name {
                    "objects" => Some((Area::Objects, Arc::clone(&self.objects))),
                    "guards" => Some((Area::Controls("guards"), Arc::clone(&self.guards))),
                    "owners" => Some((Area::Controls("owners"), Arc::clone(&self.owners))),
                    _ => None,
                };
                if let Some((area, directory)) = area {
                    finding.classification = "known-container".into();
                    let child = Frame::new(directory, area, None)?;
                    return Ok((finding, Some(child)));
                }
                if ![
                    "namespace.json",
                    "owner.lock",
                    "activity.lock",
                    "catalog.sqlite",
                    "catalog.sqlite-wal",
                    "catalog.sqlite-shm",
                    "catalog.sqlite-journal",
                ]
                .contains(&name)
                {
                    return Ok((finding, None));
                }
                if super::storage::sqlite_file(name) {
                    let file = frame.directory.observe_file(name)?;
                    if name == "catalog.sqlite" && file.identity != self.header().catalog_identity {
                        return Err(Error::new(
                            ErrorCategory::StaleIdentity,
                            "inventory-anchor-replaced",
                            "namespace anchor was replaced",
                        ));
                    }
                    finding.classification = if name == "catalog.sqlite" {
                        "known"
                    } else {
                        "sqlite-runtime-file-not-a-deletion-target"
                    }
                    .into();
                    finding.logical_bytes = Measurement::Observed {
                        value: file.logical_bytes,
                    };
                    finding.allocated_bytes = file.allocated_bytes;
                    return Ok((finding, None));
                }
                expected = None;
            }
            Area::Objects => {
                let Ok(id) = Id::parse(name) else {
                    return Ok((finding, None));
                };
                let object = match self.object(&id) {
                    Ok(object) if object.state != super::ObjectState::Removed => object,
                    Ok(_) => return Ok((finding, None)),
                    Err(error)
                        if matches!(
                            error.category,
                            ErrorCategory::CacheMissing | ErrorCategory::CacheEvicted
                        ) =>
                    {
                        return Ok((finding, None));
                    }
                    Err(error) => return Err(error),
                };
                let pin = ObjectGuard::acquire(
                    &self.header().namespace,
                    &self.directory,
                    &self.guards,
                    &self.objects,
                    &id,
                    object
                        .guard_identity
                        .as_ref()
                        .ok_or_else(|| Error::corrupt("unsealed inventory object"))?,
                )?;
                if object.directory_identity.as_ref() != Some(&pin.directory.identity()?) {
                    return Err(Error::new(
                        ErrorCategory::StaleIdentity,
                        "inventory-container-replaced",
                        "object container was replaced",
                    ));
                }
                finding.classification = "known-container".into();
                return Ok((
                    finding,
                    Some(Frame::new(
                        Arc::clone(&pin.directory),
                        Area::Object(id),
                        Some(pin),
                    )?),
                ));
            }
            Area::Controls(area) => {
                expected = self.read(|connection| {
                    let encoded: Option<String> = connection.query_row(
                        "SELECT json_extract(record,'$.file') FROM control_files WHERE area=?1 AND name=?2",
                        rusqlite::params![area, name], |row| row.get(0),
                    ).optional()?;
                    encoded.map(|encoded| Ok(serde_json::from_str(&encoded)?)).transpose()
                })?;
                if expected.as_ref().is_none_or(|record| record.removed) {
                    return Ok((finding, None));
                }
            }
            Area::Object(id) => {
                expected = self.read(|connection| {
                    let encoded: Option<String> = connection
                        .query_row(
                            "SELECT record FROM members WHERE object_id=?1 AND name=?2",
                            rusqlite::params![id.as_str(), name],
                            |row| row.get(0),
                        )
                        .optional()?;
                    encoded
                        .map(|encoded| Ok(serde_json::from_str(&encoded)?))
                        .transpose()
                })?;
                if expected.as_ref().is_none_or(|record| record.removed) {
                    return Ok((finding, None));
                }
            }
        }
        let cached_file = if let Some(expected) = &expected
            && matches!(&frame.area, Area::Object(_))
        {
            let cache = self.verification.try_lock().map_err(|error| match error {
                TryLockError::WouldBlock => Error::busy("member-verifier-active"),
                TryLockError::Poisoned(_) => Error::corrupt("member verification cache poisoned"),
            })?;
            if let Some(context) = cache.as_ref()
                && super::FileIdentity::of(&context.native.file)? == expected.identity
            {
                context.check(&frame.directory)?;
                // A duplicate shares the existing lease; reopening would break
                // Linux verification even though inventory is read-only.
                Some(context.native.file.try_clone()?)
            } else {
                None
            }
        } else {
            None
        };
        let file = match cached_file {
            Some(file) => file,
            None => frame.directory.open_file(name, false)?,
        };
        if let Some(expected) = expected {
            let _memory = self.verification_scratch(super::storage::SECURITY_BYTES as u64)?;
            let length = file.metadata()?.len();
            if super::FileIdentity::of(&file)? != expected.identity
                || super::Ownership::capture(&file)? != expected.ownership
                || (!expected.producer_open
                    && length != expected.logical_bytes
                    && !(expected.pending_length.filter(|length| *length != 0) == Some(length)
                        && expected
                            .seal
                            .as_ref()
                            .is_some_and(|seal| expected.pending_manifest == Some(seal.manifest))))
            {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "inventory-file-modified",
                    "owned file differs from its catalog evidence",
                ));
            }
        }
        finding.classification = if matches!(frame.area, Area::Root) {
            let identity = super::FileIdentity::of(&file)?;
            let expected = match name {
                "owner.lock" => Some(&self.header().owner_identity),
                "activity.lock" => Some(&self.header().activity_identity),
                _ => None,
            };
            if expected.is_some_and(|expected| identity != *expected) {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "inventory-anchor-replaced",
                    "namespace anchor was replaced",
                ));
            }
            if name == "namespace.json" {
                let header: NamespaceHeader = frame.directory.read_json(name, 64 * 1024)?;
                if serde_json::to_value(header)? != serde_json::to_value(self.header())? {
                    return Err(Error::corrupt(
                        "namespace header differs from its opened identity",
                    ));
                }
            }
            "known"
        } else {
            "known"
        }
        .into();
        finding.logical_bytes = Measurement::Observed {
            value: file.metadata()?.len(),
        };
        finding.allocated_bytes = allocated_bytes(&file)?;
        Ok((finding, None))
    }

    pub fn close_inventory(&self, cursor: &InventoryCursor) -> Result<bool> {
        if cursor.namespace != self.header().namespace || cursor.instance != *self.instance() {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "inventory-identity",
                "inventory belongs to another namespace or instance",
            ));
        }
        Ok(self
            .inventories
            .lock()
            .map_err(|_| Error::corrupt("inventory registry poisoned"))?
            .remove(&cursor.ticket)
            .is_some())
    }

    pub(super) fn expire_inventories(&self) -> Result<()> {
        self.inventories
            .lock()
            .map_err(|_| Error::corrupt("inventory registry poisoned"))?
            .retain(|_, inventory| inventory.expires > Instant::now());
        Ok(())
    }
}

#[derive(Serialize)]
pub struct DiscoveredNamespace {
    pub path: NativePath,
    pub header: Option<NamespaceHeader>,
    pub ownership: String,
    pub error: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct DiscoveryPage {
    pub discovery_schema: u32,
    pub entries: Vec<DiscoveredNamespace>,
    pub elapsed_nanos: u64,
    pub elapsed_budget_exceeded: bool,
    pub complete: bool,
}

/// A bounded live enumeration of a cache parent's namespace directory. It
/// neither activates a namespace nor acquires deletion authorization.
pub struct NamespaceDiscovery {
    directory: Arc<Directory>,
    entries: ReadDir,
}

impl NamespaceDiscovery {
    pub fn open(cache_parent: &std::path::Path) -> Result<Self> {
        let directory = Directory::open(cache_parent)?.child(super::STORE_DIRECTORY)?;
        let entries = std::fs::read_dir(directory.path())?;
        Ok(Self { directory, entries })
    }

    pub fn next_page(&mut self, limit: u32) -> Result<DiscoveryPage> {
        if !(1..=PAGE_LIMIT).contains(&limit) {
            return Err(Error::invalid("discovery page limit must be in 1..=256"));
        }
        self.directory.verify()?;
        let started = Instant::now();
        let mut page = DiscoveryPage {
            discovery_schema: 1,
            entries: Vec::new(),
            elapsed_nanos: 0,
            elapsed_budget_exceeded: false,
            complete: false,
        };
        while page.entries.len() < limit as usize && started.elapsed() < Duration::from_millis(100)
        {
            let Some(entry) = self.entries.next() else {
                page.complete = true;
                break;
            };
            let entry = entry?;
            let path = NativePath::from_path(&entry.path())?;
            let observed = (|| {
                let name = entry.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| Error::invalid("unknown native namespace name"))?;
                if name.len() != 64
                    || !name
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(Error::invalid("unknown entry beneath managed cache parent"));
                }
                let directory = self.directory.child(name)?;
                let header: NamespaceHeader = directory.read_json("namespace.json", 64 * 1024)?;
                if header.schema != super::STORAGE_VERSION
                    || header.authentication != super::authentication::FORMAT
                    || header.repository != name
                    || header.directory_identity != directory.identity()?
                {
                    return Err(Error::incompatible("namespace identity or schema differs"));
                }
                let lock = directory.open_file("owner.lock", true)?;
                if super::FileIdentity::of(&lock)? != header.owner_identity {
                    return Err(Error::corrupt("namespace ownership anchor differs"));
                }
                let ownership = match fs2::FileExt::try_lock_exclusive(&lock) {
                    Ok(()) => "available",
                    Err(error) => {
                        let error = super::lifetime::lock_error(error, "namespace-owned");
                        if error.category == ErrorCategory::Busy {
                            "owned"
                        } else {
                            return Err(error);
                        }
                    }
                };
                Ok((header, ownership.to_string()))
            })();
            page.entries.push(match observed {
                Ok((header, ownership)) => DiscoveredNamespace {
                    path,
                    header: Some(header),
                    ownership,
                    error: None,
                },
                Err(error) => DiscoveredNamespace {
                    path,
                    header: None,
                    ownership: "unknown".into(),
                    error: Some(serde_json::to_value(error)?),
                },
            });
        }
        self.directory.verify()?;
        page.elapsed_nanos = u64::try_from(started.elapsed().as_nanos())
            .map_err(|_| Error::corrupt("discovery duration overflow"))?;
        page.elapsed_budget_exceeded = started.elapsed() > Duration::from_millis(100);
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_is_paged_read_only_and_does_not_enter_unknown_directories() {
        let temp = tempfile::tempdir().unwrap();
        let mut policy = super::super::policy::fixture_policy();
        policy.work.page_objects = 1;
        let namespace =
            Namespace::initialize_identity(&"a".repeat(64), temp.path(), policy).unwrap();
        let unknown = namespace.path().join("foreign");
        std::fs::create_dir(&unknown).unwrap();
        std::fs::write(unknown.join("untouched"), b"private data").unwrap();
        let mut cursor = None;
        let mut entries = Vec::new();
        for _ in 0..128 {
            let page = namespace.inventory_page(cursor).unwrap();
            assert!(page.examined <= 1);
            entries.extend(page.entries);
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert!(cursor.is_none());
        let unknown = serde_json::to_value(NativePath::from_path(&unknown).unwrap()).unwrap();
        let findings: Vec<_> = entries
            .iter()
            .filter(|entry| entry.classification == "unknown")
            .collect();
        assert_eq!(findings.len(), 1);
        assert_eq!(serde_json::to_value(&findings[0].path).unwrap(), unknown);
        assert_eq!(
            std::fs::read(namespace.path().join("foreign").join("untouched")).unwrap(),
            b"private data"
        );
        assert!(namespace.page(None).unwrap().objects.is_empty());
    }

    #[test]
    fn inventory_shares_cursor_admission_and_blocks_atomic_idle_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let mut policy = super::super::policy::fixture_policy();
        policy.work.page_objects = 1;
        policy.work.max_cursors = 1;
        let namespace =
            Namespace::initialize_identity(&"a".repeat(64), temp.path(), policy).unwrap();
        namespace.activate().unwrap();
        let cursor = namespace.inventory_page(None).unwrap().next.unwrap();
        assert_eq!(
            namespace.page(None).unwrap_err().category,
            ErrorCategory::ResourcePressure
        );
        let idle = namespace.stop_if_idle(Default::default()).unwrap();
        assert!(!idle.stopping);
        assert!(idle.namespace_readers_or_work);
        assert!(namespace.close_inventory(&cursor).unwrap());
        assert!(matches!(
            namespace.inventory_page(Some(cursor)),
            Err(Error {
                category: ErrorCategory::StaleVersion,
                ..
            })
        ));
        assert!(namespace.stop_if_idle(Default::default()).unwrap().stopping);
        assert!(matches!(
            namespace.inventory_page(None),
            Err(Error {
                category: ErrorCategory::Busy,
                ..
            })
        ));
    }

    #[test]
    fn discovery_reports_independent_owners_and_preserves_foreign_entries() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = Namespace::initialize_identity(
            &"a".repeat(64),
            temp.path(),
            super::super::policy::fixture_policy(),
        )
        .unwrap();
        let other = Namespace::initialize_identity(
            &"b".repeat(64),
            temp.path(),
            super::super::policy::fixture_policy(),
        )
        .unwrap();
        let other_path = other.path().to_path_buf();
        drop(other);
        let foreign = temp
            .path()
            .join(super::super::STORE_DIRECTORY)
            .join("not-a-namespace");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join("untouched"), b"not managed").unwrap();
        let mut discovery = NamespaceDiscovery::open(temp.path()).unwrap();
        let mut entries = Vec::new();
        for _ in 0..16 {
            let page = discovery.next_page(1).unwrap();
            assert!(page.entries.len() <= 1);
            entries.extend(page.entries);
            if page.complete {
                break;
            }
        }
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.ownership == "unknown")
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry
                    .header
                    .as_ref()
                    .is_some_and(|header| header.namespace == namespace.header().namespace))
                .unwrap()
                .ownership,
            "owned"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.ownership == "available")
                .count(),
            1
        );
        assert!(
            Namespace::open(&other_path).is_ok(),
            "discovery must release its ownership probes"
        );
        assert_eq!(
            std::fs::read(foreign.join("untouched")).unwrap(),
            b"not managed"
        );
    }
}
