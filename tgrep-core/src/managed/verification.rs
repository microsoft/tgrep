// Copyright (c) Microsoft Corporation. All rights reserved.

use super::authentication::{
    BLOCK_BYTES, Block, IO_BYTES, MemberSeal, hash_block, manifest_hasher,
};
use super::authentication_native::NativeFile;
use super::storage::Directory;
use super::{
    CollectionProgress, CollectionRequest, Error, ErrorCategory, FileIdentity, FileRecord, Id,
    Namespace, Ownership, Result, WorkPermit,
};
use rusqlite::params;
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

const PAGE_BLOCKS: usize = IO_BYTES / BLOCK_BYTES;
pub(super) const MEMORY_BYTES: u64 = super::authentication_native::SETUP_MEMORY
    + super::storage::SECURITY_BYTES as u64
    + BLOCK_BYTES as u64
    + 16 * 1024;

pub(super) struct Active<'a> {
    pub(super) context: MutexGuard<'a, Option<Verification>>,
    pub(super) keep: bool,
}

impl Drop for Active<'_> {
    fn drop(&mut self) {
        if !self.keep {
            self.context.take();
        }
    }
}

pub(super) struct Pass<'a> {
    pub(super) request: &'a CollectionRequest,
    pub(super) progress: &'a mut CollectionProgress,
    pub(super) permit: &'a Arc<WorkPermit>,
    pub(super) deadline: Instant,
    pub(super) context: &'a mut Option<Verification>,
}

impl Pass<'_> {
    pub(super) fn recheck(&self, namespace: &Namespace) -> Result<()> {
        self.permit.check()?;
        let policy = namespace.policy()?.version;
        let allocation = namespace.allocation()?.version;
        if policy != self.request.policy_version {
            return Err(Error::stale_version(policy));
        }
        if allocation != self.request.allocation_version {
            return Err(Error::stale_version(allocation));
        }
        Ok(())
    }
}

pub(super) struct Verification {
    pub(super) native: NativeFile,
    object: Id,
    name: String,
    identity: FileIdentity,
    ownership: Ownership,
    seal: MemberSeal,
    pub(super) current_length: u64,
    policy_version: u64,
    allocation_version: u64,
    expires: Instant,
    next_block: u64,
    partial: usize,
    partial_hash: blake3::Hasher,
    manifest: blake3::Hasher,
    pending: VecDeque<Block>,
    buffer: Box<[u8; BLOCK_BYTES]>,
    complete: bool,
    _memory: super::memory::RetainedMemory,
}

fn proof_error(detail: &str) -> Error {
    Error::new(
        ErrorCategory::CorruptMetadata,
        "member-content-proof-invalid",
        detail,
    )
}

fn modified(detail: &str) -> Error {
    Error::new(ErrorCategory::StaleIdentity, "member-modified", detail)
}

impl Verification {
    pub(super) fn new(
        namespace: &Namespace,
        directory: &Directory,
        object: &Id,
        member: &FileRecord,
        pass: &Pass<'_>,
    ) -> Result<Self> {
        let seal = member
            .seal
            .clone()
            .ok_or_else(|| proof_error("complete producer proof is absent"))?;
        seal.validate()?;
        if member.producer_open
            || member.removed
            || member
                .logical_bytes
                .checked_add(member.credited_logical_bytes)
                != Some(seal.length)
        {
            return Err(proof_error(
                "member inventory is not a sealed retained prefix",
            ));
        }
        let memory = pass.permit.memory(MEMORY_BYTES)?.retain(0)?;
        let native = NativeFile::open(directory, &member.name)?;
        let current_length = native.file.metadata()?.len();
        let authorized_pending = member
            .pending_length
            .filter(|_| member.pending_manifest == Some(seal.manifest));
        if current_length != member.logical_bytes
            && !authorized_pending.is_some_and(|target| target != 0 && target == current_length)
        {
            return Err(modified(
                "member length differs from its sealed inventory and authenticated pending target",
            ));
        }
        if current_length > seal.length
            || (current_length != seal.length && !current_length.is_multiple_of(BLOCK_BYTES as u64))
        {
            return Err(proof_error(
                "retained length is not an original authentication block boundary",
            ));
        }
        let result = Self {
            native,
            object: object.clone(),
            name: member.name.clone(),
            identity: member.identity.clone(),
            ownership: member.ownership.clone(),
            seal,
            current_length,
            policy_version: pass.request.policy_version,
            allocation_version: pass.request.allocation_version,
            expires: Self::expiration(namespace)?,
            next_block: 0,
            partial: 0,
            partial_hash: blake3::Hasher::new(),
            manifest: manifest_hasher(),
            pending: VecDeque::with_capacity(PAGE_BLOCKS),
            buffer: Box::new([0; BLOCK_BYTES]),
            complete: false,
            _memory: memory,
        };
        result.check(directory)?;
        Ok(result)
    }

