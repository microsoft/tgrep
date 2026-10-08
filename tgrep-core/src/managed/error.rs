// Copyright (c) Microsoft Corporation. All rights reserved.

use serde::{Deserialize, Serialize};
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCategory {
    InvalidInput,
    Busy,
    StaleIdentity,
    StaleVersion,
    CacheEvicted,
    CacheMissing,
    GitObjectsUnavailable,
    Incompatible,
    CorruptMetadata,
    ResourcePressure,
    Permission,
    Io,
    Cancelled,
    Deadline,
    ReceiptExpired,
    RecoveryRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommitState {
    NotCommitted,
    Committed,
    Unknown,
}

/// `detail` is local diagnostic information, not an aggregate telemetry field.
#[derive(Debug, Serialize, Deserialize)]
pub struct Error {
    pub category: ErrorCategory,
    pub reason_code: String,
    pub retryable: bool,
    pub committed_state: CommitState,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<u64>,
    #[serde(skip)]
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Error {
    pub fn new(category: ErrorCategory, reason: &str, detail: impl Into<String>) -> Self {
        Self {
            category,
            reason_code: reason.into(),
            retryable: matches!(
                category,
                ErrorCategory::Busy
                    | ErrorCategory::ResourcePressure
                    | ErrorCategory::Io
                    | ErrorCategory::RecoveryRequired
            ),
            committed_state: CommitState::NotCommitted,
            detail: detail.into(),
            operation_id: None,
            current_version: None,
            source: None,
        }
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::new(ErrorCategory::InvalidInput, "invalid-input", detail)
    }

    pub fn corrupt(detail: impl Into<String>) -> Self {
        Self::new(ErrorCategory::CorruptMetadata, "invalid-catalog", detail)
    }

    pub fn busy(reason: &str) -> Self {
        Self::new(ErrorCategory::Busy, reason, reason)
    }

    pub fn pressure(reason: &str) -> Self {
        Self::new(ErrorCategory::ResourcePressure, reason, reason)
    }

    pub fn incompatible(detail: impl Into<String>) -> Self {
        Self::new(ErrorCategory::Incompatible, "incompatible-storage", detail)
    }

    pub fn stale_version(version: u64) -> Self {
        let mut error = Self::new(
            ErrorCategory::StaleVersion,
            "stale-version",
            "the authoritative version changed",
        );
        error.current_version = Some(version);
        error
    }

    pub fn committed(mut self, state: CommitState) -> Self {
        self.committed_state = state;
        self
    }

    pub fn operation(mut self, id: impl Into<String>) -> Self {
        self.operation_id = Some(id.into());
        self
    }

    pub fn io(error: std::io::Error) -> Self {
        if let Some(managed) = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<Self>())
        {
            return Self {
                category: managed.category,
                reason_code: managed.reason_code.clone(),
                retryable: managed.retryable,
                committed_state: managed.committed_state,
                detail: managed.detail.clone(),
                operation_id: managed.operation_id.clone(),
                current_version: managed.current_version,
                source: Some(Box::new(error)),
            };
        }
        #[cfg(windows)]
        if matches!(error.raw_os_error(), Some(32 | 33 | 1224)) {
            let mut result = Self::new(
                ErrorCategory::Busy,
                "windows-sharing-or-mapping",
                error.to_string(),
            );
            result.source = Some(Box::new(error));
            return result;
        }
        let category = match error.kind() {
            std::io::ErrorKind::PermissionDenied => ErrorCategory::Permission,
            std::io::ErrorKind::WouldBlock => ErrorCategory::Busy,
            std::io::ErrorKind::StorageFull | std::io::ErrorKind::OutOfMemory => {
                ErrorCategory::ResourcePressure
            }
            _ => ErrorCategory::Io,
        };
        let reason = match error.kind() {
            std::io::ErrorKind::StorageFull => "storage-full",
            std::io::ErrorKind::OutOfMemory => "allocation-failed",
            _ => "filesystem-io",
        };
        let mut result = Self::new(category, reason, error.to_string());
        result.source = Some(Box::new(error));
        result
    }

    pub fn source_io_kind(&self) -> Option<std::io::ErrorKind> {
        self.source
            .as_deref()
            .and_then(|error| error.downcast_ref::<std::io::Error>())
            .map(std::io::Error::kind)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.reason_code, self.detail)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::io(error)
    }
}

