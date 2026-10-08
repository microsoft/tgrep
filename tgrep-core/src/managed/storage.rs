// Copyright (c) Microsoft Corporation. All rights reserved.

use super::{Error, ErrorCategory, Measurement, Result};
use crate::rooted::RootedDir;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    pub volume: u64,
    pub file: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ownership {
    pub format: u32,
    pub digest: [u8; 32],
}

pub(crate) const SECURITY_BYTES: usize = 64 * 1024;

impl Ownership {
    pub(crate) fn capture(file: &File) -> Result<Self> {
        Self::capture_with_link_state(file, false)
    }

    pub(crate) fn after_unlink(file: &File) -> Result<Self> {
        Self::capture_with_link_state(file, true)
    }

    fn capture_with_link_state(file: &File, unlinked: bool) -> Result<Self> {
        let metadata = file.metadata()?;
        #[cfg(unix)]
        if unlinked {
            use std::os::unix::fs::MetadataExt;
            if !metadata.is_file() || metadata.nlink() != 0 {
                return Err(Error::corrupt(
                    "unlinked verification handle has an unexpected type or links",
                ));
            }
        } else {
            plain(&metadata, false)?;
        }
        #[cfg(not(unix))]
        {
            let _ = unlinked;
            plain(&metadata, false)?;
        }
        let mut hash = blake3::Hasher::new();
        hash.update(b"tgrep/managed/member-ownership/v1\0");
        #[cfg(windows)]
        {
            use std::os::windows::{fs::MetadataExt, io::AsRawHandle};
            use windows_sys::Win32::Security::{
                DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetKernelObjectSecurity,
                OWNER_SECURITY_INFORMATION,
            };
            hash.update(b"windows\0");
            hash.update(&file.metadata()?.file_attributes().to_le_bytes());
            let mut bytes = [0_u8; SECURITY_BYTES];
            let mut needed = 0;
            // SAFETY: the pinned handle and bounded security descriptor buffer
            // remain valid. No SACL privilege or host security change is needed.
            if unsafe {
                GetKernelObjectSecurity(
                    file.as_raw_handle(),
                    OWNER_SECURITY_INFORMATION
                        | GROUP_SECURITY_INFORMATION
                        | DACL_SECURITY_INFORMATION,
                    bytes.as_mut_ptr().cast(),
                    bytes.len() as u32,
                    &mut needed,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let size = usize::try_from(needed)
                .ok()
                .filter(|size| *size <= bytes.len())
                .ok_or_else(|| Error::corrupt("native security descriptor exceeds its bound"))?;
            hash.update(&bytes[..size]);
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
            let metadata = file.metadata()?;
            hash.update(b"linux\0");
            hash.update(&metadata.uid().to_le_bytes());
            hash.update(&metadata.gid().to_le_bytes());
            hash.update(&metadata.mode().to_le_bytes());
            for name in [c"system.posix_acl_access", c"security.selinux"] {
                let mut bytes = [0_u8; SECURITY_BYTES];
                // SAFETY: a live descriptor, constant NUL-terminated name and
                // bounded native output buffer.
                let size = unsafe {
                    libc::fgetxattr(
                        file.as_raw_fd(),
                        name.as_ptr(),
                        bytes.as_mut_ptr().cast(),
                        bytes.len(),
                    )
                };
                hash.update(name.to_bytes_with_nul());
                if size < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ENODATA) {
                        hash.update(&0_u64.to_le_bytes());
                    } else {
                        return Err(error.into());
                    }
                } else {
                    let size =
                        usize::try_from(size).map_err(|_| Error::corrupt("invalid ACL size"))?;
                    hash.update(&(size as u64).to_le_bytes());
                    hash.update(&bytes[..size]);
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::{fd::AsRawFd, macos::fs::MetadataExt};
            unsafe extern "C" {
                fn acl_get_fd(fd: libc::c_int) -> *mut libc::c_void;
                fn acl_size(acl: *mut libc::c_void) -> libc::ssize_t;
                fn acl_copy_ext(
                    buffer: *mut libc::c_void,
                    acl: *mut libc::c_void,
                    size: libc::ssize_t,
                ) -> libc::ssize_t;
                fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
            }
            struct Acl(*mut libc::c_void);
            impl Drop for Acl {
                fn drop(&mut self) {
                    // SAFETY: this wrapper owns the ACL returned by acl_get_fd.
                    unsafe {
                        acl_free(self.0);
                    }
                }
            }
            let metadata = file.metadata()?;
            hash.update(b"macos\0");
            hash.update(&metadata.st_uid().to_le_bytes());
            hash.update(&metadata.st_gid().to_le_bytes());
            hash.update(&metadata.st_mode().to_le_bytes());
            hash.update(&metadata.st_flags().to_le_bytes());
            // SAFETY: a live pinned file descriptor.
            let acl = Acl(unsafe { acl_get_fd(file.as_raw_fd()) });
            if acl.0.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: a live native ACL owned by the wrapper.
            let size = unsafe { acl_size(acl.0) };
            if size < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let size =
                usize::try_from(size).map_err(|_| Error::corrupt("native ACL size overflow"))?;
            if size > SECURITY_BYTES {
                return Err(Error::incompatible(
                    "native ACL exceeds the supported ownership bound",
                ));
            }
            let mut bytes = [0_u8; SECURITY_BYTES];
            // SAFETY: the bounded output and owned ACL remain alive for the call.
            if unsafe { acl_copy_ext(bytes.as_mut_ptr().cast(), acl.0, size as libc::ssize_t) } < 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            hash.update(&bytes[..size]);
        }
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        return Err(Error::incompatible(
            "native ownership evidence is unsupported",
        ));
        Ok(Self {
            format: 1,
            digest: *hash.finalize().as_bytes(),
        })
    }
}

pub(crate) struct FileObservation {
    pub identity: FileIdentity,
    pub logical_bytes: u64,
    pub allocated_bytes: Measurement<u64>,
}

pub(crate) fn sqlite_file(name: &str) -> bool {
    matches!(
        name,
        "catalog.sqlite" | "catalog.sqlite-wal" | "catalog.sqlite-shm" | "catalog.sqlite-journal"
    )
}

pub(crate) fn file_change(file: &File) -> Result<[i64; 4]> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok([
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ])
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
        };
        let mut info: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: a live handle and a correctly sized native output buffer.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileBasicInfo,
                (&mut info as *mut FILE_BASIC_INFO).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok([info.LastWriteTime, info.ChangeTime, 0, 0])
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(Error::incompatible(
            "native file-change evidence is unsupported",
        ))
    }
}

