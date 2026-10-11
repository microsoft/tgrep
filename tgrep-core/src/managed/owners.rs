// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::text;
use super::lifetime::lock_error;
use super::{
    Error, ErrorCategory, FileIdentity, Id, Namespace, OwnerClaim, OwnerProof, OwnerRecord, Result,
};
use rusqlite::params;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct OwnerRelease {
    pub owner: OwnerRecord,
    pub leases_released: usize,
    pub operations_cancelled: usize,
    pub reservations_waiting: u64,
}

impl Namespace {
    /// Explicit release is consent, not a claim that the process or its readers died.
    pub fn release_owner(&self, claim: &OwnerClaim) -> Result<OwnerRelease> {
        let record = self.owner(&claim.owner)?;
        if claim.namespace != self.header().namespace || text(&record.claim)? != text(claim)? {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "stale-owner-release",
                "owner claim is not current",
            ));
        }
        self.commit_owner_release(record)
    }

    /// Positive proof holds the original guard exclusively through retirement.
    pub fn reap_owner(&self, id: &Id) -> Result<OwnerRelease> {
        let mut record = self.owner(id)?;
        if record.released {
            return self.commit_owner_release(record);
        }
        if !record.registered {
            return Err(Error::busy("unregistered-owner-has-no-proven-lifetime"));
        }
        let file = self.open_control(&self.owners, &format!("{id}.lock"), true)?;
        if FileIdentity::of(&file)? != record.claim.guard_identity {
            return Err(Error::busy("owner-guard-identity-unknown"));
        }
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|error| lock_error(error, "owner-still-alive"))?;
        record.last_proof = OwnerProof::Ended;
        self.commit_owner_release(record)
    }

    fn commit_owner_release(&self, mut record: OwnerRecord) -> Result<OwnerRelease> {
        self.transaction(|transaction| {
            let id = &record.claim.owner;
            record.released = true;
            transaction.execute("UPDATE owners SET record=?2 WHERE id=?1", params![id.as_str(), text(&record)?])?;
            transaction.execute("UPDATE scopes SET closed=1 WHERE id=?1", [id.as_str()])?;
            let leases_released = transaction.execute(
                "UPDATE records SET version=version+1,record=json_set(record,'$.released',json('true'))
                 WHERE kind='lease' AND json_extract(record,'$.owner')=?1 AND json_extract(record,'$.released')=0",
                [id.as_str()],
            )?;
            // Reservations are released by stopped producers, not by the lease reaper.
            let operations_cancelled = transaction.execute(
                "UPDATE operations SET state='cancelling',record=json_set(record,'$.cancelled',json('true'),'$.state','cancelling')
                 WHERE (scope=?1 OR json_extract(record,'$.request.owner')=?1)
                 AND state IN ('accepted','preparing','cancelling')",
                [id.as_str()],
            )?;
            transaction.execute(
                "DELETE FROM refs WHERE owner=?1 AND source_kind IN ('lease','attachment')", [id.as_str()],
            )?;
            let waiting = transaction.query_row(
                "SELECT count(*) FROM reservations WHERE owner=?1", [id.as_str()],
                |row| super::catalog::unsigned(row, 0),
            )?;
            Ok(OwnerRelease { owner: record, leases_released, operations_cancelled, reservations_waiting: waiting })
        })
    }
}
