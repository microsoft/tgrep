// Copyright (c) Microsoft Corporation. All rights reserved.

use super::storage::{Directory, file_change};
use super::{Error, ErrorCategory, Result};
use std::fs::File;

pub(super) const SETUP_MEMORY: u64 = 128 * 1024;

fn invalidated(detail: &str) -> Error {
    Error::new(
        ErrorCategory::StaleIdentity,
        "member-verification-invalidated",
        detail,
    )
}

/// Native change evidence complements producer hashes. In particular, a macOS
/// vnode observation is not an exclusion fence against uncooperative writers.
pub(super) struct NativeFile {
    pub(super) file: File,
    change: [i64; 4],
    #[cfg(target_os = "macos")]
    generation: u32,
    #[cfg(target_os = "macos")]
    events: VnodeEvents,
}

impl NativeFile {
    pub(super) fn open(directory: &Directory, name: &str) -> Result<Self> {
        let file = directory.open_verification_file(name)?;
        #[cfg(target_os = "linux")]
        acquire_lease(&file)?;
        #[cfg(target_os = "macos")]
        let events = VnodeEvents::new(&file)?;
        let result = Self {
            change: file_change(&file)?,
            #[cfg(target_os = "macos")]
            generation: content_generation(&file)?,
            #[cfg(target_os = "macos")]
            events,
            file,
        };
        result.check()?;
        Ok(result)
    }

    pub(super) fn check(&self) -> Result<()> {
        self.check_protection()?;
        if file_change(&self.file)? != self.change {
            return Err(invalidated(
                "native member changes invalidate all previously verified bytes",
            ));
        }
        #[cfg(target_os = "macos")]
        if content_generation(&self.file)? != self.generation || self.events.poll()? != 0 {
            return Err(invalidated(
                "native content generation or vnode events changed",
            ));
        }
        Ok(())
    }

    fn check_protection(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: F_GETLEASE reads the state of this live descriptor.
            let state = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_GETLEASE) };
            if state < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if state != libc::F_WRLCK {
                return Err(invalidated(
                    "the exclusive file lease was broken or revoked",
                ));
            }
        }
        Ok(())
    }

    /// Returns whether the previously verified prefix remains reusable after
    /// our own tail truncation. macOS always discards it instead of adopting the
    /// post-I/O generation as evidence for bytes verified before that I/O.
    pub(super) fn after_truncation(&mut self) -> Result<bool> {
        self.check_protection()?;
        #[cfg(target_os = "macos")]
        {
            self.after_macos_io(libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_ATTRIB)?;
            return Ok(false);
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.change = file_change(&self.file)?;
            Ok(true)
        }
    }

    pub(super) fn after_unlink(&mut self) -> Result<()> {
        self.check_protection()?;
        #[cfg(target_os = "macos")]
        self.after_macos_io(libc::NOTE_DELETE | libc::NOTE_LINK | libc::NOTE_ATTRIB)?;
        self.change = file_change(&self.file)?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn after_macos_io(&mut self, expected_events: u32) -> Result<()> {
        if self.events.poll()? & !expected_events != 0 {
            return Err(invalidated(
                "unexpected vnode mutation during destructive I/O",
            ));
        }
        self.generation = content_generation(&self.file)?;
        self.change = file_change(&self.file)?;
        self.check()
    }
}

#[cfg(target_os = "linux")]
fn acquire_lease(file: &File) -> Result<()> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct Owner {
        kind: libc::c_int,
        tid: libc::pid_t,
    }
    let fd = file.as_raw_fd();
    std::thread::scope(|scope| -> Result<()> {
        std::thread::Builder::new()
            .name("tgrep-lease-setup".into())
            .stack_size(SETUP_MEMORY as usize)
            .spawn_scoped(scope, move || -> Result<()> {
                let mut signals: libc::sigset_t = unsafe { std::mem::zeroed() };
                // SAFETY: thread-local signal configuration and a live file
                // descriptor. No process-wide signal handler is installed.
                unsafe {
                    if libc::sigemptyset(&mut signals) != 0
                        || libc::sigaddset(&mut signals, libc::SIGIO) != 0
                    {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    let code =
                        libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut());
                    if code != 0 {
                        return Err(std::io::Error::from_raw_os_error(code).into());
                    }
                    let owner = Owner {
                        kind: 0,
                        tid: libc::syscall(libc::SYS_gettid) as libc::pid_t,
                    };
                    if libc::fcntl(fd, libc::F_SETOWN_EX, &owner) < 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    if libc::fcntl(fd, libc::F_SETLEASE, libc::F_WRLCK) < 0 {
                        let error = std::io::Error::last_os_error();
                        return Err(match error.raw_os_error() {
                            Some(libc::EAGAIN) => Error::busy("member-writer-or-mapping-active"),
                            Some(libc::EINVAL | libc::ENOTSUP) => Error::incompatible(
                                "filesystem does not support managed content verification leases",
                            ),
                            _ => error.into(),
                        });
                    }
                    if libc::fcntl(fd, libc::F_SETOWN, 0) < 0 {
                        let error = std::io::Error::last_os_error();
                        if libc::fcntl(fd, libc::F_SETLEASE, libc::F_UNLCK) < 0 {
                            return Err(Error::new(
                                ErrorCategory::Io,
                                "member-lease-setup",
                                format!(
                                    "signal owner reset failed: {error}; lease release failed: {}",
                                    std::io::Error::last_os_error()
                                ),
                            ));
                        }
                        return Err(error.into());
                    }
                }
                Ok(())
            })?
            .join()
            .map_err(|_| Error::corrupt("native lease setup thread panicked"))?
    })
}