impl FileIdentity {
    pub(crate) fn of(file: &File) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            Ok(Self {
                volume: metadata.dev(),
                file: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
            };
            let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: the file owns a live handle and the output is correctly sized.
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self {
                volume: u64::from(information.dwVolumeSerialNumber),
                file: (u64::from(information.nFileIndexHigh) << 32)
                    | u64::from(information.nFileIndexLow),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = file;
            Err(Error::incompatible(
                "native storage identity is unsupported",
            ))
        }
    }
}

pub(crate) struct Directory {
    path: PathBuf,
    root: RootedDir,
    handle: File,
    #[cfg(windows)]
    _ancestors: Vec<RootedDir>,
}

fn component(name: &str) -> Result<()> {
    let mut parts = Path::new(name).components();
    if name.is_empty()
        || name.contains(['/', '\\', ':', '\0'])
        || !matches!(parts.next(), Some(Component::Normal(_)))
        || parts.next().is_some()
    {
        return Err(Error::invalid("storage name must be one literal component"));
    }
    Ok(())
}

fn plain(metadata: &fs::Metadata, directory: bool) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(Error::corrupt("managed storage contains a reparse point"));
        }
    }
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(Error::corrupt(
            "managed storage entry has an unexpected type",
        ));
    }
    #[cfg(unix)]
    if !directory {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(Error::corrupt(
                "managed storage file has external hard links",
            ));
        }
    }
    Ok(())
}

