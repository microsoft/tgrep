// Copyright (c) Microsoft Corporation. All rights reserved.

use super::{
    CollectionProgress, CommitState, Error, ErrorCategory, Measurement, Namespace,
    RecoveryProgress, Result,
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Collection,
    Recovery,
}

#[derive(Clone, Default, Serialize)]
pub struct PassDiagnostics {
    pub attempts: u64,
    pub in_flight: u64,
    pub completed: u64,
    pub completed_with_errors: u64,
    pub failed: u64,
    pub interrupted: u64,
    pub cancelled: u64,
    pub reported_errors: u64,
    pub consecutive_failures: u64,
    pub observed_duration_nanos: u64,
    pub completed_pass_logical_bytes_reclaimed: u64,
    pub completed_pass_recovered_logical_bytes: u64,
    pub failure_categories: BTreeMap<String, u64>,
    pub last_successful_pass: Option<SuccessfulPass>,
}

#[derive(Clone, Serialize)]
pub struct SuccessfulPass {
    pub completed_wall_millis: Measurement<u64>,
    pub duration_nanos: u64,
}

#[derive(Clone, Default, Serialize)]
pub struct MaintenanceAggregate {
    pub collection: PassDiagnostics,
    pub recovery: PassDiagnostics,
}

#[derive(Default)]
pub(super) struct Observations {
    aggregate: MaintenanceAggregate,
    collection_error: Option<serde_json::Value>,
    recovery_error: Option<serde_json::Value>,
    catalog_error: Option<serde_json::Value>,
    catalog_write: Option<super::catalog_io::WriteBudget>,
}

#[derive(Serialize)]
pub struct MaintenanceDiagnostics {
    pub diagnostics_schema: u32,
    pub observation_scope: &'static str,
    pub prior_instance_totals: Measurement<u64>,
    pub aggregate: MaintenanceAggregate,
    pub details: serde_json::Value,
}

fn add(total: &mut u64, value: u64) -> Result<()> {
    *total = total
        .checked_add(value)
        .ok_or_else(|| Error::corrupt("maintenance observation counter overflow"))?;
    Ok(())
}

impl Observations {
    fn pass(&mut self, kind: Kind) -> &mut PassDiagnostics {
        match kind {
            Kind::Collection => &mut self.aggregate.collection,
            Kind::Recovery => &mut self.aggregate.recovery,
        }
    }
}

pub(super) struct Pass<'a> {
    observations: &'a Mutex<Observations>,
    kind: Kind,
    started: Instant,
    finished: bool,
}

impl Pass<'_> {
    fn finish(
        &mut self,
        error: Option<&Error>,
        cancelled: bool,
        errors: u64,
        reclaimed: u64,
        recovered: u64,
    ) -> Result<()> {
        let duration = u64::try_from(self.started.elapsed().as_nanos())
            .map_err(|_| Error::corrupt("maintenance observation duration overflow"))?;
        let mut observations = self
            .observations
            .lock()
            .map_err(|_| Error::corrupt("maintenance observations poisoned"))?;
        let pass = observations.pass(self.kind);
        pass.in_flight = pass
            .in_flight
            .checked_sub(1)
            .ok_or_else(|| Error::corrupt("maintenance observation has no active pass"))?;
        self.finished = true;
        add(&mut pass.observed_duration_nanos, duration)?;
        add(&mut pass.reported_errors, errors)?;
        if let Some(error) = error {
            add(&mut pass.reported_errors, 1)?;
            add(&mut pass.failed, 1)?;
            add(&mut pass.consecutive_failures, 1)?;
            let category = serde_json::to_value(error.category)?;
            let category = category
                .as_str()
                .ok_or_else(|| Error::corrupt("maintenance error category is not a tag"))?;
            add(
                pass.failure_categories.entry(category.into()).or_default(),
                1,
            )?;
            let detail = Some(serde_json::to_value(error)?);
            match self.kind {
                Kind::Collection => observations.collection_error = detail,
                Kind::Recovery => observations.recovery_error = detail,
            }
        } else {
            add(&mut pass.completed, 1)?;
            add(&mut pass.completed_with_errors, u64::from(errors != 0))?;
            add(&mut pass.cancelled, u64::from(cancelled))?;
            add(&mut pass.completed_pass_logical_bytes_reclaimed, reclaimed)?;
            add(&mut pass.completed_pass_recovered_logical_bytes, recovered)?;
            if errors != 0 {
                add(&mut pass.consecutive_failures, 1)?;
            } else if !cancelled {
                pass.consecutive_failures = 0;
            }
            if !cancelled && errors == 0 {
                let completed_wall_millis = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .and_then(|time| u64::try_from(time.as_millis()).ok())
                    .map_or_else(
                        || Measurement::Unavailable {
                            reason: "wall-clock-range-unavailable".into(),
                        },
                        |value| Measurement::Observed { value },
                    );
                pass.last_successful_pass = Some(SuccessfulPass {
                    completed_wall_millis,
                    duration_nanos: duration,
                });
            }
        }
        Ok(())
    }

    pub(super) fn collection(mut self, result: &Result<CollectionProgress>) -> Result<()> {
        match result {
            Ok(progress) => self.finish(
                None,
                progress.cancelled,
                u64::from(progress.errors),
                progress.logical_bytes_reclaimed,
                progress.recovered_logical_bytes,
            ),
            Err(error) => self.finish(Some(error), false, 0, 0, 0),
        }
        .map_err(|error| error.committed(CommitState::Unknown))
    }

    pub(super) fn recovery(mut self, result: &Result<RecoveryProgress>) -> Result<()> {
        match result {
            Ok(progress) => self.finish(
                None,
                progress.cancelled,
                progress.issues.len() as u64,
                progress.cleanup.control_logical_bytes_reclaimed,
                progress.cleanup.control_recovered_logical_bytes,
            ),
            Err(error) => self.finish(Some(error), false, 0, 0, 0),
        }
        .map_err(|error| error.committed(CommitState::Unknown))
    }
}

