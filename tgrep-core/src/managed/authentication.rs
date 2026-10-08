// Copyright (c) Microsoft Corporation. All rights reserved.

use super::catalog::{sql_integer, text};
use super::{Error, Id, Namespace, Result};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub(crate) const FORMAT: u32 = 1;
pub(crate) const BLOCK_BYTES: usize = 4096;
pub(crate) const PROOF_BATCH: usize = 64;
pub(crate) const IO_BYTES: usize = 64 * 1024;
pub(crate) const PRODUCER_MEMORY: u64 = (16 * 1024 + super::storage::SECURITY_BYTES) as u64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberSeal {
    pub format: u32,
    pub length: u64,
    pub blocks: u64,
    pub manifest: [u8; 32],
}

impl MemberSeal {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.format != FORMAT
            || self.blocks != self.length.div_ceil(BLOCK_BYTES as u64)
            || (self.blocks == 0 && self.manifest != *manifest_hasher().finalize().as_bytes())
        {
            return Err(Error::corrupt("invalid complete producer seal"));
        }
        Ok(())
    }

    pub(crate) fn bounded(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > BLOCK_BYTES {
            return Err(Error::invalid(
                "control content exceeds its fixed authentication bound",
            ));
        }
        let mut manifest = manifest_hasher();
        if !bytes.is_empty() {
            hash_block(
                &mut manifest,
                &Block {
                    index: 0,
                    length: bytes.len() as u32,
                    digest: *blake3::hash(bytes).as_bytes(),
                },
            );
        }
        Ok(Self {
            format: FORMAT,
            length: bytes.len() as u64,
            blocks: u64::from(!bytes.is_empty()),
            manifest: *manifest.finalize().as_bytes(),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Block {
    pub index: u64,
    pub length: u32,
    pub digest: [u8; 32],
}

pub(crate) fn manifest_hasher() -> blake3::Hasher {
    let mut hash = blake3::Hasher::new();
    hash.update(b"tgrep/managed/member-seals/v1\0");
    hash
}

pub(crate) fn hash_block(hash: &mut blake3::Hasher, block: &Block) {
    hash.update(&block.index.to_le_bytes());
    hash.update(&block.length.to_le_bytes());
    hash.update(&block.digest);
}

/// Expected hashes only consume caller-owned bytes accepted by the producer.
pub(crate) struct ProducerSeal {
    length: u64,
    blocks: u64,
    partial: usize,
    block: blake3::Hasher,
    manifest: blake3::Hasher,
    pending: Vec<Block>,
}

impl ProducerSeal {
    pub(crate) fn new() -> Self {
        Self {
            length: 0,
            blocks: 0,
            partial: 0,
            block: blake3::Hasher::new(),
            manifest: manifest_hasher(),
            pending: Vec::with_capacity(PROOF_BATCH),
        }
    }

    pub(crate) fn available(&self) -> usize {
        ((PROOF_BATCH - self.pending.len()) * BLOCK_BYTES - self.partial).min(IO_BYTES)
    }

    pub(crate) fn accepted(&mut self, mut bytes: &[u8]) {
        assert!(bytes.len() <= self.available());
        self.length += bytes.len() as u64;
        while !bytes.is_empty() {
            let count = bytes.len().min(BLOCK_BYTES - self.partial);
            self.block.update(&bytes[..count]);
            self.partial += count;
            bytes = &bytes[count..];
            if self.partial == BLOCK_BYTES {
                self.finish_block();
            }
        }
    }

    fn finish_block(&mut self) {
        let block = Block {
            index: self.blocks,
            length: self.partial as u32,
            digest: *self.block.finalize().as_bytes(),
        };
        hash_block(&mut self.manifest, &block);
        self.pending.push(block);
        self.blocks += 1;
        self.partial = 0;
        self.block.reset();
    }

    pub(crate) fn flush(&mut self, namespace: &Namespace, object: &Id, name: &str) -> Result<()> {
        if !self.pending.is_empty() {
            namespace.record_seal_blocks(object, name, &self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }

    pub(crate) fn finish(
        mut self,
        namespace: &Namespace,
        object: &Id,
        name: &str,
    ) -> Result<MemberSeal> {
        if self.partial != 0 {
            self.finish_block();
        }
        self.flush(namespace, object, name)?;
        Ok(MemberSeal {
            format: FORMAT,
            length: self.length,
            blocks: self.blocks,
            manifest: *self.manifest.finalize().as_bytes(),
        })
    }
}

impl Namespace {
    fn record_seal_blocks(&self, object: &Id, name: &str, blocks: &[Block]) -> Result<()> {
        if blocks.is_empty() || blocks.len() > PROOF_BATCH {
            return Err(Error::invalid("member proof batch exceeds its fixed bound"));
        }
        self.transaction(|transaction| {
            let record: String = transaction.query_row(
                "SELECT record FROM members WHERE object_id=?1 AND name=?2",
                params![object.as_str(), name],
                |row| row.get(0),
            )?;
            let member: super::FileRecord = serde_json::from_str(&record)?;
            if !member.producer_open || member.seal.is_some() {
                return Err(Error::corrupt("cannot change a finalized producer seal"));
            }
            let last: Option<u64> = transaction.query_row(
                "SELECT block_index FROM member_seals WHERE object_id=?1 AND name=?2 ORDER BY block_index DESC LIMIT 1",
                params![object.as_str(), name],
                |row| super::catalog::unsigned(row, 0),
            ).optional()?;
            let mut next = last.map_or(0, |last| last + 1);
            for block in blocks {
                if block.length == 0 || block.length as usize > BLOCK_BYTES {
                    return Err(Error::corrupt("invalid producer authentication block"));
                }
                let previous: Option<(u32, Vec<u8>)> = transaction.query_row(
                    "SELECT length,substr(digest,1,33) FROM member_seals WHERE object_id=?1 AND name=?2 AND block_index=?3",
                    params![object.as_str(), name, sql_integer(block.index)?],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                ).optional()?;
                if let Some((length, digest)) = previous {
                    if length != block.length || digest.as_slice() != block.digest {
                        return Err(Error::corrupt("a retried producer proof differs from its immutable row"));
                    }
                    continue;
                }
                if block.index != next {
                    return Err(Error::corrupt("producer proof is not a contiguous prefix"));
                }
                transaction.execute(
                    "INSERT INTO member_seals VALUES(?1,?2,?3,?4,?5)",
                    params![
                        object.as_str(),
                        name,
                        sql_integer(block.index)?,
                        block.length,
                        &block.digest[..]
                    ],
                )?;
                next += 1;
            }
            Ok(())
        })
    }

    pub(crate) fn complete_member_seal(
        &self,
        object: &Id,
        name: &str,
        seal: MemberSeal,
    ) -> Result<()> {
        seal.validate()?;
        let operation = self.object(object)?.operation;
        self.fault(
            super::faults::Point::MemberSealBeforeCommit,
            operation.as_ref(),
        )?;
        self.transaction(|transaction| {
            let encoded: String = transaction.query_row(
                "SELECT record FROM members WHERE object_id=?1 AND name=?2",
                params![object.as_str(), name],
                |row| row.get(0),
            )?;
            let mut record: super::FileRecord = serde_json::from_str(&encoded)?;
            if record.seal.as_ref() == Some(&seal) {
                return Ok(());
            }
            if !record.producer_open || record.seal.is_some() {
                return Err(Error::corrupt("producer seal was already finalized"));
            }
            let last: Option<u64> = transaction.query_row(
                "SELECT block_index FROM member_seals WHERE object_id=?1 AND name=?2 ORDER BY block_index DESC LIMIT 1",
                params![object.as_str(), name],
                |row| super::catalog::unsigned(row, 0),
            ).optional()?;
            let blocks = last.map_or(0, |last| last + 1);
            if blocks != seal.blocks {
                return Err(Error::corrupt("producer manifest has missing proof rows"));
            }
            record.seal = Some(seal);
            transaction.execute(
                "UPDATE members SET record=?3 WHERE object_id=?1 AND name=?2",
                params![object.as_str(), name, text(&record)?],
            )?;
            Ok(())
        })?;
        self.fault(
            super::faults::Point::MemberSealAfterCommit,
            operation.as_ref(),
        )
        .map_err(|error| error.committed(super::CommitState::Committed))
    }
}
