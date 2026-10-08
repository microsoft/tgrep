// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::{connect, sql_integer, text};
use super::lifetime::{ActivityGuard, ObjectGuard, lock_error};
use super::storage::Directory;
use super::{Error, ErrorCategory, FileIdentity, Id, Namespace, NativePath, Result};
use crate::rooted::RootedDir;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootAnchor {
    pub key: String,
    pub root: NativePath,
    pub identity: FileIdentity,
    pub guard: Id,
    pub guard_identity: FileIdentity,
    pub version: u64,
    pub retired: bool,
}

pub(crate) struct RootProtection {
    pub(crate) namespace: Id,
    pub(crate) anchor: RootAnchor,
    _file: File,
    _activity: ActivityGuard,
}

fn anchor_row(connection: &rusqlite::Connection, key: &str) -> Result<Option<RootAnchor>> {
    let record: Option<String> = connection
        .query_row(
            "SELECT record FROM records WHERE kind='root' AND id=?1",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    record
        .map(|record| Ok(serde_json::from_str(&record)?))
        .transpose()
}

fn open_guard(directory: &Directory, anchor: &RootAnchor) -> Result<File> {
    let file = directory
        .child("guards")?
        .open_file(&format!("root-{}.lock", anchor.guard), true)?;
    if FileIdentity::of(&file)? != anchor.guard_identity {
        return Err(Error::corrupt("root lifetime guard was replaced"));
    }
    Ok(file)
}

impl RootProtection {
    pub(crate) fn acquire(
        base: &ObjectGuard,
        root: &RootedDir,
        path: &std::path::Path,
    ) -> Result<Arc<Self>> {
        Self::in_namespace(base.namespace_directory(), root, path)
    }

    pub(crate) fn in_namespace(
        directory: &Arc<Directory>,
        root: &RootedDir,
        path: &std::path::Path,
    ) -> Result<Arc<Self>> {
        let activity = ActivityGuard::acquire(directory)?;
        let header: super::NamespaceHeader = directory.read_json("namespace.json", 64 * 1024)?;
        if header.directory_identity != directory.identity()? {
            return Err(Error::corrupt("root namespace identity differs"));
        }
        if directory.observe_file("catalog.sqlite")?.identity != header.catalog_identity {
            return Err(Error::corrupt("root catalog identity differs"));
        }
        let identity = FileIdentity::of(&root.directory_handle()?)?;
        let native = NativePath::from_path(path)?;
        let key =
            blake3::hash(text(&serde_json::json!({"root":native,"identity":identity}))?.as_bytes())
                .to_hex()
                .to_string();
        let mut connection = connect(directory, false)?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::work::ensure_admission(&transaction)?;
        let previous: Option<String> = transaction
            .query_row(
                "SELECT record FROM records WHERE kind='root-current' AND id=?1",
                [&key],
                |row| row.get(0),
            )
            .optional()?;
        let previous = previous
            .map(|record| -> Result<_> {
                let guard: Id = serde_json::from_str(&record)?;
                anchor_row(&transaction, guard.as_str())?
                    .ok_or_else(|| Error::corrupt("current root guard has no incarnation record"))
            })
            .transpose()?;
        let (anchor, file) = match previous {
            Some(anchor) if !anchor.retired => {
                if anchor.root != native || anchor.identity != identity {
                    return Err(Error::corrupt("root lifetime binding differs"));
                }
                let file = open_guard(directory, &anchor)?;
                (anchor, file)
            }
            previous => {
                let policy: String = transaction.query_row(
                    "SELECT policy FROM state WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )?;
                let policy: super::Policy = serde_json::from_str(&policy)?;
                let count: u64 = transaction.query_row(
                    "SELECT count(*) FROM records WHERE kind='root' AND json_extract(record,'$.retired')=0",
                    [], |row| super::catalog::unsigned(row, 0),
                )?;
                if count >= u64::from(policy.work.max_leases) {
                    return Err(Error::pressure("root-anchor-limit"));
                }
                let guard = Id::new()?;
                let file = directory
                    .child("guards")?
                    .create_file(&format!("root-{guard}.lock"))?;
                file.sync_all()?;
                super::housekeeping::register_control(
                    &transaction,
                    directory.child("guards")?.as_ref(),
                    "guards",
                    &format!("root-{guard}.lock"),
                    guard.as_str(),
                )?;
                let version = previous
                    .map(|previous| {
                        previous
                            .version
                            .checked_add(1)
                            .filter(|value| *value <= i64::MAX as u64)
                            .ok_or_else(|| Error::corrupt("root version exhausted"))
                    })
                    .transpose()?
                    .unwrap_or(1);
                let anchor = RootAnchor {
                    key: key.clone(),
                    root: native,
                    identity,
                    guard,
                    guard_identity: FileIdentity::of(&file)?,
                    version,
                    retired: false,
                };
                transaction.execute(
                    "INSERT INTO records VALUES('root',?1,?2,?3) ON CONFLICT(kind,id) DO UPDATE SET version=excluded.version,record=excluded.record",
                    params![anchor.guard.as_str(), sql_integer(version)?, text(&anchor)?],
                )?;
                transaction.execute(
                    "INSERT INTO records VALUES('root-current',?1,1,?2) ON CONFLICT(kind,id) DO UPDATE SET version=version+1,record=excluded.record",
                    params![key, text(&anchor.guard)?],
                )?;
                (anchor, file)
            }
        };
        fs2::FileExt::try_lock_shared(&file)
            .map_err(|error| lock_error(error, "root-retirement-in-progress"))?;
        transaction.commit()?;
        root.verify_root()?;
        Ok(Arc::new(Self {
            namespace: header.namespace,
            anchor,
            _file: file,
            _activity: activity,
        }))
    }
}

impl Namespace {
    pub(crate) fn root_identity_busy(
        &self,
        root: &NativePath,
        identity: &FileIdentity,
    ) -> Result<bool> {
        let key =
            blake3::hash(text(&serde_json::json!({"root":root,"identity":identity}))?.as_bytes())
                .to_hex()
                .to_string();
        let anchor = self.read(|connection| {
            let guard: Option<String> = connection
                .query_row(
                    "SELECT record FROM records WHERE kind='root-current' AND id=?1",
                    [&key],
                    |row| row.get(0),
                )
                .optional()?;
            guard
                .map(|guard| -> Result<_> {
                    let id: Id = serde_json::from_str(&guard)?;
                    anchor_row(connection, id.as_str())?
                        .ok_or_else(|| Error::corrupt("root incarnation missing"))
                })
                .transpose()
        })?;
        anchor
            .as_ref()
            .map(|anchor| self.root_busy(anchor))
            .transpose()
            .map(|busy| busy.unwrap_or(false))
    }

    pub fn root_busy(&self, expected: &RootAnchor) -> Result<bool> {
        let current = self
            .read(|connection| anchor_row(connection, expected.guard.as_str()))?
            .ok_or_else(|| {
                Error::new(
                    ErrorCategory::StaleIdentity,
                    "root-anchor-missing",
                    "root guard record is unavailable",
                )
            })?;
        if current.guard != expected.guard || current.guard_identity != expected.guard_identity {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "root-anchor-replaced",
                "root has a newer lifetime incarnation",
            ));
        }
        if current.retired {
            return Ok(false);
        }
        let file = open_guard(&self.directory, &current)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(false),
            Err(error) => {
                let error = lock_error(error, "root-readers-active");
                if error.category == ErrorCategory::Busy {
                    Ok(true)
                } else {
                    Err(error)
                }
            }
        }
    }

    pub(super) fn retire_root(
        &self,
        expected: &RootAnchor,
    ) -> Result<(bool, super::housekeeping::ControlRemoval)> {
        let previous = self
            .read(|connection| anchor_row(connection, expected.guard.as_str()))?
            .ok_or_else(|| Error::corrupt("root anchor missing"))?;
        if previous.guard_identity != expected.guard_identity
            || previous.version != expected.version
        {
            return Err(Error::stale_version(previous.version));
        }
        let file = match open_guard(&self.directory, expected) {
            Ok(file) => file,
            Err(error)
                if previous.retired
                    && error.source_io_kind() == Some(std::io::ErrorKind::NotFound) =>
            {
                return self
                    .remove_owned_control("guards", &format!("root-{}.lock", expected.guard))
                    .map(|removed| (false, removed));
            }
            Err(error) => return Err(error),
        };
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|error| lock_error(error, "root-readers-active"))?;
        if !previous.retired {
            self.transaction(|transaction| {
                let mut current = anchor_row(transaction, expected.guard.as_str())?
                    .ok_or_else(|| Error::corrupt("root anchor missing"))?;
                if current.guard != expected.guard || current.version != expected.version {
                    return Err(Error::stale_version(current.version));
                }
                current.retired = true;
                transaction.execute(
                    "UPDATE records SET record=?2 WHERE kind='root' AND id=?1",
                    params![current.guard.as_str(), text(&current)?],
                )?;
                Ok(())
            })?;
        }
        drop(file);
        let removed = self
            .remove_owned_control("guards", &format!("root-{}.lock", expected.guard))
            .map_err(|error| error.committed(super::CommitState::Committed))?;
        Ok((!previous.retired, removed))
    }
}
