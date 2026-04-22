use std::collections::HashMap;
use tgrep_core::{PostingEntry, builder, query, reader, trigram};

#[test]
fn case_insensitive_search_roundtrip() {
    let content = b"internal class AlertSchema : AlertBaseSchema";

    // Extract trigrams with masks (original + lowercase) just like builder/serve does
    let mut tri_masks = trigram::extract_with_masks(content);
    let lower = content.to_ascii_lowercase();
    if lower != *content {
        tri_masks.extend(trigram::extract_with_masks(&lower));
    }

    // Build inverted index with masks for file_id=0
    let mut per_tri: HashMap<u32, trigram::TrigramMasks> = HashMap::new();
    for &(tri, m) in &tri_masks {
        let entry = per_tri.entry(tri).or_default();
        entry.loc_mask |= m.loc_mask;
        entry.next_mask |= m.next_mask;
    }
    let mut inverted: HashMap<u32, Vec<PostingEntry>> = HashMap::new();
    for (tri, m) in per_tri {
        inverted.entry(tri).or_default().push(PostingEntry {
            file_id: 0,
            loc_mask: m.loc_mask,
            next_mask: m.next_mask,
        });
    }

    // Write to temp directory
    let tmp = std::env::temp_dir().join("tgrep_ci_roundtrip_test");
    let _ = std::fs::remove_dir_all(&tmp);
    let cwd = std::env::current_dir().unwrap();
    builder::write_index_from_snapshot(&cwd, &tmp, &["test/Alert.cs".to_string()], &inverted, true)
        .unwrap();

    // Read back from disk
    let reader = reader::IndexReader::open(&tmp).unwrap();
    assert_eq!(reader.num_files(), 1);
    assert!(reader.num_trigrams() > 0, "should have trigrams on disk");

    // Case-insensitive query via regex path (default, no -F)
    let plan = query::build_query_plan("class AlertSchema", true).unwrap();
    let candidates = query::execute_plan(&plan, &|tri| reader.lookup_trigram(tri));
    assert!(
        candidates.contains(&0),
        "case-insensitive regex search should find file 0"
    );

    // Case-insensitive query via literal path (-F flag)
    let plan_lit = query::build_literal_plan("class AlertSchema", true);
    let candidates_lit = query::execute_plan(&plan_lit, &|tri| reader.lookup_trigram(tri));
    assert!(
        candidates_lit.contains(&0),
        "case-insensitive literal search should find file 0"
    );

    // Case-sensitive should also work
    let plan_cs = query::build_query_plan("class AlertSchema", false).unwrap();
    let candidates_cs = query::execute_plan(&plan_cs, &|tri| reader.lookup_trigram(tri));
    assert!(
        candidates_cs.contains(&0),
        "case-sensitive search should find file 0"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
