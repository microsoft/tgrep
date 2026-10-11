// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::{VersionedPolicy, sql_integer, unsigned};
use super::storage::Directory;
use super::{Error, ErrorCategory, Measurement, Policy, Result};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde::Serialize;
use std::time::Duration;

const WRITER_WAIT: Duration = Duration::from_millis(250);
const CLEAN_CACHE_BYTES: u64 = 256 * 1024;
const WAL_HEADER_BYTES: u64 = 32;
const FRAME_HEADER_BYTES: u64 = 24;
// SQLite's sqlite3SectorSize clamps every VFS to MAX_SECTOR_SIZE.
const MAX_SECTOR_BYTES: u64 = 65536;
const SHM_REGION_BYTES: u64 = 32768;
const SHM_REGION_FRAMES: u64 = 4096;
const NAMESPACE_HEADER_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Serialize)]
pub(super) struct Checkpoint {
    pub busy: bool,
    pub frames: i64,
    pub checkpointed_frames: i64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct WriteBudget {
    pub metadata_limit_bytes: u64,
    pub sqlite_limit_bytes: u64,
    pub control_limit_bytes: u64,
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub journal_bytes: u64,
    pub page_size: u64,
    pub page_count: u64,
    pub max_pages: u64,
    pub wal_append_bound_bytes: u64,
    pub full_sync_padding_bound_bytes: u64,
    pub sqlite_publication_bound_bytes: u64,
    pub required_publication_bound_bytes: Measurement<u64>,
    pub pager_payload_limit_bytes: u64,
    pub clean_cache_target_bytes: u64,
    pub cache_used_before_commit_bytes: Measurement<u64>,
    pub memory_scope: &'static str,
    pub checkpoint: Option<Checkpoint>,
}

pub(super) fn configure(connection: &Connection, read_only: bool) -> Result<()> {
    connection.busy_timeout(WRITER_WAIT)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "cache_size", -(CLEAN_CACHE_BYTES as i64 / 1024))?;
    connection.pragma_update(None, "cache_spill", false)?;
    connection.pragma_update(None, "mmap_size", 0)?;
    connection.pragma_update(None, "threads", 0)?;
    if !read_only {
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "fullfsync", true)?;
        connection.pragma_update(None, "checkpoint_fullfsync", true)?;
        connection.pragma_update(None, "wal_autocheckpoint", 0)?;
    }
    Ok(())
}

fn file_bytes(directory: &Directory, name: &str) -> Result<u64> {
    match directory.observe_file(name) {
        Ok(file) => Ok(file.logical_bytes),
        Err(error) if error.source_io_kind() == Some(std::io::ErrorKind::NotFound) => Ok(0),
        Err(error) => Err(error),
    }
}

fn policy(connection: &Connection) -> Result<VersionedPolicy> {
    let (version, encoded): (u64, String) = connection.query_row(
        "SELECT policy_version,policy FROM state WHERE singleton=1",
        [],
        |row| Ok((unsigned(row, 0)?, row.get(1)?)),
    )?;
    let policy: Policy = serde_json::from_str(&encoded)?;
    policy.validate()?;
    Ok(VersionedPolicy { version, policy })
}

pub(super) fn admit_control(connection: &Connection, metadata: u64, additional: u64) -> Result<()> {
    let registered: u64 = connection.query_row(
        "SELECT value FROM namespace_totals WHERE name='control_logical'",
        [],
        |row| unsigned(row, 0),
    )?;
    if registered
        .checked_add(additional)
        .and_then(|bytes| bytes.checked_add(NAMESPACE_HEADER_BYTES))
        .is_none_or(|bytes| bytes > metadata / 6)
    {
        return Err(Error::pressure("catalog-control-allocation"));
    }
    Ok(())
}

