// Copyright (c) Microsoft Corporation. All rights reserved.

use super::{Error, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Stamp {
    pub(crate) boot: String,
    pub(crate) millis: u64,
    pub(crate) wall_millis: u64,
}

pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Result<Stamp>;
}

pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Result<Stamp> {
        let (boot, millis) = platform_clock()?;
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| Error::io(std::io::Error::other(error)))?;
        Ok(Stamp {
            boot,
            millis,
            wall_millis: u64::try_from(wall.as_millis())
                .map_err(|_| Error::invalid("wall clock range"))?,
        })
    }
}

#[cfg(target_os = "linux")]
fn platform_clock() -> Result<(String, u64)> {
    use std::io::Read;
    let mut boot = String::new();
    std::fs::File::open("/proc/sys/kernel/random/boot_id")?
        .take(128)
        .read_to_string(&mut boot)?;
    let boot = boot.trim();
    if boot.len() != 36
        || !boot
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(Error::corrupt("invalid Linux boot identity"));
    }
    let mut time: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: the output is a correctly sized timespec.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let seconds = u64::try_from(time.tv_sec).map_err(|_| Error::corrupt("negative boot clock"))?;
    let nanos = u64::try_from(time.tv_nsec).map_err(|_| Error::corrupt("negative boot clock"))?;
    let millis = seconds
        .checked_mul(1000)
        .and_then(|value| value.checked_add(nanos / 1_000_000))
        .ok_or_else(|| Error::corrupt("boot clock overflow"))?;
    Ok((boot.into(), millis))
}

#[cfg(windows)]
fn platform_clock() -> Result<(String, u64)> {
    #[repr(C)]
    struct BootInformation {
        identifier: [u8; 16],
        firmware_type: u32,
        flags: u64,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQuerySystemInformation(
            class: u32,
            data: *mut std::ffi::c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetTickCount64() -> u64;
    }
    let mut boot: BootInformation = unsafe { std::mem::zeroed() };
    let mut returned = 0;
    // SAFETY: SystemBootEnvironmentInformation (90) writes this fixed native layout.
    let status = unsafe {
        NtQuerySystemInformation(
            90,
            (&mut boot as *mut BootInformation).cast(),
            std::mem::size_of::<BootInformation>() as u32,
            &mut returned,
        )
    };
    if status < 0 || returned < 16 || boot.identifier == [0; 16] {
        return Err(Error::io(std::io::Error::other(format!(
            "boot identity query failed: NTSTATUS {status:#x}"
        ))));
    }
    let mut identity = String::with_capacity(32);
    for byte in boot.identifier {
        use std::fmt::Write;
        write!(identity, "{byte:02x}").expect("writing a string");
    }
    // SAFETY: GetTickCount64 has no arguments and returns elapsed boot milliseconds.
    Ok((identity, unsafe { GetTickCount64() }))
}

#[cfg(target_os = "macos")]
fn platform_clock() -> Result<(String, u64)> {
    #[repr(C)]
    struct Timebase {
        numerator: u32,
        denominator: u32,
    }
    #[link(name = "System")]
    unsafe extern "C" {
        fn mach_continuous_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
    let mut boot = [0_u8; 128];
    let mut length = boot.len();
    // SAFETY: this read-only sysctl writes at most length bytes to the fixed buffer.
    if unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            boot.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let identity = std::str::from_utf8(&boot[..length])
        .map_err(|_| Error::corrupt("invalid macOS boot identity"))?
        .trim_end_matches('\0');
    if identity.is_empty() {
        return Err(Error::corrupt("empty macOS boot identity"));
    }
    let mut timebase = Timebase {
        numerator: 0,
        denominator: 0,
    };
    // SAFETY: the API writes one native timebase structure.
    if unsafe { mach_timebase_info(&mut timebase) } != 0 || timebase.denominator == 0 {
        return Err(Error::corrupt("macOS continuous timebase unavailable"));
    }
    // SAFETY: the continuous clock has no arguments.
    let ticks = unsafe { mach_continuous_time() };
    let millis = u128::from(ticks) * u128::from(timebase.numerator)
        / u128::from(timebase.denominator)
        / 1_000_000;
    Ok((
        identity.into(),
        u64::try_from(millis).map_err(|_| Error::corrupt("boot clock overflow"))?,
    ))
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_clock() -> Result<(String, u64)> {
    Err(Error::incompatible(
        "persistent monotonic age is unsupported on this platform",
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IdleEvidence {
    pub(crate) stamp: Stamp,
    pub(crate) proven_millis: u64,
    pub(crate) use_sequence: u64,
}

pub(crate) fn advance_idle(
    previous: Option<&IdleEvidence>,
    stamp: Stamp,
    sequence: u64,
) -> (IdleEvidence, bool) {
    let previous = previous.filter(|previous| {
        previous.use_sequence == sequence
            && previous.stamp.boot == stamp.boot
            && previous.stamp.millis <= stamp.millis
            && previous.stamp.wall_millis <= stamp.wall_millis
    });
    let proven_millis = previous.map_or(0, |previous| {
        previous
            .proven_millis
            .saturating_add(stamp.millis - previous.stamp.millis)
    });
    (
        IdleEvidence {
            stamp,
            proven_millis,
            use_sequence: sequence,
        },
        previous.is_some(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(boot: &str, millis: u64, wall_millis: u64) -> Stamp {
        Stamp {
            boot: boot.into(),
            millis,
            wall_millis,
        }
    }

    #[test]
    fn age_requires_proven_monotonic_progress_and_unchanged_use() {
        let (first, known) = advance_idle(None, stamp("a", 100, 1000), 3);
        assert!(!known);
        let (next, known) = advance_idle(Some(&first), stamp("a", 300, 1200), 3);
        assert!(known);
        assert_eq!(next.proven_millis, 200);
        for (current, sequence) in [
            (stamp("b", 500, 1400), 3),
            (stamp("a", 500, 900), 3),
            (stamp("a", 50, 1400), 3),
            (stamp("a", 500, 1400), 4),
        ] {
            let (reset, known) = advance_idle(Some(&next), current, sequence);
            assert!(!known);
            assert_eq!(reset.proven_millis, 0);
        }
    }

    #[test]
    fn native_boot_clock_is_identified_and_monotonic() {
        let first = SystemClock.now().unwrap();
        let second = SystemClock.now().unwrap();
        assert!(!first.boot.is_empty());
        assert_eq!(first.boot, second.boot);
        assert!(second.millis >= first.millis);
    }
}