impl Directory {
    pub(crate) fn open(path: &Path) -> Result<Arc<Self>> {
        plain(&fs::symlink_metadata(path)?, true)?;
        let path = fs::canonicalize(path)?;
        #[cfg(windows)]
        let ancestors = {
            let mut roots = Vec::new();
            for ancestor in path.ancestors().skip(1) {
                if ancestor.file_name().is_some() {
                    roots.push(RootedDir::open(ancestor)?);
                }
            }
            roots
        };
        let root = RootedDir::open(&path)?;
        let handle = root.directory_handle()?;
        let directory = Arc::new(Self {
            path,
            root,
            handle,
            #[cfg(windows)]
            _ancestors: ancestors,
        });
        directory.verify()?;
        Ok(directory)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn identity(&self) -> Result<FileIdentity> {
        FileIdentity::of(&self.handle)
    }

    pub(crate) fn directory_handle(&self) -> Result<File> {
        Ok(self.handle.try_clone()?)
    }

    pub(crate) fn verify(&self) -> Result<()> {
        self.root.verify_root()?;
        plain(&fs::symlink_metadata(&self.path)?, true)
    }

    pub(crate) fn child(&self, name: &str) -> Result<Arc<Self>> {
        component(name)?;
        self.verify()?;
        let file = self.open_native(name, false, false, true)?;
        let identity = FileIdentity::of(&file)?;
        let child = Self::open(&self.path.join(name))?;
        if child.identity()? != identity {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "storage-directory-replaced",
                "managed child identity changed during open",
            ));
        }
        self.verify()?;
        Ok(child)
    }

    pub(crate) fn create_child(&self, name: &str) -> Result<Arc<Self>> {
        component(name)?;
        self.verify()?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let name = std::ffi::CString::new(name).map_err(|_| Error::invalid("NUL in name"))?;
            // SAFETY: a pinned directory and one validated, NUL-terminated component.
            if unsafe { libc::mkdirat(self.handle.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(windows)]
        fs::create_dir(self.path.join(name))?;
        self.sync()?;
        self.child(name)
    }

    pub(crate) fn open_or_create_child(&self, name: &str) -> Result<Arc<Self>> {
        match self.create_child(name) {
            Ok(directory) => Ok(directory),
            Err(error)
                if error
                    .source_io_kind()
                    .is_some_and(|kind| kind == std::io::ErrorKind::AlreadyExists) =>
            {
                self.child(name)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn open_file(&self, name: &str, write: bool) -> Result<File> {
        if sqlite_file(name) {
            return Err(Error::invalid(
                "SQLite files require no-open observation; extra descriptor closes invalidate POSIX locks",
            ));
        }
        self.open_native(name, write, false, false)
    }

    pub(crate) fn open_verification_file(&self, name: &str) -> Result<File> {
        if sqlite_file(name) {
            return Err(Error::invalid(
                "catalog files cannot be collected as ordinary members",
            ));
        }
        self.open_native_options(name, true, false, false, true)
    }

    /// Closing any independently opened descriptor releases this process's POSIX
    /// locks on that inode, including SQLite's database and shared-memory locks.
    pub(crate) fn observe_file(&self, name: &str) -> Result<FileObservation> {
        component(name)?;
        self.verify()?;
        #[cfg(unix)]
        let observation = {
            use std::os::fd::AsRawFd;
            let name = std::ffi::CString::new(name).map_err(|_| Error::invalid("NUL in name"))?;
            let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: a pinned directory, one validated component and native output storage.
            if unsafe {
                libc::fstatat(
                    self.handle.as_raw_fd(),
                    name.as_ptr(),
                    &mut metadata,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            if metadata.st_mode & libc::S_IFMT != libc::S_IFREG || metadata.st_nlink != 1 {
                return Err(Error::corrupt(
                    "managed storage observation requires a regular file without external hard links",
                ));
            }
            #[allow(clippy::unnecessary_cast)]
            let identity = FileIdentity {
                volume: metadata.st_dev as u64,
                file: metadata.st_ino as u64,
            };
            FileObservation {
                identity,
                logical_bytes: metadata
                    .st_size
                    .try_into()
                    .map_err(|_| Error::corrupt("negative native file size"))?,
                allocated_bytes: Measurement::Observed {
                    value: u64::try_from(metadata.st_blocks)
                        .ok()
                        .and_then(|blocks| blocks.checked_mul(512))
                        .ok_or_else(|| Error::corrupt("native file allocation overflow"))?,
                },
            }
        };
        #[cfg(windows)]
        let observation = {
            let file = self.open_native(name, false, false, false)?;
            FileObservation {
                identity: FileIdentity::of(&file)?,
                logical_bytes: file.metadata()?.len(),
                allocated_bytes: allocated_bytes(&file)?,
            }
        };
        #[cfg(not(any(unix, windows)))]
        return Err(Error::incompatible(
            "native file observation is unsupported",
        ));
        self.verify()?;
        Ok(observation)
    }

    pub(crate) fn create_file(&self, name: &str) -> Result<File> {
        self.open_native(name, true, true, false)
    }

    fn open_native(&self, name: &str, write: bool, create: bool, directory: bool) -> Result<File> {
        self.open_native_options(name, write, create, directory, false)
    }

    fn open_native_options(
        &self,
        name: &str,
        write: bool,
        create: bool,
        directory: bool,
        protect_contents: bool,
    ) -> Result<File> {
        component(name)?;
        self.verify()?;
        #[cfg(unix)]
        let file = {
            let _ = protect_contents;
            use std::os::fd::{AsRawFd, FromRawFd};
            let name = std::ffi::CString::new(name).map_err(|_| Error::invalid("NUL in name"))?;
            let mut flags = libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
            flags |= if write { libc::O_RDWR } else { libc::O_RDONLY };
            if create {
                flags |= libc::O_CREAT | libc::O_EXCL;
            }
            if directory {
                flags |= libc::O_DIRECTORY;
            }
            // SAFETY: the directory is pinned and the name is one validated component.
            let fd = unsafe { libc::openat(self.handle.as_raw_fd(), name.as_ptr(), flags, 0o600) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: openat returned a fresh owned descriptor.
            unsafe { File::from_raw_fd(fd) }
        };
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
                FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
                FILE_SHARE_WRITE,
            };
            let mut options = File::options();
            options
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .share_mode(if protect_contents {
                    FILE_SHARE_READ
                } else {
                    FILE_SHARE_READ
                        | FILE_SHARE_WRITE
                        | if directory { 0 } else { FILE_SHARE_DELETE }
                });
            if directory {
                options.access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES);
            } else if protect_contents {
                use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
                options.access_mode(GENERIC_READ | GENERIC_WRITE | DELETE);
            } else {
                options.read(true).write(write).create_new(create);
            }
            let file = options.open(self.path.join(name))?;
            if crate::rooted::final_path_of(&file)?.parent()
                != Some(crate::rooted::final_path_of(&self.handle)?.as_path())
            {
                return Err(Error::corrupt("storage file escaped its pinned parent"));
            }
            file
        };
        #[cfg(not(any(unix, windows)))]
        return Err(Error::incompatible("managed storage is unsupported"));
        plain(&file.metadata()?, directory)?;
        #[cfg(windows)]
        if !directory {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
            };
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: the owned handle and native output buffer are valid.
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if info.nNumberOfLinks != 1 {
                return Err(Error::corrupt(
                    "managed storage file has external hard links",
                ));
            }
        }
        self.verify()?;
        Ok(file)
    }

    pub(crate) fn unlink_verified_file(
        &self,
        name: &str,
        expected: &FileIdentity,
        file: &File,
    ) -> Result<()> {
        component(name)?;
        self.verify()?;
        if FileIdentity::of(file)? != *expected || self.observe_file(name)?.identity != *expected {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "deletion-target-replaced",
                "the pinned and named deletion identities no longer agree",
            ));
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
            };
            let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            // SAFETY: this is the original read/write/delete verification handle,
            // with write/delete sharing excluded throughout authentication.
            if unsafe {
                SetFileInformationByHandle(
                    file.as_raw_handle(),
                    FileDispositionInfo,
                    (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                    std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(windows))]
        self.unlink(name, false)?;
        Ok(())
    }

    pub(crate) fn confirm_unlinked_file(&self, name: &str) -> Result<()> {
        match self.observe_file(name) {
            Err(error) if error.source_io_kind() == Some(std::io::ErrorKind::NotFound) => {
                self.sync()
            }
            Err(error) => Err(Error::new(
                ErrorCategory::Busy,
                "deletion-not-yet-observed",
                format!("physical removal is not confirmed: {error}"),
            )),
            Ok(_) => Err(Error::new(
                ErrorCategory::StaleIdentity,
                "deletion-name-still-present",
                "the deletion name remains or was recreated; reclaimed bytes are not confirmed",
            )),
        }
    }

    pub(crate) fn read_json<T: serde::de::DeserializeOwned>(
        &self,
        name: &str,
        limit: u64,
    ) -> Result<T> {
        Ok(serde_json::from_slice(&self.read_bytes(name, limit)?)?)
    }

    pub(crate) fn read_bytes(&self, name: &str, limit: u64) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.open_file(name, false)?
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(Error::corrupt("managed metadata exceeds its size limit"));
        }
        Ok(bytes)
    }

    pub(crate) fn create_json(&self, name: &str, value: &impl Serialize) -> Result<()> {
        let mut file = self.create_file(name)?;
        serde_json::to_writer(&mut file, value)?;
        file.flush()?;
        file.sync_all()?;
        self.sync()
    }

    pub(crate) fn publish_json(&self, name: &str, value: &impl Serialize) -> Result<FileIdentity> {
        component(name)?;
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > super::MAX_REQUEST_BYTES {
            return Err(Error::invalid("control file exceeds its size bound"));
        }
        let temporary = format!("{}.tmp", super::Id::new()?);
        let mut file = self.create_file(&temporary)?;
        let identity = FileIdentity::of(&file)?;
        let mut renamed = false;
        let result = (|| {
            file.write_all(&bytes)?;
            file.sync_all()?;
            self.verify()?;
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                let source = std::ffi::CString::new(temporary.as_str())
                    .map_err(|_| Error::invalid("NUL in control filename"))?;
                let destination = std::ffi::CString::new(name)
                    .map_err(|_| Error::invalid("NUL in control filename"))?;
                // SAFETY: both names are literal components in the same pinned directory.
                if unsafe {
                    libc::renameat(
                        self.handle.as_raw_fd(),
                        source.as_ptr(),
                        self.handle.as_raw_fd(),
                        destination.as_ptr(),
                    )
                } != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            #[cfg(windows)]
            fs::rename(self.path.join(&temporary), self.path.join(name))?;
            renamed = true;
            self.sync()
                .map_err(|error| error.committed(super::CommitState::Committed))?;
            Ok(identity.clone())
        })();
        drop(file);
        if result.is_err() && !renamed {
            self.remove_file(&temporary, &identity)?;
        }
        result
    }

    pub(crate) fn sync(&self) -> Result<()> {
        self.verify()?;
        #[cfg(unix)]
        self.handle.sync_all()?;
        // Windows file data is flushed separately. Directory handles do not
        // provide Unix fsync semantics; do not advertise power-loss persistence.
        Ok(())
    }

    pub(crate) fn remove_file(&self, name: &str, expected: &FileIdentity) -> Result<()> {
        #[cfg(windows)]
        return self.remove_by_handle(name, expected, false);
        #[cfg(not(windows))]
        {
            let file = self.open_file(name, false)?;
            if FileIdentity::of(&file)? != *expected {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "deletion-target-replaced",
                    "refusing to remove a different physical file",
                ));
            }
            self.unlink(name, false)?;
            self.sync()
        }
    }

    pub(crate) fn remove_child(&self, name: &str, expected: &FileIdentity) -> Result<()> {
        #[cfg(windows)]
        return self.remove_by_handle(name, expected, true);
        #[cfg(not(windows))]
        {
            let child = self.child(name)?;
            if child.identity()? != *expected {
                return Err(Error::new(
                    ErrorCategory::StaleIdentity,
                    "deletion-target-replaced",
                    "refusing to remove a different physical directory",
                ));
            }
            self.unlink(name, true)?;
            self.sync()
        }
    }

    #[cfg(windows)]
    fn remove_by_handle(&self, name: &str, expected: &FileIdentity, directory: bool) -> Result<()> {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            FileDispositionInfo, SetFileInformationByHandle,
        };
        component(name)?;
        self.verify()?;
        // Excluding FILE_SHARE_DELETE prevents a final-name replacement while
        // identity is checked and disposition is applied to this exact handle.
        let file = File::options()
            .access_mode(DELETE | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(self.path.join(name))?;
        plain(&file.metadata()?, directory)?;
        if FileIdentity::of(&file)? != *expected
            || crate::rooted::final_path_of(&file)?.parent()
                != Some(crate::rooted::final_path_of(&self.handle)?.as_path())
        {
            return Err(Error::new(
                ErrorCategory::StaleIdentity,
                "deletion-target-replaced",
                "refusing to remove a different physical entry",
            ));
        }
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: an owned DELETE handle, the matching information class, and a
        // correctly sized initialized structure which remains live for the call.
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        drop(file);
        match self.open_native(name, false, false, directory) {
            Err(error) if error.source_io_kind() == Some(std::io::ErrorKind::NotFound) => {
                self.sync()
            }
            Err(error) => Err(Error::new(
                ErrorCategory::Busy,
                "windows-deletion-not-yet-observed",
                error.to_string(),
            )),
            Ok(_) => Err(Error::new(
                ErrorCategory::StaleIdentity,
                "deletion-name-still-present",
                "a pending or replacement entry remains; reclaimed bytes are not confirmed",
            )),
        }
    }

    #[cfg(not(windows))]
    fn unlink(&self, name: &str, directory: bool) -> Result<()> {
        component(name)?;
        self.verify()?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let name = std::ffi::CString::new(name).map_err(|_| Error::invalid("NUL in name"))?;
            // SAFETY: the directory is pinned; unlinkat never follows the final component.
            if unsafe {
                libc::unlinkat(
                    self.handle.as_raw_fd(),
                    name.as_ptr(),
                    if directory { libc::AT_REMOVEDIR } else { 0 },
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(windows)]
        if directory {
            fs::remove_dir(self.path.join(name))?;
        } else {
            fs::remove_file(self.path.join(name))?;
        }
        Ok(())
    }
}

