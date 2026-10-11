// Copyright (c) Microsoft Corporation. All rights reserved.

use super::{Error, Result, WorkPermit};
use ignore::gitignore::FileReadControl;
use std::collections::HashMap;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct BoundedRead<'a, R> {
    reader: R,
    remaining: u64,
    permit: Option<&'a Arc<WorkPermit>>,
}

impl<R: Read> Read for BoundedRead<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if let Some(permit) = self.permit {
            permit.check().map_err(std::io::Error::other)?;
        }
        let limit = self
            .remaining
            .saturating_add(1)
            .min(bytes.len() as u64)
            .min(64 * 1024) as usize;
        let read = self.reader.read(&mut bytes[..limit])?;
        if read as u64 > self.remaining {
            return Err(std::io::Error::other(Error::pressure(
                "managed-input-byte-limit",
            )));
        }
        self.remaining -= read as u64;
        if let Some(permit) = self.permit {
            permit.check().map_err(std::io::Error::other)?;
        }
        Ok(read)
    }
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(
    reader: impl Read,
    limit: u64,
    permit: Option<&Arc<WorkPermit>>,
) -> Result<T> {
    Ok(serde_json::from_reader(std::io::BufReader::new(
        BoundedRead {
            reader,
            remaining: limit,
            permit,
        },
    ))?)
}

pub(crate) fn read_bytes(
    reader: impl Read,
    limit: u64,
    permit: Option<&Arc<WorkPermit>>,
) -> Result<Vec<u8>> {
    let mut reader = BoundedRead {
        reader,
        remaining: limit,
        permit,
    };
    let mut bytes = Vec::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(bytes);
        }
        bytes
            .try_reserve(read)
            .map_err(|_| Error::pressure("managed-input-allocation"))?;
        bytes.extend_from_slice(&buffer[..read]);
    }
}

pub(crate) fn hash_bytes(data: &[u8], permit: Option<&Arc<WorkPermit>>) -> Result<[u8; 32]> {
    let Some(permit) = permit else {
        return Ok(*blake3::hash(data).as_bytes());
    };
    let mut hasher = blake3::Hasher::new();
    for chunk in data.chunks(64 * 1024) {
        permit.check()?;
        hasher.update(chunk);
    }
    permit.check()?;
    Ok(*hasher.finalize().as_bytes())
}

struct State {
    snapshots: HashMap<PathBuf, Option<Arc<[u8]>>>,
    memory: super::work::MemoryCharge,
}

pub(crate) struct InputControl {
    permit: Arc<WorkPermit>,
    max_file_bytes: u64,
    state: Mutex<State>,
    failure: Mutex<Option<Error>>,
}

impl std::fmt::Debug for InputControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputControl")
            .field("max_file_bytes", &self.max_file_bytes)
            .finish_non_exhaustive()
    }
}

impl InputControl {
    pub(crate) fn new(permit: &Arc<WorkPermit>) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            permit: Arc::clone(permit),
            max_file_bytes: permit.namespace.policy()?.policy.work.blob_bytes,
            state: Mutex::new(State {
                snapshots: HashMap::new(),
                memory: permit.memory(64 * 1024)?,
            }),
            failure: Mutex::new(None),
        }))
    }

    fn record(&self, error: Error) {
        self.permit.cancel();
        match self.failure.lock() {
            Ok(mut failure) => {
                if failure.is_none() {
                    *failure = Some(error);
                }
            }
            Err(_) => eprintln!("managed input failure could not be recorded: {error}"),
        }
    }

    pub(crate) fn finish<T>(&self, result: Result<T>) -> Result<T> {
        match self
            .failure
            .lock()
            .map_err(|_| Error::corrupt("input failure lock poisoned"))?
            .take()
        {
            Some(error) => Err(error),
            None => result,
        }
    }

    fn snapshot(&self, path: &Path) -> Result<Option<Arc<[u8]>>> {
        self.permit.check()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::corrupt("input snapshot lock poisoned"))?;
        if let Some(snapshot) = state.snapshots.get(path) {
            return Ok(snapshot.clone());
        }
        state.memory.grow(
            (path.as_os_str().as_encoded_bytes().len() as u64)
                .checked_mul(4)
                .and_then(|bytes| bytes.checked_add(256))
                .ok_or_else(|| Error::pressure("input-path-account-overflow"))?,
        )?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                state.snapshots.insert(path.to_path_buf(), None);
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let before = file.metadata()?;
        if !before.is_file() {
            return Err(Error::invalid(format!(
                "ignore/config input is not a regular file: {}",
                path.display()
            )));
        }
        let mut bytes = Vec::new();
        let mut chunk = [0; 32 * 1024];
        loop {
            self.permit.check()?;
            let read = file.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            if (bytes.len() as u64)
                .checked_add(read as u64)
                .is_none_or(|bytes| bytes > self.max_file_bytes)
            {
                return Err(Error::pressure("ignore-input-byte-limit"));
            }
            state.memory.grow(
                (read as u64)
                    .checked_mul(4)
                    .ok_or_else(|| Error::pressure("ignore-input-account-overflow"))?,
            )?;
            bytes
                .try_reserve(read)
                .map_err(|_| Error::pressure("ignore-input-allocation"))?;
            bytes.extend_from_slice(&chunk[..read]);
        }
        if crate::meta::file_version(&before) != crate::meta::file_version(&file.metadata()?) {
            return Err(Error::busy("ignore-input-changed-during-read"));
        }
        let bytes: Arc<[u8]> = bytes.into();
        state
            .snapshots
            .insert(path.to_path_buf(), Some(Arc::clone(&bytes)));
        Ok(Some(bytes))
    }
}

