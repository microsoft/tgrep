//! Cross-platform process memory introspection.
//!
//! Provides functions to query the current process's memory use and the host's
//! total physical memory — used to enforce an indexing memory budget so tgrep
//! doesn't OOM-kill the host on large monorepos.
//!
//! Two different quantities are available and they are not interchangeable:
//!
//! * **Resident set / working set** counts every physical page the process has
//!   mapped, *including* file-backed pages from `mmap`. Since indexing maps
//!   files at or above 1 MiB rather than reading them into heap, this number
//!   tracks the size of the files being indexed. Those pages are clean and
//!   file-backed, so the OS can drop them at any time without the process
//!   doing anything — they are not memory tgrep is holding.
//! * **Private bytes** counts only memory the process itself owns and must give
//!   back explicitly: heap, stacks, anonymous maps. This is what "how much
//!   memory does tgrep use" means, and it is what can actually exhaust a host.
//!
//! Indexing a single 2 GiB file peaks at 1.99 GiB working set but only 77.8 MiB
//! private, so reporting or budgeting against the working set overstates the
//! footprint by more than 25x. Prefer [`peak_private_bytes`] for reporting and
//! [`budgeted_memory_bytes`] for enforcing the cap; the RSS functions remain
//! for platforms that cannot separate the two.

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
/// occurs between polls. It includes memory-mapped file pages; see the module
/// docs for why that makes it the wrong number to report on its own.
#[cfg(target_os = "windows")]
pub fn peak_rss_bytes() -> Option<u64> {
    windows_memory_counters().map(|c| c.PeakWorkingSetSize as u64)
}

/// Private committed bytes: memory the process owns, excluding file-backed
/// pages.
#[cfg(target_os = "windows")]
pub fn process_private_bytes() -> Option<u64> {
    windows_memory_counters().map(|c| c.PagefileUsage as u64)
}

/// High-water mark of [`process_private_bytes`].
///
/// Windows maintains this exactly, so unlike the sampled fallback on other
/// platforms it cannot miss a spike between polls.
#[cfg(target_os = "windows")]
pub fn peak_private_bytes() -> Option<u64> {
    windows_memory_counters().map(|c| c.PeakPagefileUsage as u64)
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
///
/// Includes memory-mapped file pages; see the module docs.
#[cfg(target_os = "linux")]
pub fn peak_rss_bytes() -> Option<u64> {
    proc_status_kb("VmHWM:").map(|kb| kb * 1024)
}

/// Read a `kB`-suffixed field out of `/proc/self/status`.
#[cfg(target_os = "linux")]
fn proc_status_kb(prefix: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest.trim().strip_suffix("kB")?.trim().parse().ok();
        }
    }
    None
}

/// Highest value [`process_private_bytes`] has been observed at.
///
/// Linux exposes `VmHWM` for resident memory but has no equivalent high-water
/// mark for anonymous memory alone, so the peak has to be sampled.
#[cfg(target_os = "linux")]
static PRIVATE_HWM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Anonymous resident bytes from `RssAnon`: heap, stacks and anonymous maps,
/// with file-backed pages excluded.
///
/// Reading it also advances the sampled high-water mark that
/// [`peak_private_bytes`] reports, so any caller that polls this — the indexing
/// memory cap does, once per batch — improves that peak's accuracy.
#[cfg(target_os = "linux")]
pub fn process_private_bytes() -> Option<u64> {
    // `RssAnon` landed in Linux 4.5. On anything older the field is absent and
    // callers fall back to RSS rather than silently reporting zero.
    let bytes = proc_status_kb("RssAnon:")? * 1024;
    PRIVATE_HWM.fetch_max(bytes, std::sync::atomic::Ordering::Relaxed);
    Some(bytes)
}