impl Drop for Pass<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let error = Error::new(
                ErrorCategory::RecoveryRequired,
                "maintenance-pass-interrupted",
                "no complete in-process observation was produced",
            );
            if let Err(error) = self.finish(Some(&error), false, 0, 0, 0) {
                eprintln!("managed maintenance observations unavailable: {error}");
            } else {
                match self.observations.lock() {
                    Ok(mut observations) => {
                        if let Err(error) = add(&mut observations.pass(self.kind).interrupted, 1) {
                            eprintln!("managed interrupted-pass observation unavailable: {error}");
                        }
                    }
                    Err(error) => {
                        eprintln!("managed interrupted-pass observation unavailable: {error}")
                    }
                }
            }
        }
    }
}

impl Namespace {
    pub(super) fn observe_catalog_write(
        &self,
        budget: &super::catalog_io::WriteBudget,
    ) -> Result<()> {
        self.observations
            .lock()
            .map_err(|_| Error::corrupt("maintenance observations poisoned"))?
            .catalog_write = Some(budget.clone());
        Ok(())
    }

    pub(super) fn observe_catalog_error(&self, error: &Error) -> Result<()> {
        self.observations
            .lock()
            .map_err(|_| Error::corrupt("maintenance observations poisoned"))?
            .catalog_error = Some(serde_json::to_value(error)?);
        Ok(())
    }

