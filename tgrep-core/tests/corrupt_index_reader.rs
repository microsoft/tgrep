//! `IndexReader` must survive a corrupt or truncated index.
//!
//! `lookup.bin` supplies the `offset` (u64) and `length` (u32) that drive
//! decoding of `index.bin`: `offset` is the slice base and `length` is the loop
//! bound. Neither is validated by the format itself, so on a damaged index they
//! are arbitrary. Reading them naively is what made the reader panic on
//! `offset = u64::MAX` (the bounds check `pos + 6 <= len` wraps, and wraps
//! *downwards*, so the further out of range the offset the more likely it is to
//! be accepted) and spin for tens of seconds on `length = u32::MAX`.
//!
//! This is the deterministic counterpart to the `fuzz_reader` fuzz target: it
//! runs the same invariants over a fixed pseudo-random corpus plus the specific
//! adversarial shapes, so the coverage exists in `cargo test` without needing a
//! nightly toolchain and `cargo-fuzz`.

use std::path::Path;
use std::time::{Duration, Instant};
use tgrep_core::reader::IndexReader;

/// `lookup.bin` record: trigram(u32 LE) + offset(u64 LE) + length(u32 LE).
const LOOKUP_ENTRY_SIZE: usize = 16;
/// `index.bin` record: file_id(u32 LE) + loc_mask(u8) + next_mask(u8).
const POSTING_ENTRY_SIZE: usize = 6;

/// No single reader operation in this file should come close to this. The old
/// unbounded loop took ~55 s in a debug build against a 60-byte `index.bin`.
const BUDGET: Duration = Duration::from_secs(30);

fn lookup_entry(trigram: u32, offset: u64, length: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(LOOKUP_ENTRY_SIZE);
    v.extend_from_slice(&trigram.to_le_bytes());
    v.extend_from_slice(&offset.to_le_bytes());
    v.extend_from_slice(&length.to_le_bytes());
    v
}

fn postings_bytes(n: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(n as usize * POSTING_ENTRY_SIZE);
    for file_id in 0..n {
        v.extend_from_slice(&file_id.to_le_bytes());
        v.push(0xAA);
        v.push(0x55);
    }
    v
}

/// `files.bin` record: file_id(u32 LE) + path_len(u16 LE) + path bytes. IDs must
/// be dense from 0 or `open` rejects the table.
fn files_bytes(n: u32) -> Vec<u8> {
    let mut v = Vec::new();
    for file_id in 0..n {
        let path = format!("f{file_id}.rs");
        v.extend_from_slice(&file_id.to_le_bytes());
        v.extend_from_slice(&(path.len() as u16).to_le_bytes());
        v.extend_from_slice(path.as_bytes());
    }
    v
}

fn write_index(dir: &Path, lookup: &[u8], postings: &[u8], files: &[u8]) {
    std::fs::write(dir.join("lookup.bin"), lookup).unwrap();
    std::fs::write(dir.join("index.bin"), postings).unwrap();
    std::fs::write(dir.join("files.bin"), files).unwrap();
}

/// Exercise every read path the way `fuzz_reader` does, and assert the one
/// invariant that matters: nothing decoded can exceed what `index.bin` holds.
fn exercise(dir: &Path, postings_len: usize) {
    let started = Instant::now();
    let Ok(reader) = IndexReader::open(dir) else {
        return; // structurally rejected up front — the loud path, also fine
    };
    let _ = reader.validate_lookup();

    let max_decodable = postings_len / POSTING_ENTRY_SIZE;
    for i in 0..reader.num_trigrams() {
        let (trigram, entries) = reader.trigram_posting_at(i);
        assert!(
            entries.len() <= max_decodable,
            "entry {i} decoded {} postings from a {postings_len}-byte index.bin",
            entries.len()
        );
        assert!(
            entries.capacity() <= max_decodable.max(1),
            "entry {i} reserved capacity {} for at most {max_decodable} entries",
            entries.capacity()
        );
        if let Some((_, raw)) = reader.nth_trigram_raw(i) {
            assert!(raw.len() <= postings_len);
        }
        assert_eq!(reader.lookup_trigram(trigram).len(), entries.len());
    }
    for probe in [0u32, 1, u32::MAX / 2, u32::MAX] {
        let _ = reader.lookup_trigram(probe);
    }

    let elapsed = started.elapsed();
    assert!(
        elapsed < BUDGET,
        "reading a {postings_len}-byte index took {elapsed:?}; \
         work is scaling with the untrusted `length` field"
    );
}