impl FileReadControl for InputControl {
    fn check_pattern(&self, source: Option<&Path>, pattern: &str) -> std::io::Result<()> {
        let result = (|| {
            self.permit.check()?;
            let path_bytes =
                source.map_or(0, |path| path.as_os_str().as_encoded_bytes().len() as u64);
            let bytes = (pattern.len() as u64)
                .checked_mul(64)
                .and_then(|bytes| {
                    path_bytes
                        .checked_mul(4)
                        .and_then(|path| bytes.checked_add(path))
                })
                .and_then(|bytes| bytes.checked_add(2048))
                .ok_or_else(|| Error::pressure("ignore-pattern-account-overflow"))?;
            self.state
                .lock()
                .map_err(|_| Error::corrupt("input snapshot lock poisoned"))?
                .memory
                .grow(bytes)
        })();
        result.map_err(|error: Error| {
            let message = error.to_string();
            self.record(error);
            std::io::Error::other(message)
        })
    }

    fn read_file(&self, path: &Path) -> std::io::Result<Arc<[u8]>> {
        match self.snapshot(path) {
            Ok(Some(bytes)) => Ok(bytes),
            Ok(None) => Err(std::io::Error::new(
                ErrorKind::NotFound,
                format!("optional ignore input is absent: {}", path.display()),
            )),
            Err(error) => {
                let message = error.to_string();
                self.record(error);
                Err(std::io::Error::other(message))
            }
        }
    }

    fn check(&self) -> std::io::Result<()> {
        self.permit.check().map_err(|error| {
            if error.category == super::ErrorCategory::Cancelled {
                return std::io::Error::other(error);
            }
            let message = error.to_string();
            self.record(error);
            std::io::Error::other(message)
        })
    }

