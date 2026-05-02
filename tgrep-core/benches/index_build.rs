use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
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

fn bench_index_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_build");
    for (file_count, bytes_per_file) in [(100usize, 512usize), (500, 512)] {
        group.throughput(Throughput::Bytes((file_count * bytes_per_file) as u64));
        group.bench_with_input(
            BenchmarkId::new("build_index", file_count),
            &(file_count, bytes_per_file),
            |b, &(file_count, bytes_per_file)| {
                b.iter_batched(
                    || create_repo(file_count, bytes_per_file),
                    |dir| {
                        let index_dir = dir.path().join(".tgrep_bench");
                        tgrep_core::builder::build_index(dir.path(), Some(&index_dir), false, &[])
                            .unwrap();
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_index_build);
criterion_main!(benches);