    fn expiration(namespace: &Namespace) -> Result<Instant> {
        Instant::now()
            .checked_add(Duration::from_millis(
                namespace.policy()?.policy.work.cursor_lifetime_ms,
            ))
            .ok_or_else(|| Error::invalid("verification expiry overflow"))
    }

    pub(super) fn matches(
        &self,
        object: &Id,
        member: &FileRecord,
        request: &CollectionRequest,
    ) -> bool {
        self.object == *object
            && self.name == member.name
            && self.identity == member.identity
            && self.ownership == member.ownership
            && member.seal.as_ref() == Some(&self.seal)
            && self.policy_version == request.policy_version
            && self.allocation_version == request.allocation_version
            && self.expires > Instant::now()
            && (self.current_length == member.logical_bytes
                || member.pending_length == Some(self.current_length))
    }

    pub(super) fn check(&self, directory: &Directory) -> Result<()> {
        self.native.check()?;
        if FileIdentity::of(&self.native.file)? != self.identity
            || self.native.file.metadata()?.len() != self.current_length
            || Ownership::capture(&self.native.file)? != self.ownership
            || directory.observe_file(&self.name)?.identity != self.identity
        {
            return Err(modified(
                "native identity, length, ownership or directory entry changed during verification",
            ));
        }
        self.native.check()
    }

    fn refill(&mut self, namespace: &Namespace) -> Result<()> {
        if !self.pending.is_empty() {
            return Ok(());
        }
        namespace.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT block_index,length,substr(digest,1,33) FROM member_seals
                 WHERE object_id=?1 AND name=?2 AND block_index>=?3 ORDER BY block_index LIMIT ?4",
            )?;
            let mut rows = statement.query(params![
                self.object.as_str(),
                self.name,
                super::catalog::sql_integer(self.next_block)?,
                PAGE_BLOCKS as u32
            ])?;
            while let Some(row) = rows.next()? {
                let digest: Vec<u8> = row.get(2)?;
                self.pending.push_back(Block {
                    index: super::catalog::unsigned(row, 0)?,
                    length: row.get(1)?,
                    digest: digest.try_into().map_err(|_| {
                        proof_error("digest length differs from the authentication format")
                    })?,
                });
            }
            Ok(())
        })?;
        if self.pending.is_empty() {
            return Err(proof_error("a required producer proof row is missing"));
        }
        Ok(())
    }

    pub(super) fn page(
        &mut self,
        namespace: &Namespace,
        directory: &Directory,
        pass: &mut Pass<'_>,
    ) -> Result<bool> {
        pass.recheck(namespace)?;
        self.check(directory)?;
        if self.complete {
            return Ok(true);
        }
        let mut read = 0_u64;
        let mut blocks = 0;
        let allowance = pass
            .request
            .bounds
            .max_delete_bytes
            .saturating_sub(pass.progress.verification_bytes)
            .min(IO_BYTES as u64);
        pass.progress.verification_pages += 1;
        while self.next_block < self.seal.blocks && blocks < PAGE_BLOCKS {
            if Instant::now() >= pass.deadline {
                break;
            }
            self.refill(namespace)?;
            let block = self
                .pending
                .front()
                .ok_or_else(|| proof_error("proof page is empty"))?;
            let start = self
                .next_block
                .checked_mul(BLOCK_BYTES as u64)
                .ok_or_else(|| proof_error("proof block offset overflow"))?;
            let expected = self
                .seal
                .length
                .saturating_sub(start)
                .min(BLOCK_BYTES as u64);
            if block.index != self.next_block
                || u64::from(block.length) != expected
                || expected == 0
            {
                return Err(proof_error(
                    "producer proof is missing, reordered or has a different block length",
                ));
            }
            if start < self.current_length {
                if allowance == read {
                    break;
                }
                let count =
                    ((block.length as usize - self.partial) as u64).min(allowance - read) as usize;
                self.native
                    .file
                    .seek(SeekFrom::Start(start + self.partial as u64))?;
                self.native.file.read_exact(&mut self.buffer[..count])?;
                self.partial_hash.update(&self.buffer[..count]);
                self.partial += count;
                read += count as u64;
                pass.progress.verification_bytes += count as u64;
                if self.partial != block.length as usize {
                    break;
                }
                if self.partial_hash.finalize().as_bytes() != &block.digest {
                    return Err(modified(
                        "current payload differs from producer-intended content",
                    ));
                }
            }
            hash_block(&mut self.manifest, block);
            self.partial_hash.reset();
            self.partial = 0;
            self.pending.pop_front();
            self.next_block += 1;
            blocks += 1;
            pass.progress.proof_rows_verified += 1;
        }
        namespace.fault(
            super::faults::Point::MemberVerificationPage,
            Some(pass.permit.operation_id()),
        )?;
        self.check(directory)?;
        pass.recheck(namespace)?;
        self.expires = Self::expiration(namespace)?;
        if self.next_block == self.seal.blocks {
            let extra: bool = namespace.read(|connection| Ok(connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM member_seals WHERE object_id=?1 AND name=?2 AND block_index>=?3)",
                params![self.object.as_str(), self.name, super::catalog::sql_integer(self.seal.blocks)?],
                |row| row.get(0),
            )?))?;
            if extra || self.manifest.finalize().as_bytes() != &self.seal.manifest {
                return Err(proof_error(
                    "complete producer manifest differs from the immutable proof rows",
                ));
            }
            self.complete = true;
            namespace.fault(
                super::faults::Point::MemberVerified,
                Some(pass.permit.operation_id()),
            )?;
            self.check(directory)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) fn after_truncation(&mut self, directory: &Directory, length: u64) -> Result<bool> {
        if self.native.file.metadata()?.len() != length
            || FileIdentity::of(&self.native.file)? != self.identity
            || Ownership::capture(&self.native.file)? != self.ownership
            || directory.observe_file(&self.name)?.identity != self.identity
        {
            return Err(modified(
                "native member identity, ownership or length changed during destructive I/O",
            ));
        }
        let retained = self.native.after_truncation()?;
        self.current_length = length;
        if !retained {
            self.complete = false;
            self.next_block = 0;
            self.partial = 0;
            self.partial_hash.reset();
            self.manifest = manifest_hasher();
            self.pending.clear();
        }
        self.check(directory)?;
        Ok(retained)
    }
}