impl From<ignore::Error> for Error {
    fn from(error: ignore::Error) -> Self {
        let detail = error.to_string();
        match error.into_io_error() {
            Some(error) => Self::io(error),
            None => Self::invalid(detail),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        if error.is_io() {
            return Self::io(error.into());
        }
        let mut result = Self::corrupt(error.to_string());
        result.source = Some(Box::new(error));
        result
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        let category = match error.sqlite_error_code() {
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
                ErrorCategory::Busy
            }
            Some(rusqlite::ErrorCode::DiskFull | rusqlite::ErrorCode::OutOfMemory) => {
                ErrorCategory::ResourcePressure
            }
            Some(rusqlite::ErrorCode::PermissionDenied | rusqlite::ErrorCode::ReadOnly) => {
                ErrorCategory::Permission
            }
            Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => {
                ErrorCategory::CorruptMetadata
            }
            None => ErrorCategory::CorruptMetadata,
            _ => ErrorCategory::Io,
        };
        let mut result = Self::new(category, "catalog-io", error.to_string());
        result.source = Some(Box::new(error));
        result
    }
}

impl From<crate::generations::GenerationError> for Error {
    fn from(error: crate::generations::GenerationError) -> Self {
        use crate::generations::GenerationError;
        if let GenerationError::Index(error) = error {
            return error.into();
        }
        if let GenerationError::Io(error) = error {
            return error.into();
        }
        if let GenerationError::Json(error) = error {
            return error.into();
        }
        let category = match &error {
            GenerationError::Git { .. } => ErrorCategory::GitObjectsUnavailable,
            GenerationError::Unsupported(_) | GenerationError::Incompatible(_) => {
                ErrorCategory::Incompatible
            }
            GenerationError::InvalidMetadata(_) => ErrorCategory::CorruptMetadata,
            _ => ErrorCategory::Io,
        };
        let mut result = Self::new(category, "generation-error", error.to_string());
        result.source = Some(Box::new(error));
        result
    }
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        if let crate::Error::Managed(error) = error {
            return *error;
        }
        if let crate::Error::Io(error) = error {
            return error.into();
        }
        if let crate::Error::Json(error) = error {
            return error.into();
        }
        let category = match &error {
            crate::Error::IndexNotFound(_) => ErrorCategory::CacheMissing,
            crate::Error::IndexCorrupted(_) => ErrorCategory::CorruptMetadata,
            _ => ErrorCategory::Io,
        };
        let mut result = Self::new(category, "index-error", error.to_string());
        result.source = Some(Box::new(error));
        result
    }
}

impl From<crate::worktrees::WorktreeError> for Error {
    fn from(error: crate::worktrees::WorktreeError) -> Self {
        use crate::worktrees::WorktreeError;
        match error {
            WorktreeError::Io(error) => error.into(),
            WorktreeError::Index(error) => error.into(),
            WorktreeError::Generation(error) => error.into(),
            WorktreeError::NotReady
            | WorktreeError::ChangedDuringReconcile
            | WorktreeError::UnstableFile(_) => {
                Self::new(ErrorCategory::Busy, "worktree-not-ready", error.to_string())
            }
            WorktreeError::InvalidInput(_) => Self::invalid(error.to_string()),
            WorktreeError::Synchronization => Self::corrupt(error.to_string()),
            WorktreeError::IncompleteWalk(_) => {
                Self::new(ErrorCategory::Io, "incomplete-walk", error.to_string())
            }
        }
    }
}