    fn report_error(&self, error: &ignore::Error) {
        if error
            .io_error()
            .is_some_and(|error| error.kind() == ErrorKind::NotFound)
        {
            return;
        }
        if self.permit.check().is_err() {
            return;
        }
        self.record(Error::invalid(format!("invalid ignore input: {error}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed::{
        ErrorCategory, Namespace, OperationToken, OwnerGuard, Token, WorkRequest,
    };

    fn fixture(
        private_bytes: u64,
    ) -> (
        tempfile::TempDir,
        Arc<Namespace>,
        OwnerGuard,
        Arc<WorkPermit>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let namespace = Namespace::initialize_identity(
            &"a".repeat(64),
            temp.path(),
            super::super::policy::fixture_policy(),
        )
        .unwrap();
        let owner = OwnerGuard::claim(namespace.prepare_owner().unwrap().claim).unwrap();
        namespace.register_owner(owner.registration()).unwrap();
        let operation = namespace
            .accept_operation(
                OperationToken {
                    scope: owner.registration().owner.clone(),
                    sequence: 1,
                    token: Token::parse("input-test").unwrap(),
                },
                "inputs",
                serde_json::json!({}),
            )
            .unwrap();
        let permit = namespace
            .reserve(
                &operation.id,
                Some(&owner.registration().owner),
                WorkRequest {
                    allocation_version: 1,
                    staging_bytes: 1024 * 1024,
                    private_bytes,
                    slots: 1,
                },
            )
            .unwrap();
        (temp, namespace, owner, permit)
    }

    #[test]
    fn one_pass_freezes_present_and_absent_ignore_sources() {
        let (temp, _namespace, _owner, permit) = fixture(1024 * 1024);
        let control = InputControl::new(&permit).unwrap();
        let present = temp.path().join("present.ignore");
        let absent = temp.path().join("absent.ignore");
        std::fs::write(&present, "original\n").unwrap();
        assert_eq!(&*control.read_file(&present).unwrap(), b"original\n");
        assert_eq!(
            control.read_file(&absent).unwrap_err().kind(),
            ErrorKind::NotFound
        );
        std::fs::write(&present, "replacement\n").unwrap();
        std::fs::write(&absent, "created during walk\n").unwrap();
        assert_eq!(&*control.read_file(&present).unwrap(), b"original\n");
        assert_eq!(
            control.read_file(&absent).unwrap_err().kind(),
            ErrorKind::NotFound
        );
        control.finish(Ok(())).unwrap();
    }

    #[test]
    fn pattern_admission_is_bounded_and_preserves_resource_errors() {
        let (temp, _namespace, _owner, permit) = fixture(96 * 1024);
        let control = InputControl::new(&permit).unwrap();
        let path = temp.path().join("many.ignore");
        std::fs::write(&path, "*.x\n".repeat(1000)).unwrap();
        let mut builder = ignore::gitignore::GitignoreBuilder::new(temp.path());
        builder.file_read_control(Some(control.clone()));
        assert!(builder.add(&path).is_some());
        assert!(builder.build().is_err());
        let error = control.finish(Ok(())).unwrap_err();
        assert_eq!(error.category, ErrorCategory::ResourcePressure);
        assert_eq!(error.reason_code, "reserved-private-memory-exhausted");
        assert!(permit.peak_private_bytes() <= permit.private_limit());
        let mut ordinary = ignore::gitignore::GitignoreBuilder::new(temp.path());
        assert!(ordinary.add(&path).is_none());
        assert!(ordinary.build().is_ok());
    }

    #[test]
    fn metadata_reader_preserves_limit_and_cancellation_categories() {
        let (_temp, _namespace, _owner, permit) = fixture(1024 * 1024);
        let error = read_json::<serde_json::Value>(&b"{\"value\":\"large\"}"[..], 8, Some(&permit))
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::ResourcePressure);
        assert_eq!(error.reason_code, "managed-input-byte-limit");
        assert_eq!(
            read_json::<serde_json::Value>(&b"{}"[..], 2, Some(&permit)).unwrap(),
            serde_json::json!({})
        );
        permit.cancel();
        let error = read_json::<serde_json::Value>(&b"{}"[..], 2, Some(&permit)).unwrap_err();
        assert_eq!(error.category, ErrorCategory::Cancelled);
        assert_eq!(error.reason_code, "operation-cancelled");
    }

    #[test]
    fn bounded_reader_checks_cancellation_after_each_input_chunk() {
        struct CancelOnRead<'a>(&'a Arc<WorkPermit>);
        impl Read for CancelOnRead<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                assert!(bytes.len() <= 64 * 1024);
                bytes.fill(b'x');
                self.0.cancel();
                Ok(bytes.len())
            }
        }
        let (_temp, _namespace, _owner, permit) = fixture(1024 * 1024);
        let error = read_bytes(CancelOnRead(&permit), 1024 * 1024, Some(&permit)).unwrap_err();
        assert_eq!(error.category, ErrorCategory::Cancelled);
    }

    #[cfg(unix)]
    #[test]
    fn nonregular_ignore_inputs_cannot_block_on_fifo_open() {
        use std::os::unix::ffi::OsStrExt;
        let (temp, _namespace, _owner, permit) = fixture(1024 * 1024);
        let control = InputControl::new(&permit).unwrap();
        let path = temp.path().join("pipe.ignore");
        let native = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the terminated pathname remains valid throughout this call.
        assert_eq!(unsafe { libc::mkfifo(native.as_ptr(), 0o600) }, 0);
        assert!(control.read_file(&path).is_err());
        assert_eq!(
            control.finish(Ok(())).unwrap_err().category,
            ErrorCategory::InvalidInput
        );
    }
}