pub(crate) fn allocated_bytes(file: &File) -> Result<Measurement<u64>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let value = file
            .metadata()?
            .blocks()
            .checked_mul(512)
            .ok_or_else(|| Error::corrupt("allocated block count overflow"))?;
        Ok(Measurement::Observed { value })
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandleEx,
        };
        let mut information: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: a valid file handle, correct information class and output size.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                (&mut information as *mut FILE_STANDARD_INFO).cast(),
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let value = u64::try_from(information.AllocationSize)
            .map_err(|_| Error::corrupt("negative file allocation size"))?;
        Ok(Measurement::Observed { value })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Ok(Measurement::Unavailable {
            reason: "allocated-file-size-unsupported".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_open_observation_preserves_identity_size_and_allocation() {
        let temp = tempfile::tempdir().unwrap();
        let directory = Directory::open(temp.path()).unwrap();
        let mut file = directory.create_file("catalog.sqlite").unwrap();
        file.write_all(b"observed bytes").unwrap();
        let observation = directory.observe_file("catalog.sqlite").unwrap();
        assert_eq!(observation.identity, FileIdentity::of(&file).unwrap());
        assert_eq!(observation.logical_bytes, file.metadata().unwrap().len());
        assert_eq!(
            serde_json::to_value(observation.allocated_bytes).unwrap(),
            serde_json::to_value(allocated_bytes(&file).unwrap()).unwrap()
        );
        assert!(directory.observe_file("../outside").is_err());
        assert!(directory.observe_file(".").is_err());
        assert!(directory.open_file("catalog.sqlite", false).is_err());
        assert!(directory.open_file("catalog.sqlite-shm", false).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("catalog.sqlite", temp.path().join("symlink")).unwrap();
            assert!(directory.observe_file("symlink").is_err());
            fs::hard_link(
                temp.path().join("catalog.sqlite"),
                temp.path().join("hardlink"),
            )
            .unwrap();
            assert!(directory.observe_file("catalog.sqlite").is_err());
            assert!(directory.observe_file("hardlink").is_err());
        }
    }

    #[test]
    fn storage_only_removes_the_expected_owned_file() {
        let temp = tempfile::tempdir().unwrap();
        let directory = Directory::open(temp.path()).unwrap();
        let mut file = directory.create_file("owned").unwrap();
        file.write_all(b"owned bytes").unwrap();
        let identity = FileIdentity::of(&file).unwrap();
        assert!(directory.open_file("../outside", false).is_err());
        let wrong = FileIdentity {
            volume: identity.volume,
            file: identity.file ^ 1,
        };
        assert!(directory.remove_file("owned", &wrong).is_err());
        drop(file);
        directory.remove_file("owned", &identity).unwrap();
        assert!(!temp.path().join("owned").exists());
    }

    #[test]
    fn existing_child_is_not_silently_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let directory = Directory::open(temp.path()).unwrap();
        let child = directory.create_child("child").unwrap();
        assert_eq!(
            directory
                .open_or_create_child("child")
                .unwrap()
                .identity()
                .unwrap(),
            child.identity().unwrap()
        );
        directory.create_file("not-a-directory").unwrap();
        assert!(directory.open_or_create_child("not-a-directory").is_err());
    }
}