fn checkpoint(
    connection: &Connection,
    directory: &Directory,
    limit: u64,
) -> Result<Option<Checkpoint>> {
    if file_bytes(directory, "catalog.sqlite-wal")? < limit / 4 {
        return Ok(None);
    }
    connection.busy_timeout(Duration::ZERO)?;
    let result = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        Ok(Checkpoint {
            busy: row.get::<_, u32>(0)? != 0,
            frames: row.get(1)?,
            checkpointed_frames: row.get(2)?,
        })
    });
    let restored = connection.busy_timeout(WRITER_WAIT);
    if let Err(error) = restored {
        return Err(Error::new(
            ErrorCategory::Io,
            "catalog-timeout-restore",
            format!("writer timeout restoration failed: {error}; checkpoint: {result:?}"),
        ));
    }
    Ok(Some(result?))
}

pub(super) fn cache_used(connection: &Connection) -> Measurement<u64> {
    let mut current = 0;
    let mut unused_highwater = 0;
    // SAFETY: the connection remains exclusively borrowed for this native observation.
    let status = unsafe {
        rusqlite::ffi::sqlite3_db_status(
            connection.handle(),
            rusqlite::ffi::SQLITE_DBSTATUS_CACHE_USED,
            &mut current,
            &mut unused_highwater,
            0,
        )
    };
    if status == rusqlite::ffi::SQLITE_OK && current >= 0 {
        Measurement::Observed {
            value: current as u64,
        }
    } else {
        Measurement::Unavailable {
            reason: format!("sqlite-cache-counter-unavailable: status={status}, value={current}"),
        }
    }
}

impl WriteBudget {
    fn for_pages(&self, pages: u64) -> (u64, u128) {
        let frame = self.page_size + FRAME_HEADER_BYTES;
        let append = WAL_HEADER_BYTES + pages * frame + self.full_sync_padding_bound_bytes;
        let wal = u128::from(self.wal_bytes) + u128::from(append);
        // The first region has 4062 slots, subsequent regions have 4096.
        let shm = u128::from(SHM_REGION_BYTES)
            * (1 + wal
                .div_ceil(u128::from(frame))
                .div_ceil(u128::from(SHM_REGION_FRAMES)));
        (
            append,
            u128::from(self.database_bytes.max(pages * self.page_size))
                + wal
                + u128::from(self.shm_bytes).max(shm)
                + u128::from(self.journal_bytes),
        )
    }
}

