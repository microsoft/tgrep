use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn create_repo(file_count: usize, bytes_per_file: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let line = b"pub fn search_index(query: &str) -> bool { query.contains(\"needle\") }\n";
    for i in 0..file_count {
        let mut data = Vec::with_capacity(bytes_per_file);
        while data.len() < bytes_per_file {
            data.extend_from_slice(line);
        }
        data.truncate(bytes_per_file);
        std::fs::write(src.join(format!("file_{i:05}.rs")), data).unwrap();
    }
    dir
}

fn build_index_once(root: &Path) {
    let index_dir = root.join(".tgrep_bench");
    tgrep_core::builder::build_index(root, Some(&index_dir), false, &[]).unwrap();
}

fn bench_index_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_build");
    for (file_count, bytes_per_file) in
        [(100usize, 512usize), (500, 512), (2_000, 512), (5_000, 512)]
    {
        group.throughput(Throughput::Bytes((file_count * bytes_per_file) as u64));
        group.bench_with_input(
            BenchmarkId::new("build_index", file_count),
            &(file_count, bytes_per_file),
            |b, &(file_count, bytes_per_file)| {
                b.iter_batched(
                    || create_repo(file_count, bytes_per_file),
                    |dir| {
                        build_index_once(dir.path());
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

#[cfg(windows)]
fn child_working_set_bytes(child: &std::process::Child) -> Option<(u64, u64)> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };

    let handle = child.as_raw_handle();
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        PageFaultCount: 0,
        PeakWorkingSetSize: 0,
        WorkingSetSize: 0,
        QuotaPeakPagedPoolUsage: 0,
        QuotaPagedPoolUsage: 0,
        QuotaPeakNonPagedPoolUsage: 0,
        QuotaNonPagedPoolUsage: 0,
        PagefileUsage: 0,
        PeakPagefileUsage: 0,
    };

    let ok = unsafe {
        K32GetProcessMemoryInfo(
            handle,
            &mut counters,
            size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    if ok == 0 {
        None
    } else {
        Some((
            counters.WorkingSetSize as u64,
            counters.PeakWorkingSetSize as u64,
        ))
    }
}

#[cfg(windows)]
fn measure_peak_working_set(file_count: usize, bytes_per_file: usize) -> u64 {
    let repo = create_repo(file_count, bytes_per_file);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--peak-memory-child")
        .arg(repo.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut peak = 0u64;
    loop {
        if let Some((working_set, peak_working_set)) = child_working_set_bytes(&child) {
            peak = peak.max(working_set).max(peak_working_set);
        }
        if let Some(status) = child.try_wait().unwrap() {
            if let Some((working_set, peak_working_set)) = child_working_set_bytes(&child) {
                peak = peak.max(working_set).max(peak_working_set);
            }
            assert!(status.success(), "peak memory child failed: {status}");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    peak
}

fn format_mib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0
}

#[cfg(windows)]
fn run_peak_memory_probe(file_count: usize) {
    let bytes_per_file = 512usize;
    let peak = measure_peak_working_set(file_count, bytes_per_file);
    eprintln!(
        "index_build/build_index/{file_count} peak working set: {peak} bytes ({:.2} MiB)",
        format_mib(peak)
    );
}

#[cfg(not(windows))]
fn run_peak_memory_probe(_file_count: usize) {
    eprintln!("index_build peak memory probe is currently implemented only on Windows");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--peak-memory-child") {
        let root = args
            .get(2)
            .expect("missing root path for peak memory child");
        build_index_once(Path::new(root));
        return;
    }

    if let Some(index) = args.iter().position(|arg| arg == "--peak-memory") {
        let file_count = args
            .get(index + 1)
            .and_then(|arg| arg.parse().ok())
            .unwrap_or(5_000);
        run_peak_memory_probe(file_count);
        return;
    }

    let mut criterion = Criterion::default().configure_from_args();
    bench_index_build(&mut criterion);
    criterion.final_summary();
}
