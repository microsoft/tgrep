use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use std::path::Path;
#[cfg(windows)]
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

fn create_high_diversity_repo(file_count: usize, bytes_per_file: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..file_count {
        let mut state = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
        let mut data = Vec::with_capacity(bytes_per_file);
        while data.len() < bytes_per_file {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let byte = 32 + ((state >> 32) % 95) as u8;
            data.push(byte);
        }
        std::fs::write(src.join(format!("diverse_{i:05}.txt")), data).unwrap();
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
    group.throughput(Throughput::Bytes(1_000 * 1024));
    group.bench_function("build_index_high_diversity/1000", |b| {
        b.iter_batched(
            || create_high_diversity_repo(1_000, 1024),
            |dir| {
                build_index_once(dir.path());
            },
            BatchSize::SmallInput,
        );
    });
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
fn measure_peak_working_set(
    file_count: usize,
    bytes_per_file: usize,
    create: fn(usize, usize) -> TempDir,
) -> u64 {
    let repo = create(file_count, bytes_per_file);
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
fn run_peak_memory_probe(file_count: usize, bytes_per_file: usize, high_diversity: bool) {
    let create = if high_diversity {
        create_high_diversity_repo
    } else {
        create_repo
    };
    let peak = measure_peak_working_set(file_count, bytes_per_file, create);
    let case_name = if high_diversity {
        "build_index_high_diversity"
    } else {
        "build_index"
    };
    eprintln!(
        "index_build/{case_name}/{file_count} peak working set: {peak} bytes ({:.2} MiB)",
        format_mib(peak)
    );
}

#[cfg(not(windows))]
fn run_peak_memory_probe(_file_count: usize, _bytes_per_file: usize, _high_diversity: bool) {
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
        run_peak_memory_probe(file_count, 512, false);
        return;
    }

    if let Some(index) = args
        .iter()
        .position(|arg| arg == "--peak-memory-high-diversity")
    {
        let file_count = args
            .get(index + 1)
            .and_then(|arg| arg.parse().ok())
            .unwrap_or(1_000);
        run_peak_memory_probe(file_count, 1024, true);
        return;
    }

    let mut criterion = Criterion::default().configure_from_args();
    bench_index_build(&mut criterion);
    criterion.final_summary();
}
