// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::text;
use super::clock::Stamp;
use super::policy::Advancement;
use super::{Error, Id, MigrationRequest, OperationRecord, OperationToken, Result, ViewManager};
use crate::generations::git;
use crate::meta::ContentId;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdaptiveRequest {
    pub view: Id,
    pub owner: Id,
    pub expected_version: u64,
    pub allocation_version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdaptiveDecision {
    pub eligible: bool,
    pub reason_code: String,
    pub source_version: u64,
    pub target_commit: Option<String>,
    pub overlay_bytes: u64,
    pub expected_reduction_bytes: Option<u64>,
    pub paths_examined: u32,
    pub target_blob_bytes_read: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdaptiveState {
    stamp: Stamp,
    signature: Option<String>,
    armed: bool,
    attempts: u32,
    decision: Option<AdaptiveDecision>,
    source_version: u64,
    input_epoch: u64,
    policy_version: u64,
    allocation_version: u64,
    owner: Id,
    last_error: Option<serde_json::Value>,
}

pub(crate) fn sufficient(before: u64, after: u64, absolute: u64, percent: u8) -> bool {
    before.checked_sub(after).is_some_and(|reduction| {
        reduction >= absolute
            && u128::from(reduction) * 100 >= u128::from(before) * u128::from(percent)
    })
}

fn save_state(transaction: &Transaction<'_>, view: &Id, state: &AdaptiveState) -> Result<()> {
    transaction.execute(
        "INSERT INTO records VALUES('adaptive',?1,1,?2) ON CONFLICT(kind,id)
         DO UPDATE SET version=version+1,record=excluded.record",
        params![view.as_str(), text(state)?],
    )?;
    Ok(())
}

fn save_decision(
    transaction: &Transaction<'_>,
    operation: &Id,
    decision: &AdaptiveDecision,
    input_epoch: u64,
) -> Result<()> {
    let record: String = transaction.query_row(
        "SELECT record FROM operations WHERE id=?1",
        [operation.as_str()],
        |row| row.get(0),
    )?;
    let mut current: OperationRecord = serde_json::from_str(&record)?;
    current.progress["adaptive"] = serde_json::to_value(decision)?;
    current.progress["adaptive_input_epoch"] = json!(input_epoch);
    super::Namespace::save_operation(transaction, &current)
}

impl ViewManager {
    pub fn automatic_advancement_due(&self, view: &Id, owner: &Id) -> Result<bool> {
        let policy = self.namespace().policy()?;
        let Advancement::Adaptive {
            cooldown_ms,
            max_attempts,
            ..
        } = policy.policy.advancement
        else {
            return Ok(false);
        };
        let previous = self.adaptive_state(view)?;
        let Some(previous) = previous else {
            return Ok(true);
        };
        let now = self.namespace().clock.now()?;
        let elapsed = (previous.stamp.boot == now.boot
            && previous.stamp.wall_millis <= now.wall_millis)
            .then(|| now.millis.checked_sub(previous.stamp.millis))
            .flatten();
        if elapsed.is_none() {
            return Ok(true);
        }
        if elapsed.is_some_and(|elapsed| elapsed < cooldown_ms) {
            return Ok(false);
        }
        let slot = self.slot(view)?;
        if previous.input_epoch != slot.input_epoch()
            || previous.policy_version != policy.version
            || previous.allocation_version != self.namespace().allocation()?.version
            || previous.source_version != slot.current()?.record.version
            || previous.owner != *owner
            || previous
                .decision
                .as_ref()
                .is_some_and(|decision| decision.reason_code == "adaptive-cooldown")
        {
            return Ok(true);
        }
        if previous
            .last_error
            .as_ref()
            .is_some_and(|error| error["reason_code"] == "fixed-pin-participant")
        {
            return self.namespace().read(|connection| Ok(!connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM records WHERE kind='lease' AND json_extract(record,'$.view')=?1
                 AND json_extract(record,'$.released')=0 AND json_extract(record,'$.original.migratable')=0)",
                [view.as_str()], |row| row.get::<_, bool>(0),
            )?));
        }
        let retryable = previous.last_error.as_ref().map_or_else(
            || {
                previous
                    .decision
                    .as_ref()
                    .is_some_and(|decision| decision.eligible)
            },
            |error| error["retryable"] == true,
        );
        Ok(previous.attempts < max_attempts && retryable)
    }

    fn adaptive_state(&self, view: &Id) -> Result<Option<AdaptiveState>> {
        self.namespace().read(|connection| {
            let record: Option<String> = connection
                .query_row(
                    "SELECT record FROM records WHERE kind='adaptive' AND id=?1",
                    [view.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            record
                .map(|record| Ok(serde_json::from_str(&record)?))
                .transpose()
        })
    }

    pub fn accept_automatic_advancement(
        &self,
        token: super::Token,
        request: AdaptiveRequest,
    ) -> Result<OperationRecord> {
        self.namespace().validate_active_owner(&request.owner)?;
        self.namespace()
            .accept_system_operation(token, "adaptive", serde_json::to_value(request)?)
    }

    pub fn accept_adaptive(
        &self,
        token: OperationToken,
        request: AdaptiveRequest,
    ) -> Result<OperationRecord> {
        if token.scope != request.owner {
            return Err(Error::invalid("adaptive token scope must be its owner"));
        }
        self.namespace().validate_operation_owner(&token)?;
        self.namespace()
            .accept_operation(token, "adaptive", serde_json::to_value(request)?)
    }

    pub fn last_adaptive(&self, view: &Id) -> Result<Option<AdaptiveDecision>> {
        Ok(self.adaptive_state(view)?.and_then(|state| state.decision))
    }

    pub fn last_adaptive_error(&self, view: &Id) -> Result<Option<serde_json::Value>> {
        Ok(self
            .adaptive_state(view)?
            .and_then(|state| state.last_error))
    }

    pub(super) fn execute_adaptive(
        &self,
        operation: &OperationRecord,
        request: AdaptiveRequest,
    ) -> Result<()> {
        let mut input_epoch = self.slot(&request.view)?.current()?.record.input_epoch;
        let saved = operation
            .progress
            .get("adaptive")
            .map(|value| serde_json::from_value::<AdaptiveDecision>(value.clone()))
            .transpose()?;
        let decision = match saved {
            Some(decision) if decision.eligible => {
                input_epoch = operation.progress["adaptive_input_epoch"]
                    .as_u64()
                    .ok_or_else(|| Error::corrupt("adaptive intent has no captured input epoch"))?;
                decision
            }
            _ => match self.evaluate_adaptive(operation, &request) {
                Ok(decision) => decision,
                Err(error) => {
                    self.record_adaptive_failure(&request, &error, false, input_epoch)?;
                    return Err(error);
                }
            },
        };
        if !decision.eligible {
            self.namespace().transaction(|transaction| {
                save_decision(transaction, &operation.id, &decision, input_epoch)
            })?;
            self.namespace().record_operation_result(
                &operation.id,
                Ok(json!({"adaptive":decision,"current":self.recover(&request.view)?})),
                false,
            )?;
            return Ok(());
        }
        let slot = self.slot(&request.view)?;
        let result = self.execute_migration(
            &self.namespace().operation(&operation.id)?,
            MigrationRequest {
                view: request.view.clone(),
                root: slot.root().to_path_buf(),
                expected_version: request.expected_version,
                target_commit: decision
                    .target_commit
                    .ok_or_else(|| Error::corrupt("adaptive target missing"))?,
                profile: *slot.current()?.record.pin()?.key.profile(),
                owner: request.owner.clone(),
                allocation_version: request.allocation_version,
            },
        );
        if let Err(error) = &result {
            self.record_adaptive_failure(&request, error, true, input_epoch)?;
        } else {
            let mut state = self
                .adaptive_state(&request.view)?
                .ok_or_else(|| Error::corrupt("successful adaptive evaluation is missing"))?;
            // Publishing the captured HEAD makes its reducible committed
            // divergence zero, even if irreducible private content remains.
            state.armed = true;
            self.save_adaptive(&request.view, &state)
                .map_err(|error| error.committed(super::CommitState::Committed))?;
        }
        result
    }

    fn record_adaptive_failure(
        &self,
        request: &AdaptiveRequest,
        error: &Error,
        attempt_counted: bool,
        input_epoch: u64,
    ) -> Result<()> {
        let slot = self.slot(&request.view)?;
        let current = slot.current()?;
        if current.record.version != request.expected_version {
            return Ok(());
        }
        let previous = self.adaptive_state(&request.view)?;
        let policy_version = self.namespace().policy()?.version;
        let attempts = previous.as_ref().map_or(1, |previous| {
            if previous.input_epoch != input_epoch
                || previous.policy_version != policy_version
                || previous.allocation_version != request.allocation_version
                || previous.source_version != request.expected_version
                || previous.owner != request.owner
            {
                1
            } else {
                previous
                    .attempts
                    .saturating_add(u32::from(!attempt_counted))
                    .max(1)
            }
        });
        let decision = previous
            .as_ref()
            .and_then(|previous| previous.decision.clone());
        self.save_adaptive(
            &request.view,
            &AdaptiveState {
                stamp: self.namespace().clock.now()?,
                signature: previous
                    .as_ref()
                    .and_then(|previous| previous.signature.clone()),
                armed: previous.as_ref().is_none_or(|previous| previous.armed),
                attempts,
                decision,
                source_version: current.record.version,
                // A failed probe must not acknowledge a newly invalidated,
                // unreconciled epoch and suppress evaluation after its refresh.
                input_epoch,
                policy_version,
                allocation_version: request.allocation_version,
                owner: request.owner.clone(),
                last_error: Some(serde_json::to_value(error)?),
            },
        )
    }

    fn evaluate_adaptive(
        &self,
        operation: &OperationRecord,
        request: &AdaptiveRequest,
    ) -> Result<AdaptiveDecision> {
        let namespace = self.namespace();
        let versioned_policy = namespace.policy()?;
        let policy = versioned_policy.policy;
        let Advancement::Adaptive {
            high_bytes,
            low_bytes,
            min_reduction_bytes,
            min_reduction_percent,
            cooldown_ms,
            max_paths,
            max_read_bytes,
            max_attempts,
        } = policy.advancement
        else {
            return Err(Error::incompatible("adaptive advancement is not enabled"));
        };
        self.check_migratable(&request.view, &request.owner, request.expected_version)?;
        let slot = self.slot(&request.view)?;
        let query = slot.query(request.expected_version)?;
        let current = query.published();
        let cost = current.view.overlay_cost()?;
        let mut decision = AdaptiveDecision {
            eligible: false,
            reason_code: "below-high-watermark".into(),
            source_version: request.expected_version,
            target_commit: None,
            overlay_bytes: cost.bytes,
            expected_reduction_bytes: None,
            paths_examined: 0,
            target_blob_bytes_read: 0,
        };
        let previous = self.adaptive_state(&request.view)?;
        let stamp = namespace.clock.now()?;
        let mut state = AdaptiveState {
            stamp: stamp.clone(),
            signature: previous
                .as_ref()
                .and_then(|previous| previous.signature.clone()),
            armed: previous.as_ref().is_none_or(|previous| previous.armed),
            attempts: previous.as_ref().map_or(0, |previous| previous.attempts),
            decision: Some(decision.clone()),
            source_version: request.expected_version,
            input_epoch: current.record.input_epoch,
            policy_version: versioned_policy.version,
            allocation_version: request.allocation_version,
            owner: request.owner.clone(),
            last_error: None,
        };
        if previous.as_ref().is_some_and(|previous| {
            previous.policy_version != state.policy_version
                || previous.allocation_version != state.allocation_version
                || previous.source_version != state.source_version
                || previous.owner != state.owner
                || previous
                    .last_error
                    .as_ref()
                    .is_some_and(|error| error["reason_code"] == "fixed-pin-participant")
        }) {
            state.attempts = 0;
        }
        if cost.bytes <= low_bytes {
            state.armed = true;
        }
        if let Some(previous) = &previous {
            let elapsed = (previous.stamp.boot == stamp.boot
                && previous.stamp.wall_millis <= stamp.wall_millis)
                .then(|| stamp.millis.checked_sub(previous.stamp.millis))
                .flatten();
            if elapsed.is_none_or(|elapsed| elapsed < cooldown_ms) {
                decision.reason_code = "adaptive-cooldown".into();
                if elapsed.is_some() {
                    return Ok(decision);
                }
                state.decision = Some(decision.clone());
                self.save_adaptive(&request.view, &state)?;
                return Ok(decision);
            }
        }
        if cost.bytes < high_bytes {
            state.decision = Some(decision.clone());
            self.save_adaptive(&request.view, &state)?;
            return Ok(decision);
        }
        let permit = self.reserve(
            &operation.id,
            Some(&request.owner),
            request.allocation_version,
        )?;
        let control = super::process::Control::work(&permit);
        let (commit, target) = namespace.generation_key_controlled(
            current.view.repository(),
            "HEAD",
            *current.record.pin()?.key.profile(),
            &control,
        )?;
        decision.target_commit = Some(commit);
        if target.tree_oid() == current.record.pin()?.key.tree_oid() {
            state.armed = true;
            decision.reason_code = "current-generation-key".into();
            decision.expected_reduction_bytes = Some(0);
        } else {
            let _memory = permit.memory(permit.private_limit() / 2)?;
            let evidence = current.view.private_evidence(max_paths, &permit)?;
            let paths: Vec<_> = evidence.iter().map(|entry| entry.path.as_str()).collect();
            let entries = git::entries_for_paths(
                current.view.repository(),
                target.tree_oid(),
                &paths,
                &control,
                usize::try_from(permit.private_limit() / 64)
                    .map_err(|_| Error::pressure("adaptive-metadata-address-range"))?,
                max_paths,
            )?;
            let target_entries: HashMap<_, _> = entries
                .iter()
                .map(|entry| (entry.path.as_str(), entry))
                .collect();
            let tracked: Vec<_> = evidence
                .iter()
                .filter(|entry| {
                    target_entries.contains_key(entry.path.as_str())
                        || current.view.generation().entry(&entry.path).is_some()
                })
                .collect();
            #[derive(Serialize)]
            struct Signature<'a> {
                source: &'a crate::generations::GenerationKey,
                target: &'a crate::generations::GenerationKey,
                tracked: &'a Vec<&'a crate::worktrees::PrivateEvidence>,
                policy: &'a Advancement,
            }
            let signature = blake3::Hash::from(crate::output::checksum(
                &Signature {
                    source: &current.record.pin()?.key,
                    target: &target,
                    tracked: &tracked,
                    policy: &policy.advancement,
                },
                Some(&permit),
            )?)
            .to_hex()
            .to_string();
            let unchanged = state.signature.as_ref() == Some(&signature);
            let retry = unchanged
                && previous.as_ref().is_some_and(|previous| {
                    previous.last_error.as_ref().map_or_else(
                        || {
                            previous
                                .decision
                                .as_ref()
                                .is_some_and(|decision| decision.eligible)
                        },
                        |error| error["retryable"] == true,
                    )
                });
            if unchanged && (!retry || state.attempts >= max_attempts) {
                decision.reason_code = "unchanged-tracked-evidence".into();
            } else {
                if !unchanged {
                    state.attempts = 0;
                }
                state.signature = Some(signature);
                let mut blobs = None;
                let mut reduced = 0_u64;
                for evidence in tracked {
                    permit.check()?;
                    decision.paths_examined += 1;
                    let target_content = match target_entries.get(evidence.path.as_str()) {
                        None => None,
                        Some(entry)
                            if !entry.mode.is_regular()
                                || current
                                    .record
                                    .pin()?
                                    .key
                                    .profile()
                                    .max_blob_bytes
                                    .is_some_and(|limit| {
                                        entry.size.is_some_and(|size| size > limit)
                                    }) =>
                        {
                            None
                        }
                        Some(entry) => {
                            if let Some(previous) = current.view.generation().entry(&entry.path)
                                && previous.oid == entry.oid
                            {
                                if previous.content_id() == evidence.content {
                                    reduced =
                                        reduced.checked_add(evidence.bytes).ok_or_else(|| {
                                            Error::corrupt("adaptive accounting overflow")
                                        })?;
                                }
                                continue;
                            }
                            let size = entry
                                .size
                                .ok_or_else(|| Error::corrupt("regular target blob has no size"))?;
                            if decision
                                .target_blob_bytes_read
                                .checked_add(size)
                                .is_none_or(|total| total > max_read_bytes)
                                || size > policy.work.blob_bytes
                            {
                                return Err(Error::pressure("adaptive-blob-read-limit"));
                            }
                            let _blob = permit.memory(size.checked_mul(4).ok_or_else(|| {
                                Error::pressure("adaptive-blob-memory-overflow")
                            })?)?;
                            let batch = match &mut blobs {
                                Some(batch) => batch,
                                None => blobs.insert(git::Blobs::controlled(
                                    current.view.repository(),
                                    control.clone(),
                                )?),
                            };
                            let bytes = batch.read(&entry.oid, size)?;
                            decision.target_blob_bytes_read += size;
                            let decoded = crate::encoding::decode_for_index_controlled(
                                &bytes,
                                Some(&permit),
                            )?;
                            if crate::trigram::is_binary(&decoded) {
                                None
                            } else {
                                Some(ContentId::from_indexed_bytes_controlled(
                                    &decoded,
                                    Some(&permit),
                                )?)
                            }
                        }
                    };
                    if target_content == evidence.content {
                        reduced = reduced
                            .checked_add(evidence.bytes)
                            .ok_or_else(|| Error::corrupt("adaptive accounting overflow"))?;
                    }
                }
                if let Some(blobs) = blobs {
                    blobs.finish()?;
                }
                decision.expected_reduction_bytes = Some(reduced);
                if reduced <= low_bytes {
                    state.armed = true;
                }
                decision.eligible = (state.armed || retry)
                    && sufficient(
                        cost.bytes,
                        cost.bytes.saturating_sub(reduced),
                        min_reduction_bytes,
                        min_reduction_percent,
                    );
                decision.reason_code = if decision.eligible {
                    "reducible-tracked-divergence"
                } else if !state.armed {
                    "adaptive-hysteresis"
                } else {
                    "insufficient-reducible-cost"
                }
                .into();
                if decision.eligible {
                    state.armed = false;
                    state.attempts += 1;
                }
            }
        }
        query.validate()?;
        state.decision = Some(decision.clone());
        // One eligible evaluation is one logical attempt. Persist its exact
        // target with the attempt count before any bounded worker deferral.
        self.namespace().transaction(|transaction| {
            save_state(transaction, &request.view, &state)?;
            save_decision(transaction, &operation.id, &decision, state.input_epoch)
        })?;
        Ok(decision)
    }

    fn save_adaptive(&self, view: &Id, state: &AdaptiveState) -> Result<()> {
        self.namespace()
            .transaction(|transaction| save_state(transaction, view, state))
    }
}