fn admit(
    transaction: &Transaction<'_>,
    directory: &Directory,
    policy: &Policy,
    checkpoint: Option<Checkpoint>,
) -> Result<WriteBudget> {
    let journal: String = transaction.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if journal != "wal" {
        return Err(Error::incompatible("catalog publication requires WAL mode"));
    }
    let page_size = transaction.pragma_query_value(None, "page_size", |row| unsigned(row, 0))?;
    if !(512_u64..=65536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(Error::corrupt("unsupported SQLite page size"));
    }
    let page_count = transaction.pragma_query_value(None, "page_count", |row| unsigned(row, 0))?;
    let metadata = policy.work.metadata_bytes;
    let mut budget = WriteBudget {
        metadata_limit_bytes: metadata,
        // Control files have their own sixth. Their observed bytes still participate
        // in new-work admission; externally enlarged anchors cannot forbid cleanup.
        sqlite_limit_bytes: metadata - metadata / 6,
        control_limit_bytes: metadata / 6,
        database_bytes: file_bytes(directory, "catalog.sqlite")?,
        wal_bytes: file_bytes(directory, "catalog.sqlite-wal")?,
        shm_bytes: file_bytes(directory, "catalog.sqlite-shm")?,
        journal_bytes: file_bytes(directory, "catalog.sqlite-journal")?,
        page_size,
        page_count,
        max_pages: 0,
        wal_append_bound_bytes: 0,
        full_sync_padding_bound_bytes: (MAX_SECTOR_BYTES - 1)
            .div_ceil(page_size + FRAME_HEADER_BYTES)
            * (page_size + FRAME_HEADER_BYTES),
        sqlite_publication_bound_bytes: 0,
        required_publication_bound_bytes: Measurement::Unavailable {
            reason: "publication-bound-not-calculated".into(),
        },
        pager_payload_limit_bytes: 0,
        clean_cache_target_bytes: CLEAN_CACHE_BYTES,
        cache_used_before_commit_bytes: Measurement::Unavailable {
            reason: "before-commit-observation-not-reached".into(),
        },
        memory_scope: "one managed catalog connection; page payload capped by max_pages, \
            clean cache target is soft; native cache observation includes pager headers, \
            excludes other connections and non-pager SQLite heap; not an RSS limit",
        checkpoint,
    };
    let mut low = 0;
    let mut high = (metadata / 3 / page_size).min(u64::from(u32::MAX - 1));
    while low < high {
        let pages = low + (high - low).div_ceil(2);
        if budget.for_pages(pages).1 <= u128::from(budget.sqlite_limit_bytes) {
            low = pages;
        } else {
            high = pages - 1;
        }
    }
    budget.max_pages = low;
    budget.pager_payload_limit_bytes = low * page_size;
    let (append, publication) = budget.for_pages(low);
    budget.wal_append_bound_bytes = append;
    budget.sqlite_publication_bound_bytes = u64::try_from(publication)
        .map_err(|_| Error::corrupt("catalog publication bound overflow"))?;
    budget.required_publication_bound_bytes =
        match u64::try_from(budget.for_pages(page_count.max(1)).1) {
            Ok(value) => Measurement::Observed { value },
            Err(_) => Measurement::Unavailable {
                reason: "required-publication-exceeds-u64".into(),
            },
        };
    if low < page_count.max(1) {
        let reader_blocked = budget.checkpoint.as_ref().is_some_and(|value| value.busy);
        return Err(Error::new(
            if reader_blocked {
                ErrorCategory::Busy
            } else {
                ErrorCategory::ResourcePressure
            },
            if reader_blocked {
                "catalog-checkpoint-readers-active"
            } else {
                "catalog-metadata-allocation"
            },
            format!(
                "insufficient serialized catalog publication headroom: {}",
                serde_json::to_string(&budget)?
            ),
        ));
    }
    transaction.pragma_update(None, "max_page_count", sql_integer(low)?)?;
    let actual = transaction.pragma_query_value(None, "max_page_count", |row| unsigned(row, 0))?;
    if actual != low {
        return Err(Error::incompatible(
            "SQLite did not enforce the catalog page ceiling",
        ));
    }
    Ok(budget)
}

pub(super) fn begin<'a>(
    connection: &'a mut Connection,
    directory: &Directory,
    change: Option<&VersionedPolicy>,
) -> Result<(Transaction<'a>, WriteBudget)> {
    let previous = policy(connection)?;
    let threshold = change.map_or(previous.policy.work.metadata_bytes, |change| {
        previous
            .policy
            .work
            .metadata_bytes
            .min(change.policy.work.metadata_bytes)
    });
    let checkpoint = checkpoint(connection, directory, threshold)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Policy, physical lengths and page ceiling are read after acquiring the actual
    // SQLite writer lock. Independent protected readers cannot spend stale headroom.
    let previous = policy(&transaction)?;
    let selected = if let Some(change) = change {
        if previous.version != change.version {
            return Err(Error::stale_version(previous.version));
        }
        change.policy.validate()?;
        admit_control(&transaction, change.policy.work.metadata_bytes, 0)?;
        &change.policy
    } else {
        &previous.policy
    };
    let budget = admit(&transaction, directory, selected, checkpoint)?;
    Ok((transaction, budget))
}

pub(super) fn begin_initial<'a>(
    connection: &'a mut Connection,
    directory: &Directory,
    policy: &Policy,
) -> Result<Transaction<'a>> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    admit(&transaction, directory, policy, None)?;
    Ok(transaction)
}

#[cfg(test)]
mod tests {
    use super::super::catalog::connect;
    use super::*;

