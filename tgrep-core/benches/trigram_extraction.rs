use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};

fn corpus(size: usize, mixed_case: bool) -> Vec<u8> {
    let lower = b"fn search_index(query: &str) -> Vec<Result> { query.bytes().collect() }\n";
    let mixed = b"Fn SearchIndex(Query: &str) -> Vec<Result> { Query.Bytes().Collect() }\n";
    let pattern: &[u8] = if mixed_case { mixed } else { lower };
    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        data.extend_from_slice(pattern);
    }
    data.truncate(size);
    data
}

fn bench_trigram_extraction(c: &mut Criterion) {
    let mut group = c.benchmark_group("trigram_extraction");
    for size in [1_024usize, 16 * 1_024, 256 * 1_024] {
        let lower = corpus(size, false);
        let mixed = corpus(size, true);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("extract_with_masks_lower", size),
            &lower,
            |b, data| {
                b.iter(|| tgrep_core::trigram::extract_with_masks(black_box(data)));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("extract_merged_masks_lower", size),
            &lower,
            |b, data| {
                b.iter(|| tgrep_core::trigram::extract_merged_masks(black_box(data)));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("extract_merged_masks_mixed", size),
            &mixed,
            |b, data| {
                b.iter(|| tgrep_core::trigram::extract_merged_masks(black_box(data)));
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_trigram_extraction);
criterion_main!(benches);
