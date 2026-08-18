//! Cross-platform process memory introspection.
//!
//! Provides functions to query the current process's resident set size (RSS)
//! and the host's total physical memory — used to enforce an indexing memory
//! budget so tgrep doesn't OOM-kill the host on large monorepos.

/// Returns the current process's resident set size in bytes, or `None` if
/// the platform query fails.
#[cfg(target_os = "windows")]
pub fn process_rss_bytes() -> Option<u64> {
    windows_memory_counters().map(|c| c.WorkingSetSize as u64)
}

/// Returns the highest resident set size the process has reached, or `None`
/// if the platform query fails.
///
/// This is an OS-maintained high-water mark, so it is exact and costs nothing
/// to read — unlike sampling [`process_rss_bytes`], which can miss a peak that
/// occurs between polls.
#[cfg(target_os = "windows")]
pub fn peak_rss_bytes() -> Option<u64> {
    windows_memory_counters().map(|c| c.PeakWorkingSetSize as u64)
}

#[cfg(target_os = "windows")]
fn windows_memory_counters()
-> Option<windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::System::ProcessStatus::GetProcessMemoryInfo;
    use windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let handle = GetCurrentProcess();
        let mut counters = MaybeUninit::<PROCESS_MEMORY_COUNTERS>::zeroed();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // GetProcessMemoryInfo requires `cb` to hold the struct size on input.
        (*counters.as_mut_ptr()).cb = size;
        let ok = GetProcessMemoryInfo(handle, counters.as_mut_ptr(), size);
        if ok != 0 {
            Some(counters.assume_init())
        } else {
            None
        }
    }
}

#[cfg(target_os = "windows")]
pub fn total_physical_memory_bytes() -> Option<u64> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    unsafe {
        let mut status = MaybeUninit::<MEMORYSTATUSEX>::zeroed();
        (*status.as_mut_ptr()).dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        let ok = GlobalMemoryStatusEx(status.as_mut_ptr());
        if ok != 0 {
            Some(status.assume_init().ullTotalPhys)
        } else {
            None
        }
    }
}

#[cfg(target_os = "linux")]
pub fn process_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let rss_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    Some(rss_pages * page_size as u64)
}

#[cfg(target_os = "linux")]
pub fn total_physical_memory_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.trim().strip_suffix("kB")?.trim().parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Peak RSS from `VmHWM` ("high water mark") in `/proc/self/status`.
#[cfg(target_os = "linux")]
pub fn peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest.trim().strip_suffix("kB")?.trim().parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(target_os = "macos")]
pub fn process_rss_bytes() -> Option<u64> {
    use std::mem::MaybeUninit;
    unsafe {
        // `ru_maxrss` reports the *peak* RSS, so it never decreases after a
        // flush reclaims memory and would keep the process looking over-budget
        // forever. Query the *current* resident size via proc_pidinfo instead.
        let mut info = MaybeUninit::<libc::proc_taskinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        let ret = libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr() as *mut libc::c_void,
            size,
        );
        if ret == size {
            Some(info.assume_init().pti_resident_size)
        } else {
            None
        }
    }
}

#[cfg(target_os = "macos")]
pub fn total_physical_memory_bytes() -> Option<u64> {
    unsafe {
        let mut size: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        let mut mib = [libc::CTL_HW, libc::HW_MEMSIZE];
        let ret = libc::sysctl(
            mib.as_mut_ptr(),
            2,
            &mut size as *mut u64 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        );
        if ret == 0 { Some(size) } else { None }
    }
}

/// Peak RSS via `getrusage`. On macOS `ru_maxrss` is in **bytes** (on Linux the
/// same field is kilobytes), so no scaling is applied here.
#[cfg(target_os = "macos")]
pub fn peak_rss_bytes() -> Option<u64> {
    use std::mem::MaybeUninit;
    unsafe {
        let mut usage = MaybeUninit::<libc::rusage>::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) == 0 {
            Some(usage.assume_init().ru_maxrss as u64)
        } else {
            None
        }
    }
}

// Fallback for unsupported platforms
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn process_rss_bytes() -> Option<u64> {
    None
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn total_physical_memory_bytes() -> Option<u64> {
    None
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn peak_rss_bytes() -> Option<u64> {
    None
}

/// Format a byte count for humans, e.g. `1.53 GiB` or `151.2 MiB`.
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Compute the default memory cap: 50% of physical RAM, with a floor of 512 MB
/// and a ceiling of 16 GB. Returns bytes.
pub fn default_memory_cap_bytes() -> u64 {
    const FLOOR: u64 = 512 * 1024 * 1024; // 512 MB
    const CEILING: u64 = 16 * 1024 * 1024 * 1024; // 16 GB

    let half_ram = total_physical_memory_bytes()
        .map(|total| total / 2)
        .unwrap_or(4 * 1024 * 1024 * 1024); // fallback: 4 GB

    half_ram.clamp(FLOOR, CEILING)
}

#[cfg(test)]
mod tests {
    use super::*;

    // On supported platforms the queries must succeed and return non-zero
    // values. This guards regressions like a missing `PROCESS_MEMORY_COUNTERS.cb`
    // (or `MEMORYSTATUSEX.dwLength`) initialization, which would make the call
    // fail and silently disable the memory cap.
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    #[test]
    fn process_rss_is_nonzero() {
        let rss = process_rss_bytes().expect("process RSS query should succeed");
        assert!(rss > 0, "process RSS should be non-zero, got {rss}");
    }

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    #[test]
    fn total_physical_memory_is_nonzero() {
        let total =
            total_physical_memory_bytes().expect("total physical memory query should succeed");
        assert!(
            total > 0,
            "total physical memory should be non-zero, got {total}"
        );
    }

    #[test]
    fn default_cap_is_within_bounds() {
        let cap = default_memory_cap_bytes();
        assert!(cap >= 512 * 1024 * 1024);
        assert!(cap <= 16 * 1024 * 1024 * 1024);
    }

    // Peak must be a real high-water mark, not the current value: it has to be
    // queryable and at least as large as current RSS. A platform returning the
    // wrong struct field (e.g. WorkingSetSize instead of PeakWorkingSetSize)
    // would still be non-zero, so compare the two rather than just check > 0.
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    #[test]
    fn peak_rss_is_at_least_current_rss() {
        let current = process_rss_bytes().expect("current RSS query should succeed");
        let peak = peak_rss_bytes().expect("peak RSS query should succeed");
        assert!(peak > 0, "peak RSS should be non-zero");
        assert!(
            peak >= current,
            "peak RSS {peak} should be >= current RSS {current}"
        );
    }

    #[test]
    fn format_bytes_picks_sensible_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2 * 1024), "2.0 KiB");
        assert_eq!(format_bytes(151 * 1024 * 1024), "151.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");
        // Boundaries promote to the next unit rather than reading as "1024".
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GiB");
    }
}