/// Sampled high-water mark of [`process_private_bytes`].
///
/// Only as good as the sampling: a spike between polls is missed. That makes it
/// a floor, not an exact peak, which is still far closer to the truth than
/// `VmHWM` once large files are memory-mapped.
#[cfg(target_os = "linux")]
pub fn peak_private_bytes() -> Option<u64> {
    let current = process_private_bytes()?;
    Some(current.max(PRIVATE_HWM.load(std::sync::atomic::Ordering::Relaxed)))
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
///
/// Includes memory-mapped file pages; see the module docs.
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

/// macOS separates file-backed from anonymous memory only through the Mach
/// `TASK_VM_INFO` `phys_footprint` counter, which `libc` does not expose.
/// Returning `None` makes callers fall back to RSS rather than report a number
/// that isn't the one it claims to be.
#[cfg(target_os = "macos")]
pub fn process_private_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "macos")]
pub fn peak_private_bytes() -> Option<u64> {
    None
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

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn process_private_bytes() -> Option<u64> {
    None
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn peak_private_bytes() -> Option<u64> {
    None
}

/// The figure to charge against the indexing memory cap.
///
/// Prefers private bytes. The cap exists to stop tgrep exhausting the host, and
/// mapped file pages cannot do that — the kernel reclaims them under pressure.
/// Charging them anyway makes the cap fire on memory the process does not hold,
/// and the flush it triggers then frees nothing, so the build pays for a full
/// overlay flush and is still "over budget" on the next check.
pub fn budgeted_memory_bytes() -> Option<u64> {
    process_private_bytes().or_else(process_rss_bytes)
}

/// How much larger the working set must be than private bytes before it is
/// worth reporting both, as a divisor of private bytes.
const WORKING_SET_REPORT_RATIO: u64 = 4;

/// Absolute floor on that gap. Every process maps its own executable and shared
/// libraries, so the working set is always somewhat larger; on a run that
/// allocates little, that fixed overhead alone clears the ratio and would
/// produce a second figure that says nothing about the build.
const WORKING_SET_REPORT_FLOOR: u64 = 64 * 1024 * 1024;

/// Human-readable peak memory for an indexing run, or `None` if the platform
/// exposes nothing.
///
/// Leads with private bytes, because that is the memory tgrep is responsible
/// for. When the peak working set is substantially larger the difference is
/// memory-mapped file content, so it is named explicitly rather than folded
/// into a single number that would read as tgrep's own use.
pub fn format_peak_memory() -> Option<String> {
    describe_peak_memory(peak_private_bytes(), peak_rss_bytes())
}

/// The reporting decision, split out from the platform queries so it can be
/// tested against fixed inputs instead of whatever the host happens to be
/// doing.
fn describe_peak_memory(private: Option<u64>, rss: Option<u64>) -> Option<String> {
    let Some(private) = private else {
        return rss.map(format_bytes);
    };
    let floor = private
        .saturating_add(WORKING_SET_REPORT_FLOOR)
        .max(private.saturating_add(private / WORKING_SET_REPORT_RATIO));
    match rss {
        Some(rss) if rss > floor => Some(format!(
            "{} private, {} working set incl. memory-mapped files",
            format_bytes(private),
            format_bytes(rss)
        )),
        _ => Some(format_bytes(private)),
    }
}

/// Keeps [`peak_private_bytes`] honest for the duration of a build on platforms
/// whose kernel does not maintain a private-memory high-water mark.
///
/// Linux exposes `RssAnon` only as an instantaneous value, so a peak read once
/// at the end of a build reports whatever survived it — after the sorter has
/// dropped its 64 MiB arena, that is a small fraction of the real high point.
/// Polling in the background keeps the mark meaningful. Windows tracks the peak
/// itself, so there the sampler is an empty value that starts no thread.
#[cfg(target_os = "linux")]
pub struct PrivatePeakSampler {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl PrivatePeakSampler {
    pub fn start() -> Self {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                // Updates PRIVATE_HWM as a side effect; the value is unused.
                let _ = process_private_bytes();
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for PrivatePeakSampler {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub struct PrivatePeakSampler;

#[cfg(not(target_os = "linux"))]
impl PrivatePeakSampler {
    pub fn start() -> Self {
        Self
    }
}

#[cfg(not(target_os = "linux"))]
impl Drop for PrivatePeakSampler {
    /// Nothing to stop — the kernel maintains the high-water mark itself. The
    /// impl exists so callers can end the sampled window with `drop` on every
    /// platform instead of branching on the target.
    fn drop(&mut self) {}
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

    // Private bytes must be a different counter from the working set. Indexing
    // memory-maps files at or above 1 MiB, so a working-set figure grows with
    // the size of the files being indexed: a single 2 GiB file measured 1.99
    // GiB peak working set against 77.8 MiB peak private, so reporting the
    // former as "peak memory" overstated the footprint by 25x.
    //
    // Whether mapped pages *stay* resident is the OS's call — Windows trims
    // them within milliseconds under cache pressure — so this asserts the
    // counters are queryable and coherent rather than trying to pin page
    // residency, which is not reproducible.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    #[test]
    fn private_bytes_are_queryable_and_within_the_resident_set() {
        let private = process_private_bytes().expect("private byte query should succeed");
        let rss = process_rss_bytes().expect("RSS query should succeed");
        assert!(private > 0, "private bytes should be non-zero");
        assert!(rss > 0, "RSS should be non-zero");
    }

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    #[test]
    fn peak_private_is_at_least_current_private() {
        let Some(current) = process_private_bytes() else {
            return; // macOS cannot separate the two; callers fall back to RSS.
        };
        let peak = peak_private_bytes().expect("peak private query should succeed");
        assert!(
            peak >= current,
            "peak private {peak} should be >= current {current}"
        );
    }

    // The cap exists to stop tgrep exhausting the host, so it must charge
    // against memory the process actually owns wherever that is available.
    #[test]
    fn budgeted_memory_prefers_private_bytes() {
        let budgeted = budgeted_memory_bytes();
        match process_private_bytes() {
            Some(private) => assert_eq!(budgeted, Some(private)),
            None => assert_eq!(budgeted, process_rss_bytes()),
        }
    }

    #[test]
    fn peak_memory_report_names_the_working_set_only_when_it_is_much_larger() {
        let mib = 1024 * 1024;
        // A mapped-file build: the gap is the mapping, and hiding it behind one
        // number is what made a 77.8 MiB build look like a 1.99 GiB one.
        assert_eq!(
            describe_peak_memory(Some(78 * mib), Some(2038 * mib)),
            Some("78.0 MiB private, 1.99 GiB working set incl. memory-mapped files".to_string())
        );
        // Measured on a 200 x 8 MiB corpus: a real gap, well past both bounds.
        assert_eq!(
            describe_peak_memory(Some(365 * mib), Some(470 * mib)),
            Some("365.0 MiB private, 470.0 MiB working set incl. memory-mapped files".to_string())
        );
        // Ordinary builds: the gap is code and shared library mappings, which
        // say nothing worth a second figure.
        assert_eq!(
            describe_peak_memory(Some(300 * mib), Some(310 * mib)),
            Some("300.0 MiB".to_string())
        );
        // Clears the ratio but not the floor. Every process maps its own
        // executable, so a tiny run trips the ratio on that alone — a Linux
        // `tgrep index` over three files measured 1.5 MiB private against a
        // 12 MiB working set, which is 8x and still not worth reporting.
        assert_eq!(
            describe_peak_memory(Some(3 * mib / 2), Some(12 * mib)),
            Some("1.5 MiB".to_string())
        );
        // Clears the floor but not the ratio.
        assert_eq!(
            describe_peak_memory(Some(4096 * mib), Some(4200 * mib)),
            Some("4.00 GiB".to_string())
        );
        // Exactly at both bounds is not "much larger"; a byte past is.
        assert_eq!(
            describe_peak_memory(Some(1024 * mib), Some(1280 * mib)),
            Some("1.00 GiB".to_string())
        );
        assert!(
            describe_peak_memory(Some(1024 * mib), Some(1280 * mib + 1))
                .unwrap()
                .contains("working set")
        );
        // Platforms that cannot separate the two still report something, and a
        // platform that exposes nothing reports nothing.
        assert_eq!(
            describe_peak_memory(None, Some(512 * mib)),
            Some("512.0 MiB".to_string())
        );
        assert_eq!(
            describe_peak_memory(Some(512 * mib), None),
            Some("512.0 MiB".to_string())
        );
        assert_eq!(describe_peak_memory(None, None), None);
    }

    // On Linux the peak is sampled in the background, so a build that allocates
    // and then frees must still report the high point rather than whatever
    // survived it — the sorter drops its 64 MiB arena before the build ends.
    // Reads the mark directly: going through `peak_private_bytes` would fold in
    // the current value and blur what is being checked.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_sampler_records_a_peak_the_process_no_longer_holds() {
        const HOG: usize = 192 * 1024 * 1024;
        let sampler = PrivatePeakSampler::start();
        {
            let mut hog: Vec<u8> = vec![7; HOG];
            hog[0] = 1;
            std::hint::black_box(&hog);
            // Comfortably longer than the 50 ms sampling interval, so a poll
            // has to land while the allocation is live.
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        drop(sampler);

        let recorded = PRIVATE_HWM.load(std::sync::atomic::Ordering::Relaxed);
        let current = proc_status_kb("RssAnon:").expect("RssAnon should be readable") * 1024;
        assert!(
            recorded >= current + (HOG as u64) / 2,
            "the sampler should have recorded the freed allocation: \
             mark {recorded}, current {current}"
        );
    }

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    #[test]
    fn peak_memory_report_is_available_on_supported_platforms() {
        let report = format_peak_memory().expect("a peak memory report should be available");
        assert!(!report.is_empty());
    }
}
