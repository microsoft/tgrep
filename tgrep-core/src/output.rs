// Copyright (c) Microsoft Corporation. All rights reserved.

use crate::Result;
use crate::managed::lifetime::ObjectGuard;
use crate::managed::work::{ChargedWriter, WorkPermit};
use crate::ondisk::IndexLayout;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One writer implementation, with a capability rather than a mutable managed path.
#[derive(Clone)]
pub(crate) enum Output {
    Legacy(PathBuf),
    Compatibility {
        path: PathBuf,
        permit: Arc<WorkPermit>,
    },
    Managed {
        pin: Arc<ObjectGuard>,
        permit: Arc<WorkPermit>,
    },
}

impl Output {
    pub(crate) fn legacy(path: &Path) -> Self {
        Self::Legacy(path.to_path_buf())
    }
    pub(crate) fn managed(pin: Arc<ObjectGuard>, permit: Arc<WorkPermit>) -> Self {
        Self::Managed { pin, permit }
    }
    pub(crate) fn compatibility(path: &Path, permit: &Arc<WorkPermit>) -> Self {
        Self::Compatibility {
            path: path.to_path_buf(),
            permit: Arc::clone(permit),
        }
    }
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Legacy(path) | Self::Compatibility { path, .. } => path,
            Self::Managed { pin, .. } => pin.directory.path(),
        }
    }
    pub(crate) fn layout(&self) -> IndexLayout {
        match self {
            Self::Legacy(_) | Self::Compatibility { .. } => IndexLayout::Legacy,
            Self::Managed { .. } => IndexLayout::Managed,
        }
    }
    pub(crate) fn control(&self) -> Option<&Arc<WorkPermit>> {
        match self {
            Self::Managed { permit, .. } | Self::Compatibility { permit, .. } => Some(permit),
            _ => None,
        }
    }
    pub(crate) fn check(&self) -> Result<()> {
        if let Some(permit) = self.control() {
            permit.check()?;
        }
        Ok(())
    }
    pub(crate) fn create(&self, name: &str) -> Result<OutputFile> {
        Ok(match self {
            Self::Legacy(path) => {
                crate::managed::reject_unguarded(path)?;
                std::fs::create_dir_all(path)?;
                OutputFile::Legacy(File::create(path.join(name))?)
            }
            Self::Managed { pin, permit } => OutputFile::Managed(ChargedWriter::new(
                Arc::clone(pin),
                name,
                Arc::clone(permit),
            )?),
            Self::Compatibility { path, permit } => {
                crate::managed::reject_unguarded(path)?;
                permit.check()?;
                let directory = crate::managed::storage::Directory::open(path)?;
                OutputFile::Compatibility {
                    file: directory.create_file(name)?,
                    permit: Arc::clone(permit),
                }
            }
        })
    }

    pub(crate) fn open_file(&self, name: &str) -> Result<File> {
        match self {
            Self::Managed { pin, .. } => Ok(pin.directory.open_file(name, false)?),
            Self::Legacy(path) => Ok(File::open(path.join(name))?),
            Self::Compatibility { path, permit } => {
                permit.check()?;
                Ok(crate::managed::storage::Directory::open(path)?.open_file(name, false)?)
            }
        }
    }
    pub(crate) fn write_json(&self, name: &str, value: &impl serde::Serialize) -> Result<()> {
        self.write_json_hashed(name, value).map(|_| ())
    }

    pub(crate) fn write_json_hashed(
        &self,
        name: &str,
        value: &impl serde::Serialize,
    ) -> Result<[u8; 32]> {
        let mut file = BufWriter::new(self.create(name)?);
        let mut hasher = blake3::Hasher::new();
        {
            let mut writer = HashingWriter {
                writer: &mut file,
                hasher: &mut hasher,
                permit: self.control(),
            };
            serde_json::to_writer(&mut writer, value)?;
        }
        file.flush()?;
        file.get_ref().sync()?;
        Ok(*hasher.finalize().as_bytes())
    }
    pub(crate) fn open_base(&self) -> Result<crate::shared::SharedBase> {
        let Some(permit) = self.control() else {
            return crate::shared::SharedBase::open(self.path());
        };
        let limits =
            crate::reader::SnapshotLimits::measure(self.layout(), |name| self.open_file(name))?;
        let memory = permit.memory(limits.private_estimate()?)?;
        let mut base = match self {
            Self::Legacy(path) => crate::shared::SharedBase::open(path)?,
            Self::Compatibility { path, permit } => {
                crate::shared::SharedBase::open_controlled(path, permit, &limits)?
            }
            Self::Managed { pin, permit } => {
                crate::shared::SharedBase::open_managed(Arc::clone(pin), Some(permit), &limits)?
            }
        };
        base.retain_memory(memory.retain(limits.mapped_bytes()?)?)?;
        Ok(base)
    }
    pub(crate) fn spill(&self) -> Result<Option<Self>> {
        match self {
            Self::Legacy(_) => Ok(None),
            Self::Managed { permit, .. } | Self::Compatibility { permit, .. } => {
                permit.check()?;
                let (_, pin) = permit.namespace.create_object(
                    crate::managed::ObjectKind::BuildStage,
                    None,
                    permit,
                )?;
                Ok(Some(Self::managed(pin, Arc::clone(permit))))
            }
        }
    }
}

pub(crate) enum OutputFile {
    Legacy(File),
    Compatibility { file: File, permit: Arc<WorkPermit> },
    Managed(ChargedWriter),
}

impl OutputFile {
    pub(crate) fn sync(&self) -> Result<()> {
        match self {
            Self::Legacy(file) | Self::Compatibility { file, .. } => Ok(file.sync_all()?),
            Self::Managed(file) => Ok(file.sync_all()?),
        }
    }
}

impl Write for OutputFile {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Legacy(file) => file.write(bytes),
            Self::Managed(file) => file.write(bytes),
            Self::Compatibility { file, permit } => permit.write(file, bytes),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Legacy(file) => file.flush(),
            Self::Compatibility { file, permit } => {
                permit.check().map_err(std::io::Error::other)?;
                file.flush()
            }
            Self::Managed(file) => file.flush(),
        }
    }
}

struct HashingWriter<'a, W> {
    writer: &'a mut W,
    hasher: &'a mut blake3::Hasher,
    permit: Option<&'a Arc<WorkPermit>>,
}

impl<W: Write> Write for HashingWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if let Some(permit) = self.permit {
            permit.check().map_err(std::io::Error::other)?;
        }
        let written = self.writer.write(&bytes[..bytes.len().min(64 * 1024)])?;
        self.hasher.update(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

pub(crate) fn checksum(
    value: &impl serde::Serialize,
    permit: Option<&Arc<WorkPermit>>,
) -> Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    serde_json::to_writer(
        HashingWriter {
            writer: &mut std::io::sink(),
            hasher: &mut hasher,
            permit,
        },
        value,
    )?;
    Ok(*hasher.finalize().as_bytes())
}
