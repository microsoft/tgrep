// Copyright (c) Microsoft Corporation. All rights reserved.

#![cfg(target_os = "macos")]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;

fn generation(file: &File) -> std::io::Result<Option<u32>> {
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
    const RETURNED: u32 = 0x8000_0000;
    let mut attributes = Attributes {
        count: 5,
        reserved: 0,
        common: GENERATION | RETURNED,
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
            size_of_val(&result),
            0x20,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if result[0] as usize != size_of_val(&result) || result[1] & GENERATION == 0 {
        return Ok(None);
    }
    Ok((result[6] != 0).then_some(result[6]))
}

#[test]
fn native_content_generation_invalidates_writes_and_rejects_retained_writable_mappings() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("owned-generation-proof.bin");
    std::fs::write(&path, vec![b'a'; 8192]).unwrap();
    let mut observer = File::open(&path).unwrap();
    let before = generation(&observer).unwrap();
    assert!(
        before.is_some(),
        "the native fixture filesystem lacks supported nonzero content generation"
    );
    {
        let mut writer = File::options().write(true).open(&path).unwrap();
        let times = writer.metadata().unwrap();
        writer.write_all(&vec![b'b'; 8192]).unwrap();
        writer.sync_all().unwrap();
        writer
            .set_times(
                std::fs::FileTimes::new()
                    .set_accessed(times.accessed().unwrap())
                    .set_modified(times.modified().unwrap()),
            )
            .unwrap();
    }
    let after_write = generation(&observer).unwrap();
    assert!(
        after_write.is_some() && after_write != before,
        "native generation must not be defeated by restoring user-settable timestamps: \
         {before:?} -> {after_write:?}"
    );
    let mut mapping = {
        let creator = File::options().read(true).write(true).open(&path).unwrap();
        // SAFETY: this owned fixture deliberately exercises external writable
        // mapping interference; no immutable Rust references point at its bytes.
        unsafe { memmap2::MmapOptions::new().map_mut(&creator).unwrap() }
    };
    let mapped = generation(&observer).unwrap();
    mapping[0] = b'c';
    let delayed = generation(&observer).unwrap();
    mapping.flush().unwrap();
    let flushed = generation(&observer).unwrap();
    assert_eq!(
        (mapped, delayed, flushed),
        (None, None, None),
        "a writable section with its creator descriptor closed must not supply reusable \
         content evidence; before={after_write:?}"
    );
    drop(mapping);
    let after_unmap = generation(&observer).unwrap();
    let mut first = [0_u8; 1];
    observer.seek(SeekFrom::Start(0)).unwrap();
    observer.read_exact(&mut first).unwrap();
    assert_eq!(first, [b'c']);
    assert!(
        after_unmap.is_some() && after_unmap != after_write,
        "mapped writes must invalidate earlier evidence after the mapping closes: \
         {after_write:?} -> {after_unmap:?}"
    );
    drop(observer);
    temp.close().unwrap();
}