#[cfg(target_os = "macos")]
fn content_generation(file: &File) -> Result<u32> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct Attributes {
        count: u16,
        reserved: u16,
        common: u32,
        volume: u32,
        directory: u32,
        file: u32,
        fork: u32,
    }
    unsafe extern "C" {
        fn fgetattrlist(
            fd: libc::c_int,
            attributes: *mut Attributes,
            result: *mut libc::c_void,
            size: libc::size_t,
            options: libc::c_ulong,
        ) -> libc::c_int;
    }
    const GENERATION: u32 = 0x0008_0000;
    let mut attributes = Attributes {
        count: 5,
        reserved: 0,
        common: GENERATION | 0x8000_0000,
        volume: 0,
        directory: 0,
        file: 0,
        fork: 0,
    };
    let mut result = [0_u32; 7];
    // SAFETY: a live descriptor, native attrlist layout and bounded output.
    if unsafe {
        fgetattrlist(
            file.as_raw_fd(),
            &mut attributes,
            result.as_mut_ptr().cast(),
            std::mem::size_of_val(&result),
            0x20,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if result[0] as usize != std::mem::size_of_val(&result)
        || result[1] & GENERATION == 0
        || result[6] == 0
    {
        return Err(Error::incompatible(
            "nonzero native content generation is unavailable, including while writable-mapped",
        ));
    }
    Ok(result[6])
}

#[cfg(target_os = "macos")]
struct VnodeEvents(std::os::fd::OwnedFd);

#[cfg(target_os = "macos")]
impl VnodeEvents {
    fn new(file: &File) -> Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        // SAFETY: kqueue returns a fresh owned descriptor or -1.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: the fresh descriptor has one owner.
        let result = Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
        // SAFETY: descriptor configuration does not affect another process.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let event = libc::kevent {
            ident: file.as_raw_fd() as libc::uintptr_t,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE
                | libc::NOTE_EXTEND
                | libc::NOTE_ATTRIB
                | libc::NOTE_LINK
                | libc::NOTE_RENAME
                | libc::NOTE_DELETE
                | libc::NOTE_REVOKE,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: initialized event and live queue/file descriptors.
        if unsafe { libc::kevent(fd, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(result)
    }

    fn poll(&self) -> Result<u32> {
        use std::os::fd::AsRawFd;
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: live queue with one subscription, initialized bounded output,
        // and a zero timeout so inspection never waits for an event.
        let count = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                std::ptr::null(),
                0,
                &mut event,
                1,
                &timeout,
            )
        };
        if count < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if count == 0 {
            return Ok(0);
        }
        if event.flags & libc::EV_ERROR != 0 {
            return Err(std::io::Error::from_raw_os_error(event.data as i32).into());
        }
        Ok(event.fflags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[cfg(windows)]
    #[test]
    fn writable_mapping_blocks_verification_after_creator_handle_closes() {
        let temp = tempfile::tempdir().unwrap();
        let directory = Directory::open(temp.path()).unwrap();
        let mut file = directory.create_file("member").unwrap();
        file.write_all(&[b'x'; 8192]).unwrap();
        drop(file);
        drop(NativeFile::open(&directory, "member").unwrap());
        let mapping = {
            let creator = directory.open_file("member", true).unwrap();
            // SAFETY: this owned fixture deliberately exercises an external
            // writable section; no immutable Rust references alias its bytes.
            unsafe { memmap2::MmapOptions::new().map_mut(&creator).unwrap() }
        };
        assert!(NativeFile::open(&directory, "member").is_err());
        drop(mapping);
        let protected = NativeFile::open(&directory, "member").unwrap();
        protected.check().unwrap();
        drop((protected, directory));
        temp.close().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_lease_observes_nonblocking_writer_break_and_recovers_after_close() {
        let temp = tempfile::tempdir().unwrap();
        let directory = Directory::open(temp.path()).unwrap();
        let mut file = directory.create_file("member").unwrap();
        file.write_all(&[b'x'; 8192]).unwrap();
        drop(file);
        let mut protected = NativeFile::open(&directory, "member").unwrap();
        protected.file.set_len(4096).unwrap();
        protected.file.sync_all().unwrap();
        assert!(protected.after_truncation().unwrap());
        protected.check().unwrap();
        assert!(directory.open_file("member", true).is_err());
        assert_eq!(
            protected.check().unwrap_err().reason_code,
            "member-verification-invalidated"
        );
        drop(protected);
        drop(directory.open_file("member", true).unwrap());
        drop(directory);
        temp.close().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_native_evidence_brackets_pages_and_is_not_reused_after_own_io() {
        let temp = tempfile::tempdir().unwrap();
        let directory = Directory::open(temp.path()).unwrap();
        let mut file = directory.create_file("member").unwrap();
        file.write_all(&[b'x'; 8192]).unwrap();
        drop(file);
        let protected = NativeFile::open(&directory, "member").unwrap();
        protected.check().unwrap();
        let mut writer = directory.open_file("member", true).unwrap();
        writer.write_all(b"late prefix").unwrap();
        writer.sync_all().unwrap();
        drop(writer);
        assert_eq!(
            protected.check().unwrap_err().reason_code,
            "member-verification-invalidated"
        );
        drop(protected);
        let mut protected = NativeFile::open(&directory, "member").unwrap();
        protected.file.set_len(4096).unwrap();
        protected.file.sync_all().unwrap();
        assert!(!protected.after_truncation().unwrap());
        drop((protected, directory));
        temp.close().unwrap();
    }
}
