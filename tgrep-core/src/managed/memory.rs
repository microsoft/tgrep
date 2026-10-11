// Copyright (c) Microsoft Corporation. All rights reserved.

use super::{Error, Id, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryUsage {
    pub measurement_scope: String,
    pub resident_bytes: super::Measurement<u64>,
    pub retained_private_estimate_bytes: u64,
    pub unreserved_retained_private_estimate_bytes: u64,
    pub peak_retained_private_estimate_bytes: u64,
    pub mapped_bytes: u64,
    pub peak_mapped_bytes: u64,
    pub retained_allocations: u64,
}

impl Default for MemoryUsage {
    fn default() -> Self {
        Self {
            measurement_scope: "this-process-and-namespace; external-process readers excluded"
                .into(),
            resident_bytes: super::Measurement::Unavailable {
                reason: "resident-memory-is-not-a-namespace-quantity".into(),
            },
            retained_private_estimate_bytes: 0,
            unreserved_retained_private_estimate_bytes: 0,
            peak_retained_private_estimate_bytes: 0,
            mapped_bytes: 0,
            peak_mapped_bytes: 0,
            retained_allocations: 0,
        }
    }
}

#[derive(Default)]
pub(crate) struct MemoryState {
    usage: MemoryUsage,
    covered: HashMap<Id, u64>,
}

impl MemoryState {
    pub(crate) fn unreserved_capacity(
        &mut self,
        connection: &Connection,
        limit: u64,
    ) -> Result<u64> {
        let usage = self.usage(connection)?;
        let reserved: u64 = connection.query_row(
            "SELECT coalesce(sum(json_extract(record,'$.request.private_bytes')),0) FROM reservations",
            [], |row| super::catalog::unsigned(row, 0),
        )?;
        let used = reserved
            .checked_add(usage.unreserved_retained_private_estimate_bytes)
            .ok_or_else(|| Error::pressure("retained-private-allocation"))?;
        limit
            .checked_sub(used)
            .ok_or_else(|| Error::pressure("retained-private-allocation"))
    }

    pub(crate) fn usage(&mut self, connection: &Connection) -> Result<MemoryUsage> {
        let mut statement = connection.prepare("SELECT id FROM reservations LIMIT 65")?;
        let active = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if active.len() > 64 {
            return Err(Error::corrupt(
                "reservation count exceeds the supported work-slot bound",
            ));
        }
        self.covered
            .retain(|id, _| active.iter().any(|active| active == id.as_str()));
        self.snapshot()
    }

    fn snapshot(&self) -> Result<MemoryUsage> {
        let covered = self.covered.values().try_fold(0_u64, |total, bytes| {
            total
                .checked_add(*bytes)
                .ok_or_else(|| Error::corrupt("retained memory accounting overflow"))
        })?;
        let mut result = self.usage.clone();
        result.unreserved_retained_private_estimate_bytes = result
            .retained_private_estimate_bytes
            .checked_sub(covered)
            .ok_or_else(|| Error::corrupt("retained memory coverage exceeds usage"))?;
        Ok(result)
    }

    fn add(&mut self, private: u64, mapped: u64, reservation: Option<&Id>) -> Result<()> {
        let private_total = self
            .usage
            .retained_private_estimate_bytes
            .checked_add(private)
            .ok_or_else(|| Error::pressure("retained-private-account-overflow"))?;
        let mapped_total = self
            .usage
            .mapped_bytes
            .checked_add(mapped)
            .ok_or_else(|| Error::pressure("mapped-account-overflow"))?;
        let allocations = self
            .usage
            .retained_allocations
            .checked_add(1)
            .ok_or_else(|| Error::pressure("retained-allocation-count-overflow"))?;
        if let Some(id) = reservation {
            let total = self
                .covered
                .get(id)
                .copied()
                .unwrap_or(0)
                .checked_add(private)
                .ok_or_else(|| Error::pressure("retained-reservation-account-overflow"))?;
            self.covered.insert(id.clone(), total);
        }
        self.usage.retained_private_estimate_bytes = private_total;
        self.usage.mapped_bytes = mapped_total;
        self.usage.retained_allocations = allocations;
        self.usage.peak_retained_private_estimate_bytes = self
            .usage
            .peak_retained_private_estimate_bytes
            .max(private_total);
        self.usage.peak_mapped_bytes = self.usage.peak_mapped_bytes.max(mapped_total);
        Ok(())
    }

    pub(crate) fn finish_reservation(&mut self, id: &Id) {
        self.covered.remove(id);
    }
}

#[derive(Default)]
pub(crate) struct MemoryAccount {
    state: Mutex<MemoryState>,
}

impl MemoryAccount {
    pub(crate) fn for_namespace(namespace: &Id) -> Result<Arc<Self>> {
        static ACCOUNTS: OnceLock<Mutex<HashMap<Id, Weak<MemoryAccount>>>> = OnceLock::new();
        let mut accounts = ACCOUNTS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| Error::corrupt("memory account registry poisoned"))?;
        if let Some(account) = accounts.get(namespace).and_then(Weak::upgrade) {
            return Ok(account);
        }
        accounts.retain(|_, account| account.strong_count() != 0);
        let account = Arc::new(Self::default());
        accounts.insert(namespace.clone(), Arc::downgrade(&account));
        Ok(account)
    }

    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, MemoryState>> {
        self.state
            .lock()
            .map_err(|_| Error::corrupt("memory account poisoned"))
    }

    pub(crate) fn retain_reserved(
        self: &Arc<Self>,
        private: u64,
        mapped: u64,
        permit: &Arc<super::WorkPermit>,
    ) -> Result<RetainedMemory> {
        let mut state = self.lock()?;
        permit.check()?;
        state.add(private, mapped, Some(&permit.record.id))?;
        permit.retain_charge(private);
        Ok(RetainedMemory {
            account: Arc::clone(self),
            private,
            mapped,
            reservation: Some(permit.record.id.clone()),
            permit: Arc::downgrade(permit),
        })
    }

    pub(crate) fn retain_unreserved(
        self: &Arc<Self>,
        private: u64,
        mapped: u64,
        capacity: impl FnOnce(&mut MemoryState) -> Result<u64>,
    ) -> Result<RetainedMemory> {
        // Reservations and retained allocations always take memory before the
        // catalog. Finish the fallible catalog read before creating a charge.
        let mut state = self.lock()?;
        if private > capacity(&mut state)? {
            return Err(Error::pressure("retained-private-allocation"));
        }
        state.add(private, mapped, None)?;
        Ok(RetainedMemory {
            account: Arc::clone(self),
            private,
            mapped,
            reservation: None,
            permit: Weak::new(),
        })
    }
}

/// Accounting follows the data, not the daemon or a build's admission slot.
/// It is process-local capacity accounting, not an OS resident-memory limit.
pub(crate) struct RetainedMemory {
    account: Arc<MemoryAccount>,
    private: u64,
    mapped: u64,
    reservation: Option<Id>,
    permit: Weak<super::WorkPermit>,
}

impl Drop for RetainedMemory {
    fn drop(&mut self) {
        let mut state = match self.account.lock() {
            Ok(state) => state,
            Err(error) => {
                eprintln!(
                    "managed memory accounting remains conservative after release failure: {error}"
                );
                return;
            }
        };
        state.usage.retained_private_estimate_bytes -= self.private;
        state.usage.mapped_bytes -= self.mapped;
        state.usage.retained_allocations -= 1;
        if let Some(id) = &self.reservation
            && let Some(covered) = state.covered.get_mut(id)
        {
            *covered -= self.private;
            if *covered == 0 {
                state.covered.remove(id);
            }
        }
        if let Some(permit) = self.permit.upgrade() {
            permit.release_retained_charge(self.private);
        }
    }
}
