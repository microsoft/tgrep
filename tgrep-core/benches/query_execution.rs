use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use tgrep_core::PostingEntry;
use tgrep_core::query::{QueryPlan, TrigramQuery, execute_plan_with_masks};

fn posting_list(len: u32, stride: u32) -> Vec<PostingEntry> {
    (0..len)
        .map(|i| PostingEntry {
            file_id: i * stride,
            loc_mask: u8::MAX,
            next_mask: u8::MAX,
        })
        .collect()
}

fn and_plan(hashes: &[u32]) -> QueryPlan {
    QueryPlan::And(
        hashes
            .iter()
            .copied()
            .map(|hash| TrigramQuery {
                hash,
                expected_next: None,
            })
            .collect(),
    )
}

fn or_plan(hashes: &[u32]) -> QueryPlan {
    QueryPlan::Or(hashes.iter().map(|hash| and_plan(&[*hash])).collect())
}

fn bench_query_execution(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_execution");

    for len in [100u32, 1_000, 10_000] {
        let hashes = [0x616263, 0x626364, 0x636465, 0x646566];
        let postings: HashMap<u32, Vec<PostingEntry>> = HashMap::from([
            (hashes[0], posting_list(len, 1)),
            (hashes[1], posting_list(len / 2, 2)),
            (hashes[2], posting_list(len / 4, 4)),
            (hashes[3], posting_list(len / 8, 8)),
        ]);
        let plan = and_plan(&hashes);

        group.bench_with_input(
            BenchmarkId::new("and_intersection", len),
            &plan,
            |b, plan| {
                b.iter(|| {
                    execute_plan_with_masks(black_box(plan), &|hash| {
                        postings.get(&hash).cloned().unwrap_or_default()
                    })
                });
            },
        );
    }

    for branches in [4usize, 16, 64] {
        let hashes: Vec<u32> = (0..branches).map(|i| 0x616263 + i as u32).collect();
        let postings: HashMap<u32, Vec<PostingEntry>> = hashes
            .iter()
            .copied()
            .map(|hash| (hash, posting_list(1_000, 1)))
            .collect();
        let plan = or_plan(&hashes);

        group.bench_with_input(BenchmarkId::new("or_union", branches), &plan, |b, plan| {
            b.iter(|| {
                execute_plan_with_masks(black_box(plan), &|hash| {
                    postings.get(&hash).cloned().unwrap_or_default()
                })
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_query_execution);
criterion_main!(benches);