    pub(super) fn observe_pass(&self, kind: Kind) -> Result<Pass<'_>> {
        let mut observations = self
            .observations
            .lock()
            .map_err(|_| Error::corrupt("maintenance observations poisoned"))?;
        let pass = observations.pass(kind);
        add(&mut pass.attempts, 1)?;
        add(&mut pass.in_flight, 1)?;
        Ok(Pass {
            observations: &self.observations,
            kind,
            started: Instant::now(),
            finished: false,
        })
    }

    pub fn maintenance_diagnostics(&self) -> Result<MaintenanceDiagnostics> {
        let observations = self
            .observations
            .lock()
            .map_err(|_| Error::corrupt("maintenance observations poisoned"))?;
        Ok(MaintenanceDiagnostics {
            diagnostics_schema: 1,
            observation_scope: "current-namespace-instance; receipt replays excluded",
            prior_instance_totals: Measurement::Unavailable {
                reason: "inspect-durable-operation-receipts-for-earlier-instances".into(),
            },
            aggregate: observations.aggregate.clone(),
            details: serde_json::json!({
                "last_collection_error":observations.collection_error,
                "last_recovery_error":observations.recovery_error,
                "last_catalog_error":observations.catalog_error,
                "last_catalog_write":observations.catalog_write,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed::{CollectionBounds, CollectionRequest, Token};

    #[test]
    fn observations_separate_partial_failures_cancellation_and_success_from_reclaimed_totals() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = Namespace::initialize_identity(
            &"a".repeat(64),
            temp.path(),
            crate::managed::policy::fixture_policy(),
        )
        .unwrap();
        let pass = namespace.observe_pass(Kind::Collection).unwrap();
        assert_eq!(
            namespace
                .maintenance_diagnostics()
                .unwrap()
                .aggregate
                .collection
                .in_flight,
            1
        );
        pass.collection(&Ok(CollectionProgress {
            logical_bytes_reclaimed: 17,
            recovered_logical_bytes: 9,
            ..Default::default()
        }))
        .unwrap();
        let successful = serde_json::to_value(
            namespace
                .maintenance_diagnostics()
                .unwrap()
                .aggregate
                .collection
                .last_successful_pass,
        )
        .unwrap();
        assert!(!successful.is_null());
        namespace
            .observe_pass(Kind::Collection)
            .unwrap()
            .collection(&Err(Error::new(
                ErrorCategory::Io,
                "injected-io",
                "private-path-like-error",
            )
            .committed(CommitState::Committed)))
            .unwrap();
        namespace
            .observe_pass(Kind::Collection)
            .unwrap()
            .collection(&Ok(CollectionProgress {
                errors: 1,
                logical_bytes_reclaimed: 3,
                ..Default::default()
            }))
            .unwrap();
        namespace
            .observe_pass(Kind::Collection)
            .unwrap()
            .collection(&Ok(CollectionProgress {
                cancelled: true,
                ..Default::default()
            }))
            .unwrap();
        let diagnostics = namespace.maintenance_diagnostics().unwrap();
        let collection = &diagnostics.aggregate.collection;
        assert_eq!(
            (
                collection.attempts,
                collection.in_flight,
                collection.completed,
                collection.failed
            ),
            (4, 0, 3, 1)
        );
        assert_eq!(
            (
                collection.completed_with_errors,
                collection.cancelled,
                collection.reported_errors
            ),
            (1, 1, 2)
        );
        assert_eq!(collection.consecutive_failures, 2);
        assert_eq!(collection.completed_pass_logical_bytes_reclaimed, 20);
        assert_eq!(collection.completed_pass_recovered_logical_bytes, 9);
        assert_eq!(
            serde_json::to_value(&collection.last_successful_pass).unwrap(),
            successful
        );
        assert!(
            !serde_json::to_string(&diagnostics.aggregate)
                .unwrap()
                .contains("private-path-like-error")
        );
        assert!(
            diagnostics
                .details
                .to_string()
                .contains("private-path-like-error")
        );
        assert!(matches!(
            diagnostics.prior_instance_totals,
            Measurement::Unavailable { .. }
        ));
        drop(namespace.observe_pass(Kind::Recovery).unwrap());
        let recovery = namespace
            .maintenance_diagnostics()
            .unwrap()
            .aggregate
            .recovery;
        assert_eq!(
            (
                recovery.attempts,
                recovery.failed,
                recovery.interrupted,
                recovery.in_flight
            ),
            (1, 1, 1, 0)
        );
        assert!(recovery.last_successful_pass.is_none());
    }

    #[test]
    fn receipt_replays_do_not_invent_additional_maintenance_passes() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = Namespace::initialize_identity(
            &"a".repeat(64),
            temp.path(),
            crate::managed::policy::fixture_policy(),
        )
        .unwrap();
        namespace.activate().unwrap();
        let policy = namespace.policy().unwrap();
        let request = CollectionRequest {
            policy_version: policy.version,
            allocation_version: 1,
            bounds: CollectionBounds::from_policy(&policy.policy.collection),
            cursor: None,
        };
        let operation = namespace
            .accept_system_operation(
                Token::parse("observed-pass").unwrap(),
                "collection",
                serde_json::to_value(&request).unwrap(),
            )
            .unwrap();
        let first = namespace.collect_pass(&operation.id, &request).unwrap();
        let replay = namespace.collect_pass(&operation.id, &request).unwrap();
        assert_eq!(
            serde_json::to_value(first).unwrap(),
            serde_json::to_value(replay).unwrap()
        );
        let collection = namespace
            .maintenance_diagnostics()
            .unwrap()
            .aggregate
            .collection;
        assert_eq!(
            (
                collection.attempts,
                collection.completed,
                collection.failed,
                collection.in_flight
            ),
            (1, 1, 0, 0)
        );
        assert!(collection.last_successful_pass.is_some());
        let path = namespace.path().to_owned();
        drop(namespace);
        let reopened = Namespace::open(&path).unwrap();
        let diagnostics = reopened.maintenance_diagnostics().unwrap();
        assert_eq!(diagnostics.aggregate.collection.attempts, 0);
        assert!(
            diagnostics
                .aggregate
                .collection
                .last_successful_pass
                .is_none()
        );
        assert!(matches!(
            diagnostics.prior_instance_totals,
            Measurement::Unavailable { .. }
        ));
    }
}
