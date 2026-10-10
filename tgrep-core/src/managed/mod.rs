// Copyright (c) Microsoft Corporation. All rights reserved.

//! Versioned ownership and lifecycle of repository-scoped shared indexes.
//!
//! Managed storage is a separate format. Legacy generation managers remain
//! retain-all; a managed object can only be opened with an ownership guard.

mod accounting;
mod adaptive;
mod authentication;
mod authentication_native;
mod catalog;
mod catalog_io;
mod checkpoints;
mod clock;
mod collection;
mod diagnostics;
mod error;
#[doc(hidden)]
pub mod faults;
mod generations;
mod housekeeping;
pub(crate) mod inputs;
mod inventory;
pub(crate) mod lifetime;
mod maintenance;
mod management;
pub(crate) mod memory;
mod owners;
pub mod policy;
mod private_control;
pub(crate) mod process;
pub(crate) mod roots;
pub(crate) mod storage;
mod verification;
mod views;
pub(crate) mod work;

pub use crate::shared::CheckpointRoot as NativePath;
pub use accounting::{InspectedObject, InspectionTotals, StorageInspection, StorageUsage};
pub use adaptive::{AdaptiveDecision, AdaptiveRequest};
pub use authentication::MemberSeal;
pub use catalog::{
    CatalogCursor, CatalogPage, FileRecord, Namespace, NamespaceHeader, ObjectRecord,
    OperationRecord, OwnerRecord, VersionedPolicy,
};
pub use checkpoints::{CheckpointBinding, CheckpointDescriptor, CurrentPin, ProtectedCheckpoint};
pub use collection::{
    CollectionBounds, CollectionCursor, CollectionPreview, CollectionProgress, CollectionRequest,
    CollectionSkip, Eligibility, ReferenceCount,
};
pub use diagnostics::{
    MaintenanceAggregate, MaintenanceDiagnostics, PassDiagnostics, SuccessfulPass,
};
pub use error::{CommitState, Error, ErrorCategory, Result};
pub use generations::{GenerationDescriptor, Materialization, open_generation};
pub use housekeeping::CleanupCounts;
pub use inventory::{
    DiscoveredNamespace, DiscoveryPage, InventoryCursor, InventoryEntry, InventoryPage,
    NamespaceDiscovery,
};
pub use lifetime::{OwnerClaim, OwnerGuard, OwnerProof};
pub use maintenance::{
    ExternalWork, IdleOutcome, MaintenanceIssue, RecoveryCursor, RecoveryProgress, RecoveryRequest,
};
pub use management::{
    MetadataMutation, OperationReadGuard, publish_control_file, remove_control_file,
};
pub use memory::MemoryUsage;
pub use owners::OwnerRelease;
pub use policy::Policy;
pub use private_control::{publish_private_control_file, read_private_control_file};
#[cfg(feature = "managed-test-hooks")]
#[doc(hidden)]
pub use process::SupervisedChild;
pub use roots::RootAnchor;
pub use storage::{FileIdentity, Ownership};
pub use views::{
    AttachRequest, DetachResult, LeaseRecord, LegacyAttachRequest, MigrationRequest, ObserveView,
    OriginalAttachment, PinIntent, PublishedView, ReconcileRequest, RefreshRequest, ViewManager,
    ViewQuery, ViewRecord, ViewSlot, ViewStatus, ViewVersion,
};
pub use work::{Allocation, ReservationRecord, WorkPermit, WorkRequest, WorkUsage};

use serde::{Deserialize, Deserializer, Serialize};

pub const PROTOCOL_VERSION: u32 = 2;
pub const STORAGE_VERSION: u32 = 2;
pub const STORE_DIRECTORY: &str = "tgrep-managed-v2";
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

pub(crate) fn reject_unguarded(path: &std::path::Path) -> crate::Result<()> {
    if path.join("paths.tgm").try_exists()?
        || path.ancestors().any(|ancestor| {
            ancestor
                .file_name()
                .is_some_and(|name| name == STORE_DIRECTORY)
        })
    {
        return Err(
            Error::incompatible("managed storage requires a protected managed reader").into(),
        );
    }
    Ok(())
}

/// A physical incarnation, never a logical generation key or a filesystem path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Id(String);

impl Id {
    pub fn new() -> Result<Self> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|error| Error::io(std::io::Error::other(error.to_string())))?;
        let mut value = String::with_capacity(32);
        for byte in bytes {
            use std::fmt::Write;
            write!(value, "{byte:02x}").expect("writing a string");
        }
        Ok(Self(value))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::invalid(
                "identity must contain 32 lowercase hex digits",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn object(namespace: &str, sequence: u64) -> Self {
        let prefix = format!("{sequence:016x}");
        let digest = blake3::hash(format!("{namespace}:{prefix}").as_bytes()).to_hex();
        Self(format!("{prefix}{}", &digest.as_str()[..16]))
    }

    pub(crate) fn allocated_object(&self, namespace: &str, through: u64) -> bool {
        u64::from_str_radix(&self.0[..16], 16).is_ok_and(|sequence| {
            sequence != 0 && sequence <= through && *self == Self::object(namespace, sequence)
        })
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Caller-persisted idempotency token. Token reuse with other input is a conflict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        {
            return Err(Error::invalid(
                "token must contain 1-128 ASCII letters, digits, hyphens or underscores",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Token {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObjectKind {
    Generation,
    Checkpoint,
    BuildStage,
    MigrationStage,
    CheckpointStage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObjectState {
    Preparing,
    Published,
    Retired,
    PendingDeletion,
    Removed,
    Quarantined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReferenceKind {
    View,
    Lease,
    Query,
    Reader,
    Attachment,
    Builder,
    Predecessor,
    Checkpoint,
    CheckpointSave,
    CheckpointRestore,
    Migration,
    Recovery,
    Persistent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationState {
    Accepted,
    Preparing,
    Committed,
    Completed,
    Cancelling,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationToken {
    pub scope: Id,
    pub sequence: u64,
    pub token: Token,
}

impl OperationToken {
    pub fn validate(&self) -> Result<()> {
        if self.sequence == 0 || self.sequence > i64::MAX as u64 {
            return Err(Error::invalid("operation sequence must be in 1..=i64::MAX"));
        }
        Ok(())
    }
}

/// Unknown measurements are explicit; zero is reserved for an observed zero.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Measurement<T> {
    Observed { value: T },
    Unavailable { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_are_not_paths_or_lossy_aliases() {
        let id = Id::new().unwrap();
        assert_eq!(
            serde_json::from_str::<Id>(&serde_json::to_string(&id).unwrap()).unwrap(),
            id
        );
        for value in ["", "../other", "0123456789012345678901234567890F", "a/b"] {
            assert!(Id::parse(value).is_err());
        }
        for value in ["", "a/b", "a b", "\u{fffd}"] {
            assert!(Token::parse(value).is_err());
        }
    }

    #[test]
    fn unknown_measurement_cannot_be_serialized_as_zero() {
        let value: Measurement<u64> = Measurement::Unavailable {
            reason: "unsupported-platform-counter".into(),
        };
        let json = serde_json::to_value(value).unwrap();
        assert_eq!(json["status"], "unavailable");
        assert!(json.get("value").is_none());
    }
}
