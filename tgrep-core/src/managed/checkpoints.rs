// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::{object_row, save_object};
use super::lifetime::ObjectGuard;
use super::{
    Error, ErrorCategory, FileIdentity, Id, Namespace, NativePath, ObjectKind, ObjectState,
    ReferenceKind, Result, WorkPermit,
};
use crate::generations::{Generation, GenerationKey};
use crate::output::Output;
use crate::shared::OverlayCheckpoint;
use crate::worktrees::{WorktreeOptions, WorktreeView};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentPin {
    pub commit: String,
    pub key: GenerationKey,
    /// None only in explicit legacy-storage / retain-all compatibility mode.
    pub incarnation: Option<Id>,
    pub fingerprint: [u8; 32],
}

impl CurrentPin {
    pub fn from_materialization(materialization: &super::Materialization) -> Self {
        let descriptor = &materialization.descriptor;
        Self {
            commit: descriptor.requested_commit.clone(),
            key: descriptor.key.clone(),
            incarnation: Some(descriptor.incarnation.clone()),
            fingerprint: descriptor.fingerprint,
        }
    }

    pub(crate) fn validate_generation(&self, generation: &Generation) -> Result<()> {
        if &self.key != generation.key()
            || self.fingerprint != generation.base().snapshot_id()
            || self.incarnation.as_ref()
                != generation
                    .base()
                    .reader()
                    .managed_identity()
                    .map(|(_, object)| object)
        {
            return Err(Error::corrupt(
                "current pin and protected generation differ",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointBinding {
    pub schema: u32,
    pub namespace: Id,
    pub repository: String,
    pub view: Id,
    pub root: NativePath,
    pub root_identity: FileIdentity,
    pub pin: CurrentPin,
    pub view_version: u64,
    pub input_epoch: u64,
    pub reconciled_epoch: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointDescriptor {
    pub incarnation: Id,
    pub binding: CheckpointBinding,
    pub logical_bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    binding: CheckpointBinding,
    overlay: OverlayCheckpoint,
    contents: Vec<(String, crate::meta::ContentId)>,
}

pub struct ProtectedCheckpoint {
    pub descriptor: CheckpointDescriptor,
    _guard: Arc<ObjectGuard>,
}

impl Namespace {
    pub fn save_checkpoint(
        self: &Arc<Self>,
        view: &WorktreeView,
        identity: &Id,
        version: u64,
        input_epoch: u64,
        pin: &CurrentPin,
        permit: &Arc<WorkPermit>,
    ) -> Result<ProtectedCheckpoint> {
        if !Arc::ptr_eq(self, &permit.namespace) || version == 0 {
            return Err(Error::invalid(
                "checkpoint needs its namespace reservation and positive view version",
            ));
        }
        pin.validate_generation(view.generation())?;
        if pin.key.repository_identity() != self.header().repository {
            return Err(Error::invalid("checkpoint belongs to another repository"));
        }
        if pin.incarnation.is_none()
            && self.header().storage != super::policy::StorageMode::CompatibilityRetainAll
        {
            return Err(Error::incompatible(
                "managed checkpoints require managed generations",
            ));
        }
        let captured = view.capture_checkpoint(permit)?;
        let binding = CheckpointBinding {
            schema: super::STORAGE_VERSION,
            namespace: self.header().namespace.clone(),
            repository: self.header().repository.clone(),
            view: identity.clone(),
            root: NativePath::from_path(view.root())?,
            root_identity: view.root_identity()?,
            pin: pin.clone(),
            view_version: version,
            input_epoch,
            reconciled_epoch: captured.epoch,
        };
        let (record, guard) = self.create_object(ObjectKind::Checkpoint, None, permit)?;
        let result = (|| {
            let output = Output::managed(Arc::clone(&guard), Arc::clone(permit));
            let fingerprint = output.write_json_hashed(
                "checkpoint.tgm",
                &Envelope {
                    binding: binding.clone(),
                    overlay: captured.checkpoint,
                    contents: captured.contents,
                },
            )?;
            guard.directory.sync()?;
            permit.check()?;
            let object = self.transaction(|transaction| {
                let mut object = object_row(transaction, &record.id)?;
                if object.state != ObjectState::Preparing {
                    return Err(Error::busy("checkpoint-no-longer-preparing"));
                }
                super::catalog::ensure_object_sealed(transaction, &record.id)?;
                object.state = ObjectState::Published;
                object.binding = Some(serde_json::to_value(&binding)?);
                object.fingerprint = Some(fingerprint);
                save_object(transaction, &mut object)?;
                if let Some(generation) = &pin.incarnation {
                    Self::add_reference(
                        transaction,
                        &Id::new()?,
                        ReferenceKind::Checkpoint,
                        record.id.as_str(),
                        generation,
                        None,
                    )?;
                }
                Ok(object)
            })?;
            self.fault(
                super::faults::Point::CheckpointPublished,
                Some(permit.operation_id()),
            )
            .map_err(|error| error.committed(super::CommitState::Committed))?;
            Ok(ProtectedCheckpoint {
                descriptor: CheckpointDescriptor {
                    incarnation: record.id.clone(),
                    binding,
                    logical_bytes: object.logical_bytes,
                },
                _guard: guard,
            })
        })();
        if let Err(error) = &result {
            self.transaction(|transaction| {
                let mut object = object_row(transaction, &record.id)?;
                if object.state == ObjectState::Preparing {
                    object.state = ObjectState::Retired;
                    object.error = Some(serde_json::to_value(error)?);
                    save_object(transaction, &mut object)?;
                }
                Ok(())
            })?;
        }
        result
    }

    pub fn checkpoint(&self, id: &Id) -> Result<CheckpointDescriptor> {
        let object = self.object(id)?;
        if object.kind != ObjectKind::Checkpoint {
            return Err(Error::invalid("incarnation is not a checkpoint"));
        }
        if object.state != ObjectState::Published {
            return Err(Error::new(
                ErrorCategory::CacheEvicted,
                "checkpoint-withdrawn",
                "checkpoint is no longer restorable",
            ));
        }
        let binding: CheckpointBinding = serde_json::from_value(
            object
                .binding
                .ok_or_else(|| Error::corrupt("checkpoint binding is absent"))?,
        )?;
        if binding.namespace != self.header().namespace
            || binding.schema != super::STORAGE_VERSION
            || binding.repository != self.header().repository
        {
            return Err(Error::corrupt(
                "checkpoint binding differs from its namespace",
            ));
        }
        Ok(CheckpointDescriptor {
            incarnation: id.clone(),
            binding,
            logical_bytes: object.logical_bytes,
        })
    }

    pub fn restore_checkpoint(
        self: &Arc<Self>,
        id: &Id,
        expected: &CheckpointBinding,
        root: &Path,
        generation: Arc<Generation>,
        options: WorktreeOptions,
        permit: &Arc<WorkPermit>,
    ) -> Result<WorktreeView> {
        if !Arc::ptr_eq(self, &permit.namespace) {
            return Err(Error::invalid(
                "restore reservation belongs to another namespace",
            ));
        }
        permit.check()?;
        let guard = self.pin(id)?;
        let descriptor = self.checkpoint(id)?;
        if &descriptor.binding != expected {
            return Err(Error::new(
                ErrorCategory::StaleVersion,
                "checkpoint-binding-changed",
                "restore requires the exact recorded binding",
            ));
        }
        expected.pin.validate_generation(&generation)?;
        let memory = descriptor
            .logical_bytes
            .checked_mul(32)
            .ok_or_else(|| Error::pressure("checkpoint-restore-memory-overflow"))?;
        let memory = permit.memory(memory)?;
        let bytes = super::inputs::read_bytes(
            guard.directory.open_file("checkpoint.tgm", false)?,
            descriptor.logical_bytes,
            Some(permit),
        )?;
        if self.object(id)?.fingerprint != Some(super::inputs::hash_bytes(&bytes, Some(permit))?) {
            return Err(Error::corrupt("checkpoint payload checksum differs"));
        }
        let envelope: Envelope =
            super::inputs::read_json(bytes.as_slice(), bytes.len() as u64, Some(permit))?;
        drop(bytes);
        if envelope.binding != descriptor.binding {
            return Err(Error::corrupt(
                "checkpoint payload and catalog binding differ",
            ));
        }
        let view = WorktreeView::new_controlled(root, generation, options, permit)?;
        if NativePath::from_path(view.root())? != expected.root
            || view.root_identity()? != expected.root_identity
        {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "checkpoint-root-replaced",
                "checkpoint belongs to a different physical worktree",
            ));
        }
        view.restore_checkpoint_value(envelope.overlay, envelope.contents, permit, memory)?;
        permit.check()?;
        Ok(view)
    }
}
