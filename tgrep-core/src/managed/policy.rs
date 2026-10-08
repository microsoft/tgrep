// Copyright (c) Microsoft Corporation. All rights reserved.

use super::{Error, Result, STORAGE_VERSION};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageMode {
    Managed,
    CompatibilityRetainAll,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Retention {
    RetainAll,
    Bounded { target_bytes: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Advancement {
    Fixed,
    Adaptive {
        high_bytes: u64,
        low_bytes: u64,
        min_reduction_bytes: u64,
        min_reduction_percent: u8,
        cooldown_ms: u64,
        max_paths: u32,
        max_read_bytes: u64,
        max_attempts: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Schedule {
    Disabled,
    Periodic { interval_ms: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkLimits {
    pub max_views: u32,
    pub max_leases: u32,
    pub workers: u32,
    pub queue_items: u32,
    pub staging_bytes: u64,
    pub private_work_bytes: u64,
    pub sort_buffer_bytes: u64,
    pub blob_bytes: u64,
    pub operation_timeout_ms: u64,
    pub page_objects: u32,
    pub max_cursors: u32,
    pub cursor_lifetime_ms: u64,
    pub max_receipts: u32,
    pub metadata_bytes: u64,
}

impl WorkLimits {
    pub(crate) fn minimum_private_bytes(&self) -> Result<u64> {
        self.sort_buffer_bytes
            .checked_mul(4)
            .and_then(|bytes| {
                self.blob_bytes
                    .checked_mul(6)
                    .and_then(|input| bytes.checked_add(input))
            })
            .and_then(|bytes| bytes.checked_add(256 * 1024))
            .ok_or_else(|| Error::invalid("private work reservation overflow"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionPolicy {
    pub schedule: Schedule,
    pub on_pressure: bool,
    pub checkpoint_grace_ms: u64,
    pub generation_grace_ms: u64,
    pub max_duration_ms: u64,
    pub max_examined: u32,
    pub max_removed: u32,
    pub max_delete_bytes: u64,
    pub chunk_bytes: u64,
    pub max_pages: u32,
    pub retry_ms: u64,
}

/// All new limits are explicit. Absence is not interpreted as unlimited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema: u32,
    pub storage: StorageMode,
    pub retention: Retention,
    pub advancement: Advancement,
    pub work: WorkLimits,
    pub collection: CollectionPolicy,
}

fn integer(name: &str, value: u64, minimum: u64, maximum: u64) -> Result<()> {
    if !(minimum..=maximum).contains(&value) {
        return Err(Error::invalid(format!(
            "{name} must be in {minimum}..={maximum}"
        )));
    }
    Ok(())
}

fn positive(name: &str, value: u64) -> Result<()> {
    integer(name, value, 1, i64::MAX as u64)
}

fn duration(name: &str, value: u64, allow_zero: bool) -> Result<()> {
    integer(name, value, u64::from(!allow_zero), i64::MAX as u64)?;
    if Instant::now()
        .checked_add(Duration::from_millis(value))
        .is_none()
    {
        return Err(Error::invalid(format!(
            "{name} overflows the monotonic clock"
        )));
    }
    Ok(())
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        if self.schema != STORAGE_VERSION {
            return Err(Error::incompatible("unsupported managed policy schema"));
        }
        if let Retention::Bounded { target_bytes } = self.retention {
            positive("retention.target_bytes", target_bytes)?;
            if self.storage != StorageMode::Managed {
                return Err(Error::incompatible(
                    "legacy generation stores cannot enable bounded retention",
                ));
            }
        }
        let work = &self.work;
        integer("max_views", u64::from(work.max_views), 1, 1024)?;
        integer("max_leases", u64::from(work.max_leases), 1, 65536)?;
        integer("workers", u64::from(work.workers), 1, 64)?;
        integer("queue_items", u64::from(work.queue_items), 1, 65536)?;
        integer("page_objects", u64::from(work.page_objects), 1, 1024)?;
        integer("max_cursors", u64::from(work.max_cursors), 1, 1024)?;
        integer("max_receipts", u64::from(work.max_receipts), 1, 1_000_000)?;
        for (name, bytes) in [
            ("staging_bytes", work.staging_bytes),
            ("private_work_bytes", work.private_work_bytes),
            ("sort_buffer_bytes", work.sort_buffer_bytes),
            ("blob_bytes", work.blob_bytes),
            ("metadata_bytes", work.metadata_bytes),
        ] {
            positive(name, bytes)?;
        }
        integer(
            "metadata_bytes",
            work.metadata_bytes,
            1024 * 1024,
            i64::MAX as u64,
        )?;
        integer(
            "sort_buffer_bytes",
            work.sort_buffer_bytes,
            16 * 1024,
            usize::MAX as u64,
        )?;
        let per_worker = work
            .minimum_private_bytes()?
            .checked_mul(u64::from(work.workers))
            .ok_or_else(|| Error::invalid("private work reservation overflow"))?;
        if per_worker > work.private_work_bytes {
            return Err(Error::invalid(
                "worker sort and input buffers exceed private_work_bytes; extraction also consumes the reservation",
            ));
        }
        duration("operation_timeout_ms", work.operation_timeout_ms, false)?;
        duration("cursor_lifetime_ms", work.cursor_lifetime_ms, false)?;
        let collection = &self.collection;
        duration("checkpoint_grace_ms", collection.checkpoint_grace_ms, true)?;
        duration("generation_grace_ms", collection.generation_grace_ms, true)?;
        duration("max_duration_ms", collection.max_duration_ms, false)?;
        duration("retry_ms", collection.retry_ms, false)?;
        if let Schedule::Periodic { interval_ms } = collection.schedule {
            duration("interval_ms", interval_ms, false)?;
        }
        for (name, count) in [
            ("max_examined", collection.max_examined),
            ("max_removed", collection.max_removed),
            ("max_pages", collection.max_pages),
        ] {
            integer(name, u64::from(count), 1, 1_000_000)?;
        }
        positive("max_delete_bytes", collection.max_delete_bytes)?;
        integer("chunk_bytes", collection.chunk_bytes, 4096, i64::MAX as u64)?;
        if collection.chunk_bytes > collection.max_delete_bytes {
            return Err(Error::invalid("chunk_bytes exceeds max_delete_bytes"));
        }
        if let Advancement::Adaptive {
            high_bytes,
            low_bytes,
            min_reduction_bytes,
            min_reduction_percent,
            cooldown_ms,
            max_paths,
            max_read_bytes,
            max_attempts,
        } = self.advancement
        {
            positive("high_bytes", high_bytes)?;
            integer("low_bytes", low_bytes, 0, high_bytes - 1)?;
            positive("min_reduction_bytes", min_reduction_bytes)?;
            integer(
                "min_reduction_percent",
                u64::from(min_reduction_percent),
                1,
                100,
            )?;
            duration("cooldown_ms", cooldown_ms, false)?;
            integer("adaptive.max_paths", u64::from(max_paths), 1, 1_000_000)?;
            positive("adaptive.max_read_bytes", max_read_bytes)?;
            integer("adaptive.max_attempts", u64::from(max_attempts), 1, 64)?;
        }
        Ok(())
    }

    pub fn storage_target(&self) -> Option<u64> {
        match self.retention {
            Retention::RetainAll => None,
            Retention::Bounded { target_bytes } => Some(target_bytes),
        }
    }
}

#[cfg(test)]
pub(crate) fn fixture_policy() -> Policy {
    Policy {
        schema: STORAGE_VERSION,
        storage: StorageMode::Managed,
        retention: Retention::RetainAll,
        advancement: Advancement::Fixed,
        work: WorkLimits {
            max_views: 8,
            max_leases: 32,
            workers: 2,
            queue_items: 16,
            staging_bytes: 64 * 1024 * 1024,
            private_work_bytes: 64 * 1024 * 1024,
            sort_buffer_bytes: 1024 * 1024,
            blob_bytes: 1024 * 1024,
            operation_timeout_ms: 30_000,
            page_objects: 16,
            max_cursors: 8,
            cursor_lifetime_ms: 30_000,
            max_receipts: 1024,
            metadata_bytes: 16 * 1024 * 1024,
        },
        collection: CollectionPolicy {
            schedule: Schedule::Disabled,
            on_pressure: false,
            checkpoint_grace_ms: 0,
            generation_grace_ms: 0,
            max_duration_ms: 1000,
            max_examined: 64,
            max_removed: 16,
            max_delete_bytes: 1024 * 1024,
            chunk_bytes: 64 * 1024,
            max_pages: 4,
            retry_ms: 10,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_limits_and_modes_fail_closed() {
        let policy = fixture_policy();
        policy.validate().unwrap();
        let mut value = serde_json::to_value(&policy).unwrap();
        value["work"]
            .as_object_mut()
            .unwrap()
            .remove("staging_bytes");
        assert!(serde_json::from_value::<Policy>(value).is_err());
        let mut policy = policy;
        policy.work.workers = 0;
        assert!(policy.validate().is_err());
        policy.work.workers = 2;
        policy.storage = StorageMode::CompatibilityRetainAll;
        policy.retention = Retention::Bounded { target_bytes: 1 };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn quotas_include_parallel_work_and_chunk_bounds() {
        let mut policy = fixture_policy();
        policy.work.private_work_bytes = 1;
        assert!(policy.validate().is_err());
        policy = fixture_policy();
        policy.collection.max_delete_bytes = policy.collection.chunk_bytes - 1;
        assert!(policy.validate().is_err());
        policy = fixture_policy();
        policy.work.sort_buffer_bytes = 16 * 1024 - 1;
        assert!(policy.validate().is_err());
        policy.work.sort_buffer_bytes = 16 * 1024;
        assert!(policy.validate().is_ok());
    }
}