/// The two shapes reported in the wild, plus the neighbours that probe the same
/// arithmetic: offsets that wrap the bounds check, and lengths that dwarf the file.
#[test]
fn adversarial_lookup_entries_are_handled_without_panic_or_hang() {
    let postings = postings_bytes(10);
    let files = files_bytes(10);

    let offsets = [
        0,
        1,
        5,
        postings.len() as u64 - 1,
        postings.len() as u64,
        postings.len() as u64 + 1,
        u64::MAX,
        u64::MAX - 1,
        u64::MAX - POSTING_ENTRY_SIZE as u64,
        u64::MAX / 2,
        u32::MAX as u64,
        1 << 40,
    ];
    let lengths = [0u32, 1, 5, 10, 11, 1 << 16, 1 << 31, u32::MAX - 1, u32::MAX];

    let tmp = tempfile::tempdir().unwrap();
    for offset in offsets {
        for length in lengths {
            write_index(
                tmp.path(),
                &lookup_entry(1, offset, length),
                &postings,
                &files,
            );
            exercise(tmp.path(), postings.len());
        }
    }
}

/// A lookup entry whose range is partly valid must still yield the prefix that
/// genuinely fits — the fix must not turn graceful degradation into data loss.
#[test]
fn truncated_postings_still_yield_the_entries_that_fit() {
    let tmp = tempfile::tempdir().unwrap();
    let postings = postings_bytes(6);
    // Start at entry 2 and claim 100 entries; only 4 whole entries remain.
    write_index(
        tmp.path(),
        &lookup_entry(1, 2 * POSTING_ENTRY_SIZE as u64, 100),
        &postings,
        &files_bytes(6),
    );
    let reader = IndexReader::open(tmp.path()).unwrap();
    let ids: Vec<u32> = reader.lookup_trigram(1);
    assert_eq!(ids, vec![2, 3, 4, 5]);
    // ...and the same index is reported as corrupt by the validating path.
    assert!(reader.validate_lookup().is_err());
}

/// Deterministic pseudo-random index bytes. Mirrors `fuzz_reader` so a
/// regression is caught by `cargo test` even without a fuzzing run.
#[test]
fn random_index_bytes_never_panic() {
    // xorshift64* — a fixed seed keeps failures reproducible.
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };

    let tmp = tempfile::tempdir().unwrap();
    for _ in 0..300 {
        let num_entries = (next() % 8) as usize;
        let num_postings = (next() % 12) as u32;
        let postings = postings_bytes(num_postings);

        let mut lookup = Vec::with_capacity(num_entries * LOOKUP_ENTRY_SIZE);
        for i in 0..num_entries {
            // Ascending trigrams keep the binary search meaningful; the offset
            // and length are what we want arbitrary.
            let trigram = (i as u32).wrapping_mul(7).wrapping_add(1);
            let offset = match next() % 4 {
                // Biased towards plausible-but-wrong values, since a uniformly
                // random u64 is almost always trivially out of range.
                0 => next() % (postings.len() as u64 + 8),
                1 => u64::MAX - (next() % 8),
                2 => next(),
                _ => (next() % 8) * POSTING_ENTRY_SIZE as u64,
            };
            let length = match next() % 3 {
                0 => (next() % 16) as u32,
                1 => u32::MAX - (next() % 4) as u32,
                _ => next() as u32,
            };
            lookup.extend_from_slice(&lookup_entry(trigram, offset, length));
        }

        write_index(tmp.path(), &lookup, &postings, &files_bytes(num_postings));
        exercise(tmp.path(), postings.len());
    }
}
