use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use tgrep_core::PostingEntry;
use tgrep_core::query::{self, QueryPlan, TrigramQuery, execute_plan_with_masks};
use tgrep_core::reader::IndexReader;

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

fn create_common_literal_index(file_count: usize) -> (tempfile::TempDir, IndexReader, QueryPlan) {
    let dir = tempfile::TempDir::new().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..file_count {
        std::fs::write(
            src.join(format!("file_{i:05}.rs")),
            format!("pub fn file_{i:05}() {{ let value = \"common_search_token_{i:05}\"; }}\n"),
        )
        .unwrap();
    }
    let index_dir = dir.path().join(".tgrep_bench");
    tgrep_core::builder::build_index(dir.path(), Some(&index_dir), false, &[]).unwrap();
    let reader = IndexReader::open(&index_dir).unwrap();
    let plan = query::build_literal_plan("common_search_token", false);
    (dir, reader, plan)
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

    for file_count in [1_000usize, 5_000] {
        let (_dir, reader, plan) = create_common_literal_index(file_count);
        group.bench_with_input(
            BenchmarkId::new("on_disk_common_literal", file_count),
            &plan,
            |b, plan| {
                b.iter(|| {
                    execute_plan_with_masks(black_box(plan), &|hash| {
                        reader.lookup_trigram_with_masks(hash)
                    })
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_query_execution);
criterion_main!(benches);