    #[test]
    fn full_sync_without_powersafe_overwrite_pads_beyond_the_database_page_count() {
        let temp = tempfile::Builder::new()
            .prefix("catalog psow %# ")
            .tempdir()
            .unwrap();
        let directory = Directory::open(temp.path()).unwrap();
        directory.create_file("catalog.sqlite").unwrap();
        let path = directory.path().join("catalog.sqlite");
        let encoded: String = path
            .to_str()
            .unwrap()
            .bytes()
            .map(|byte| format!("%{byte:02X}"))
            .collect();
        // Unix caches device characteristics before a later PSOW file-control
        // change. Disable it when opening, before the VFS computes that cache.
        let mut connection = Connection::open_with_flags(
            format!("file:{encoded}?psow=0"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        configure(&connection, false).unwrap();
        connection
            .execute_batch(
                "PRAGMA page_size=4096; PRAGMA journal_mode=WAL; CREATE TABLE probe(value)",
            )
            .unwrap();
        let busy: u32 = connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy, 0);
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let budget = admit(
            &transaction,
            &directory,
            &super::super::policy::fixture_policy(),
            None,
        )
        .unwrap();
        transaction
            .pragma_update(None, "max_page_count", 2)
            .unwrap();
        transaction.pragma_update(None, "user_version", 1).unwrap();
        transaction
            .execute("INSERT INTO probe VALUES(1)", [])
            .unwrap();
        transaction.commit().unwrap();
        let pages = connection
            .pragma_query_value(None, "page_count", |row| unsigned(row, 0))
            .unwrap();
        let wal = file_bytes(&directory, "catalog.sqlite-wal").unwrap();
        let frame = budget.page_size + FRAME_HEADER_BYTES;
        assert_eq!(pages, 2);
        assert!(
            wal > WAL_HEADER_BYTES + pages * frame,
            "FULL-sync padding was not exercised"
        );
        assert!(wal <= budget.for_pages(pages).0);
        assert!(budget.full_sync_padding_bound_bytes >= frame);
    }

    #[test]
    fn all_managed_writers_disable_auto_checkpoints_spill_and_unaccounted_mmap() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = super::super::Namespace::initialize_identity(
            &"a".repeat(64),
            temp.path(),
            super::super::policy::fixture_policy(),
        )
        .unwrap();
        for read_only in [false, true] {
            let connection = connect(&namespace.directory, read_only).unwrap();
            for setting in ["cache_spill", "mmap_size", "threads"] {
                assert_eq!(
                    connection
                        .pragma_query_value(None, setting, |row| row.get::<_, i64>(0))
                        .unwrap(),
                    0,
                    "{setting}",
                );
            }
            if !read_only {
                assert_eq!(
                    connection
                        .pragma_query_value(None, "synchronous", |row| row.get::<_, u32>(0))
                        .unwrap(),
                    2
                );
                assert_eq!(
                    connection
                        .pragma_query_value(None, "wal_autocheckpoint", |row| row.get::<_, u32>(0))
                        .unwrap(),
                    0
                );
            }
        }
    }

    #[test]
    fn publication_bound_includes_database_growth_and_prospective_shm_regions() {
        let temp = tempfile::tempdir().unwrap();
        let directory = Directory::open(temp.path()).unwrap();
        directory.create_file("catalog.sqlite").unwrap();
        let mut connection = connect(&directory, false).unwrap();
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let mut budget = admit(
            &transaction,
            &directory,
            &super::super::policy::fixture_policy(),
            None,
        )
        .unwrap();
        budget.database_bytes = 4096;
        budget.wal_bytes = 32;
        budget.shm_bytes = SHM_REGION_BYTES;
        budget.journal_bytes = 8192;
        let (append, total) = budget.for_pages(4096);
        assert!(append > 4096 * (4096 + FRAME_HEADER_BYTES) + WAL_HEADER_BYTES);
        assert!(
            total
                >= u128::from(
                    4096 * 4096
                        + budget.wal_bytes
                        + append
                        + 2 * SHM_REGION_BYTES
                        + budget.journal_bytes,
                )
        );
        budget.database_bytes = u64::MAX;
        budget.wal_bytes = u64::MAX;
        budget.shm_bytes = u64::MAX;
        budget.journal_bytes = u64::MAX;
        assert!(budget.for_pages(u64::from(u32::MAX - 1)).1 > u128::from(u64::MAX));
    }
}
