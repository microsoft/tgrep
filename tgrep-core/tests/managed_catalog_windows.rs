// Copyright (c) Microsoft Corporation. All rights reserved.

#![cfg(windows)]

// A separate test process isolates the temporary VFS syscall replacement from
// other catalog tests. SQLite 3.53.2 confused canonical DOS paths with UNC paths
// and could leak shared OS locks after all read transactions and readers ended.

use rusqlite::{Connection, ffi};
use std::fs::File;
use std::mem::ManuallyDrop;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::Duration;
use windows_sys::Win32::Foundation::{GetLastError, HANDLE, SetLastError};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx, LOCKFILE_EXCLUSIVE_LOCK, LockFileEx,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

fn identity(file: &File) -> (u64, [u8; 16]) {
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: the owned or synchronously borrowed handle and native output
    // structure remain valid for the entire metadata query.
    assert_ne!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error(),
    );
    (info.VolumeSerialNumber, info.FileId.Identifier)
}

struct Rendezvous {
    arrived: Mutex<u32>,
    ready: Condvar,
}

impl Rendezvous {
    fn new() -> Self {
        Self {
            arrived: Mutex::new(0),
            ready: Condvar::new(),
        }
    }

    fn wait(&self) {
        let mut arrived = self.arrived.lock().unwrap();
        *arrived += 1;
        self.ready.notify_all();
        drop(
            self.ready
                .wait_timeout_while(arrived, Duration::from_millis(250), |arrived| *arrived < 2)
                .unwrap(),
        );
    }
}

struct LockProbe {
    identity: (u64, [u8; 16]),
    calls: AtomicU32,
    before: Rendezvous,
    after: Rendezvous,
}

fn installed() -> &'static Mutex<Option<Arc<LockProbe>>> {
    static PROBE: OnceLock<Mutex<Option<Arc<LockProbe>>>> = OnceLock::new();
    PROBE.get_or_init(|| Mutex::new(None))
}

unsafe extern "system" fn lock(
    handle: HANDLE,
    flags: u32,
    reserved: u32,
    low: u32,
    high: u32,
    overlapped: *mut OVERLAPPED,
) -> i32 {
    let probe = installed().lock().unwrap().clone();
    let selected = probe.as_ref().filter(|probe| {
        if flags & LOCKFILE_EXCLUSIVE_LOCK != 0 || low != 1 || high != 0 {
            return false;
        }
        // SAFETY: the VFS supplies a live OVERLAPPED for the native call.
        let offset = unsafe { (*overlapped).Anonymous.Anonymous.Offset };
        // WAL_READ_LOCK(1) is WALINDEX_LOCK_OFFSET + 4.
        if offset != 124 {
            return false;
        }
        // SAFETY: borrow the VFS's live handle for identity only, never close it.
        let file = ManuallyDrop::new(unsafe { File::from_raw_handle(handle) });
        identity(&file) == probe.identity && probe.calls.fetch_add(1, Ordering::SeqCst) < 2
    });
    if let Some(probe) = selected {
        probe.before.wait();
    }
    // SAFETY: all parameters are forwarded unchanged to the original OS API.
    let result = unsafe { LockFileEx(handle, flags, reserved, low, high, overlapped) };
    if let Some(probe) = selected {
        // Preserve the native error across the diagnostic synchronization.
        let error = unsafe { GetLastError() };
        probe.after.wait();
        unsafe { SetLastError(error) };
    }
    result
}

struct Installed {
    vfs: *mut ffi::sqlite3_vfs,
    original: ffi::sqlite3_syscall_ptr,
}

impl Installed {
    fn new(probe: Arc<LockProbe>) -> Self {
        // SAFETY: the default Windows VFS is initialized and remains alive for
        // this process. Installation precedes every concurrent SQLite call.
        unsafe {
            let vfs = ffi::sqlite3_vfs_find(std::ptr::null());
            assert!(!vfs.is_null());
            assert!((*vfs).iVersion >= 3);
            let original = (*vfs).xGetSystemCall.unwrap()(vfs, c"LockFileEx".as_ptr());
            assert!(original.is_some());
            let replacement = std::mem::transmute::<
                unsafe extern "system" fn(HANDLE, u32, u32, u32, u32, *mut OVERLAPPED) -> i32,
                unsafe extern "C" fn(),
            >(lock);
            *installed().lock().unwrap() = Some(probe);
            assert_eq!(
                (*vfs).xSetSystemCall.unwrap()(vfs, c"LockFileEx".as_ptr(), Some(replacement)),
                ffi::SQLITE_OK,
            );
            Self { vfs, original }
        }
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        // SAFETY: all reader threads have joined before the original, correctly
        // typed syscall pointer is restored on the still-live VFS.
        unsafe {
            assert_eq!(
                (*self.vfs).xSetSystemCall.unwrap()(
                    self.vfs,
                    c"LockFileEx".as_ptr(),
                    self.original,
                ),
                ffi::SQLITE_OK,
            );
        }
        *installed().lock().unwrap() = None;
    }
}

#[test]
fn canonical_windows_catalog_readers_release_native_wal_locks() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap().join("catalog.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection.busy_timeout(Duration::ZERO).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE probe(value); INSERT INTO probe VALUES(1)",
        )
        .unwrap();
    let identity = identity(&File::open(path.with_file_name("catalog.sqlite-shm")).unwrap());
    let readers = [
        Connection::open(&path).unwrap(),
        Connection::open(&path).unwrap(),
    ];
    let probe = Arc::new(LockProbe {
        identity,
        calls: AtomicU32::new(0),
        before: Rendezvous::new(),
        after: Rendezvous::new(),
    });
    let hook = Installed::new(probe.clone());
    std::thread::scope(|scope| {
        let (ready, received) = mpsc::channel();
        let mut release = Vec::new();
        let mut tasks = Vec::new();
        for reader in readers {
            let (sender, receiver) = mpsc::channel();
            release.push(sender);
            let ready = ready.clone();
            tasks.push(scope.spawn(move || {
                reader
                    .execute_batch("BEGIN; SELECT value FROM probe")
                    .unwrap();
                ready.send(()).unwrap();
                receiver.recv_timeout(Duration::from_secs(5)).unwrap();
                reader.execute_batch("ROLLBACK").unwrap();
                // SAFETY: the owned connection is queried on its owning thread.
                assert_eq!(
                    unsafe { ffi::sqlite3_txn_state(reader.handle(), c"main".as_ptr()) },
                    ffi::SQLITE_TXN_NONE,
                );
                reader.close().unwrap();
            }));
        }
        for _ in 0..2 {
            received.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        for sender in release {
            sender.send(()).unwrap();
        }
        for task in tasks {
            task.join().unwrap();
        }
    });
    let checkpoint: (u32, i64, i64) = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    let calls = probe.calls.load(Ordering::SeqCst);
    drop(hook);
    assert!(calls >= 2, "native overlapping-reader gate was not reached");
    assert_eq!(
        checkpoint,
        (0, 0, 0),
        "closed native readers left WAL locks: SQLite {}, callbacks {calls}",
        rusqlite::version(),
    );
    connection.close().unwrap();
    temp.close().unwrap();
}
