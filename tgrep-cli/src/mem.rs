//! Cross-platform process memory introspection.
//!
//! Provides functions to query the current process's resident set size (RSS)
//! and the host's total physical memory — used to enforce an indexing memory
//! budget so tgrep doesn't OOM-kill the host on large monorepos.

/// Returns the current process's resident set size in bytes, or `None` if
/// the platform query fails.
#[cfg(target_os = "windows")]
pub fn process_rss_bytes() -> Option<u64> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::System::ProcessStatus::GetProcessMemoryInfo;
    use windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let handle = GetCurrentProcess();
        let mut counters = MaybeUninit::<PROCESS_MEMORY_COUNTERS>::zeroed();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = GetProcessMemoryInfo(handle, counters.as_mut_ptr(), size);
        if ok != 0 {
            let counters = counters.assume_init();
            Some(counters.WorkingSetSize as u64)
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

// Fallback for unsupported platforms
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn process_rss_bytes() -> Option<u64> {
    None
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn total_physical_memory_bytes() -> Option<u64> {
    None
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