impl Namespace {
    pub(super) fn verification_scratch(&self, bytes: u64) -> Result<super::memory::RetainedMemory> {
        self.read(|connection| {
            let encoded: String =
                connection.query_row("SELECT policy FROM state WHERE singleton=1", [], |row| {
                    row.get(0)
                })?;
            let policy: super::Policy = serde_json::from_str(&encoded)?;
            let allocation = super::work::allocation_row(connection)?;
            self.memory.retain_unreserved(
                connection,
                bytes,
                0,
                policy
                    .work
                    .private_work_bytes
                    .min(allocation.private_work_bytes),
            )
        })
    }

    pub(super) fn active_verification(&self) -> Result<Active<'_>> {
        let context = self.verification.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => Error::busy("member-verifier-active"),
            TryLockError::Poisoned(_) => Error::corrupt("member verification cache poisoned"),
        })?;
        Ok(Active {
            context,
            keep: false,
        })
    }

    pub(super) fn discard_idle_verification(&self, expired_only: bool) -> Result<bool> {
        let mut cache = match self.verification.try_lock() {
            Ok(cache) => cache,
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Poisoned(_)) => {
                return Err(Error::corrupt("member verification cache poisoned"));
            }
        };
        if !expired_only
            || cache
                .as_ref()
                .is_some_and(|context| context.expires <= Instant::now())
        {
            cache.take();
        }
        Ok(true)
    }

    pub(super) fn verification_diagnostics(&self) -> Result<serde_json::Value> {
        self.discard_idle_verification(true)?;
        let cache = match self.verification.try_lock() {
            Ok(cache) => cache,
            Err(TryLockError::WouldBlock) => {
                return Ok(serde_json::json!({
                    "state": "active",
                    "retained_contexts": super::Measurement::<u32>::Unavailable {
                        reason: "active-verification-owner".into(),
                    },
                }));
            }
            Err(TryLockError::Poisoned(_)) => {
                return Err(Error::corrupt("member verification cache poisoned"));
            }
        };
        Ok(match cache.as_ref() {
            Some(context) => serde_json::json!({
                "state": "cached",
                "retained_contexts": super::Measurement::Observed { value: 1_u32 },
                "retained_capacity_bytes": MEMORY_BYTES,
                "object": context.object,
                "member": context.name,
                "current_length": context.current_length,
                "verified_prefix_bytes": (context.next_block * BLOCK_BYTES as u64)
                    .min(context.current_length) + context.partial as u64,
                "proof_rows_verified": context.next_block,
                "complete": context.complete,
                "idle_expiry_remaining_ms": context.expires.saturating_duration_since(Instant::now()).as_millis(),
            }),
            None => serde_json::json!({
                "state": "empty",
                "retained_contexts": super::Measurement::Observed { value: 0_u32 },
                "retained_capacity_bytes": 0,
            }),
        })
    }
}
