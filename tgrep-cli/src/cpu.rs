//! CPU budget for the background indexer.
//!
//! Indexing's CPU-heavy work (reading files + extracting trigrams) runs in
//! parallel via rayon, which by default fans out across every logical core and
//! can saturate the host. To stay a good neighbor — especially when tgrep is
//! embedded in another tool — the indexer confines that work to a bounded
//! worker pool sized from a CPU budget expressed as a percentage of cores.

use std::thread::available_parallelism;

/// Number of worker threads to use for the parallel indexing work, given a CPU
/// budget expressed as a percentage of logical cores (1–100).
///
/// Always at least 1 and never more than the available core count, so a 50%
/// budget on an 8-core host yields 4 threads, and any budget on a single-core
/// host yields 1.
pub fn index_thread_count(max_cpu_percent: u8) -> usize {
    let cores = available_parallelism().map(|n| n.get()).unwrap_or(1);
    let pct = max_cpu_percent.clamp(1, 100) as usize;
    (cores * pct).div_ceil(100).clamp(1, cores)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_to_at_least_one_thread() {
        // Even a 1% budget must keep indexing alive with a single worker.
        assert!(index_thread_count(1) >= 1);
    }

    #[test]
    fn never_exceeds_core_count() {
        let cores = available_parallelism().map(|n| n.get()).unwrap_or(1);
        assert!(index_thread_count(100) <= cores);
        // Out-of-range percentages are clamped to the 1..=100 budget.
        assert!(index_thread_count(u8::MAX) <= cores);
    }

    #[test]
    fn full_budget_uses_all_cores() {
        let cores = available_parallelism().map(|n| n.get()).unwrap_or(1);
        assert_eq!(index_thread_count(100), cores);
    }
}
