//! Shared parallelism limits for CPU-intensive tgrep work.

use std::sync::OnceLock;

/// Maximum walker threads before applying the CPU-capacity limit.
pub const MAX_WALKER_THREADS: usize = 12;

static CPU_POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

/// Number of worker threads allowed for CPU-intensive work.
///
/// This is half of the available logical CPU capacity, rounded down so tgrep
/// does not exceed half capacity on machines with an odd CPU count. A single
/// worker is kept as the minimum so tgrep remains usable on one-CPU systems.
pub fn worker_threads() -> usize {
    half_cpu_threads(available_threads())
}

/// Number of walker threads after applying both the walker-specific cap and
/// the process-wide CPU-capacity limit.
pub fn walker_threads() -> usize {
    capped_half_cpu_threads(available_threads(), MAX_WALKER_THREADS)
}

/// Run CPU-intensive Rayon work on tgrep's capped shared thread pool.
pub fn install<OP, R>(op: OP) -> R
where
    OP: FnOnce() -> R + Send,
    R: Send,
{
    cpu_pool().install(op)
}

fn cpu_pool() -> &'static rayon::ThreadPool {
    CPU_POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(worker_threads())
            .thread_name(|index| format!("tgrep-worker-{index}"))
            .build()
            .expect("failed to initialize tgrep worker thread pool")
    })
}

fn available_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |threads| threads.get())
}

fn half_cpu_threads(available: usize) -> usize {
    (available / 2).max(1)
}

fn capped_half_cpu_threads(available: usize, cap: usize) -> usize {
    half_cpu_threads(available).min(cap.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_thread_count_uses_floor_half_with_one_thread_minimum() {
        assert_eq!(half_cpu_threads(0), 1);
        assert_eq!(half_cpu_threads(1), 1);
        assert_eq!(half_cpu_threads(2), 1);
        assert_eq!(half_cpu_threads(3), 1);
        assert_eq!(half_cpu_threads(4), 2);
        assert_eq!(half_cpu_threads(5), 2);
        assert_eq!(half_cpu_threads(8), 4);
    }

    #[test]
    fn walker_thread_count_applies_half_limit_before_walker_cap() {
        assert_eq!(capped_half_cpu_threads(32, MAX_WALKER_THREADS), 12);
        assert_eq!(capped_half_cpu_threads(24, MAX_WALKER_THREADS), 12);
        assert_eq!(capped_half_cpu_threads(23, MAX_WALKER_THREADS), 11);
        assert_eq!(capped_half_cpu_threads(8, MAX_WALKER_THREADS), 4);
        assert_eq!(capped_half_cpu_threads(2, MAX_WALKER_THREADS), 1);
    }
}
