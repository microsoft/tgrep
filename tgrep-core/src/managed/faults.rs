// Copyright (c) Microsoft Corporation. All rights reserved.

use super::{Id, Namespace, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Point {
    CatalogBeforeCommit,
    CatalogAfterCommit,
    ObjectIntentSaved,
    ObjectDirectoryCreated,
    ObjectDirectorySealed,
    ObjectGuardCreated,
    ObjectGuardSealed,
    MemberCreationIntentSaved,
    MemberCreated,
    MemberSealBeforeCommit,
    MemberSealAfterCommit,
    VerificationMemoryAdmission,
    MemberVerificationPage,
    MemberVerified,
    GenerationBuilt,
    GenerationPublished,
    CheckpointPublished,
    MigrationPrepared,
    ViewBeforeCommit,
    ViewAfterCommit,
    ViewAfterSwap,
    ObjectWithdrawn,
    ObjectPendingDeletion,
    MemberIntentSaved,
    MemberBeforeIo,
    MemberAfterIo,
    MemberBeforeCredit,
    MemberAfterCredit,
    ObjectBeforeRemove,
    ObjectAfterRemove,
    GuardAfterRemove,
    ObjectRemoved,
    ControlIntentSaved,
    ControlAfterRemove,
    ControlBeforeCredit,
    IdleAccepted,
    IdleAdmissionClosed,
    IdleCommitted,
}

impl Namespace {
    pub(crate) fn fault(&self, point: Point, operation: Option<&Id>) -> Result<()> {
        #[cfg(any(test, feature = "managed-test-hooks"))]
        {
            self.faults.hit(point, operation)
        }
        #[cfg(not(any(test, feature = "managed-test-hooks")))]
        {
            let _ = (point, operation);
            Ok(())
        }
    }
}

#[cfg(any(test, feature = "managed-test-hooks"))]
mod enabled {
    use super::*;
    use crate::managed::{Error, ErrorCategory};
    use std::sync::{Condvar, Mutex};
    use std::time::{Duration, Instant};

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
    pub enum Action {
        Pause { timeout_ms: u64 },
        Error { category: ErrorCategory },
        OsError { code: i32 },
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Specification {
        pub point: Point,
        pub operation: Option<Id>,
        pub skip_hits: u32,
        pub action: Action,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum Stage {
        Armed,
        Waiting,
        Released,
        Fired,
        Expired,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Status {
        pub ticket: Id,
        pub specification: Specification,
        pub stage: Stage,
        pub hits: u32,
        pub reached_operation: Option<Id>,
    }

    #[derive(Default)]
    pub(crate) struct Faults {
        state: Mutex<Option<Status>>,
        changed: Condvar,
    }

    impl Faults {
        fn install(&self, specification: Specification) -> Result<Status> {
            if specification.skip_hits > 10_000 {
                return Err(Error::invalid("test hook skip_hits exceeds 10000"));
            }
            if matches!(specification.action, Action::OsError { code } if code <= 0) {
                return Err(Error::invalid("test hook OS error code must be positive"));
            }
            if matches!(specification.action, Action::Pause { timeout_ms } if timeout_ms == 0 || timeout_ms > 30_000)
            {
                return Err(Error::invalid("test hook timeout_ms must be 1..=30000"));
            }
            let mut state = self
                .state
                .lock()
                .map_err(|_| Error::corrupt("test hook lock poisoned"))?;
            if state
                .as_ref()
                .is_some_and(|state| matches!(state.stage, Stage::Armed | Stage::Waiting))
            {
                return Err(Error::busy("test-hook-already-active"));
            }
            let status = Status {
                ticket: Id::new()?,
                specification,
                stage: Stage::Armed,
                hits: 0,
                reached_operation: None,
            };
            *state = Some(status.clone());
            Ok(status)
        }

        fn status(&self) -> Result<Option<Status>> {
            Ok(self
                .state
                .lock()
                .map_err(|_| Error::corrupt("test hook lock poisoned"))?
                .clone())
        }

        fn release(&self, ticket: &Id) -> Result<Status> {
            let mut state = self
                .state
                .lock()
                .map_err(|_| Error::corrupt("test hook lock poisoned"))?;
            let status = state
                .as_mut()
                .filter(|state| &state.ticket == ticket)
                .ok_or_else(|| {
                    Error::new(
                        ErrorCategory::StaleIdentity,
                        "test-hook-ticket",
                        "test hook ticket is not current",
                    )
                })?;
            if matches!(status.stage, Stage::Armed | Stage::Waiting) {
                status.stage = Stage::Released;
            }
            self.changed.notify_all();
            Ok(status.clone())
        }

        pub(crate) fn hit(&self, point: Point, operation: Option<&Id>) -> Result<()> {
            let mut state = self
                .state
                .lock()
                .map_err(|_| Error::corrupt("test hook lock poisoned"))?;
            let Some(status) = state.as_mut() else {
                return Ok(());
            };
            if status.stage != Stage::Armed
                || status.specification.point != point
                || status
                    .specification
                    .operation
                    .as_ref()
                    .is_some_and(|expected| Some(expected) != operation)
            {
                return Ok(());
            }
            status.hits += 1;
            if status.hits <= status.specification.skip_hits {
                return Ok(());
            }
            status.reached_operation = operation.cloned();
            match status.specification.action {
                Action::Error { category } => {
                    status.stage = Stage::Fired;
                    Err(Error::new(
                        category,
                        "injected-test-failure",
                        format!("namespace test boundary {point:?}"),
                    ))
                }
                Action::OsError { code } => {
                    status.stage = Stage::Fired;
                    Err(Error::io(std::io::Error::from_raw_os_error(code)))
                }
                Action::Pause { timeout_ms } => {
                    status.stage = Stage::Waiting;
                    let ticket = status.ticket.clone();
                    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
                    loop {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        let (next, _) = self
                            .changed
                            .wait_timeout(state, remaining)
                            .map_err(|_| Error::corrupt("test hook lock poisoned"))?;
                        state = next;
                        let current = state
                            .as_mut()
                            .filter(|status| status.ticket == ticket)
                            .ok_or_else(|| Error::corrupt("waiting test hook was replaced"))?;
                        if current.stage != Stage::Waiting {
                            return Ok(());
                        }
                        if Instant::now() >= deadline {
                            current.stage = Stage::Expired;
                            return Err(Error::new(
                                ErrorCategory::Deadline,
                                "test-hook-deadline",
                                "test barrier was not released",
                            ));
                        }
                    }
                }
            }
        }
    }

    impl Namespace {
        /// Deterministic, namespace-local instrumentation, excluded from ordinary builds.
        pub fn install_test_fault(&self, specification: Specification) -> Result<Status> {
            self.faults.install(specification)
        }

        pub fn test_fault_status(&self) -> Result<Option<Status>> {
            self.faults.status()
        }

        pub fn release_test_fault(&self, ticket: &Id) -> Result<Status> {
            self.faults.release(ticket)
        }
    }
}

#[cfg(any(test, feature = "managed-test-hooks"))]
pub(crate) use enabled::Faults;
#[cfg(any(test, feature = "managed-test-hooks"))]
pub use enabled::{Action, Specification, Stage, Status};
