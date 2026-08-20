#![no_main]

use libfuzzer_sys::fuzz_target;
use std::path::PathBuf;
use tgrep_core::reader::IndexReader;

/// Fuzz the mmap-backed on-disk reader against arbitrary index bytes.
///
/// `fuzz_ondisk` only round-trips `PostingEntry` encode/decode, so nothing
/// reaches `IndexReader` itself — yet that is where untrusted values do damage.
/// The `offset` (u64) and `length` (u32) fields of a `lookup.bin` entry are the
/// loop bound and the slice base for decoding `index.bin`, so a corrupt pair
/// there is what turns a bad file into a panic, an out-of-range slice, or a
/// multi-gigabyte reservation.
///
/// Byte layout the fuzzer is steering (see `tgrep-core/src/ondisk.rs`):
///   lookup.bin  16-byte records: trigram(u32 LE) offset(u64 LE) length(u32 LE)
///   index.bin    6-byte records: file_id(u32 LE) loc_mask(u8) next_mask(u8)
///   files.bin   var records:     file_id(u32 LE) path_len(u16 LE) path bytes
const POSTING_ENTRY_SIZE: usize = 6;

/// Keep one iteration cheap: the target writes three files and then decodes
/// every section it wrote, so cost is linear in the input.
const MAX_INPUT: usize = 8 * 1024;

/// Cap the per-entry decode loop so a lookup table full of maximal `length`
/// values can't dominate an iteration. `validate_lookup` still walks all of it.
const MAX_ENTRIES_DECODED: usize = 64;

/// One scratch directory per fuzzer process, reused across iterations. The
/// reader is dropped at the end of each run, releasing its mmaps before the
/// next iteration overwrites the files.
fn index_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tgrep_fuzz_reader_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch index dir");
    dir
}

/// Carve the input into the three files of an index. The leading byte steers
/// how much lands in `lookup.bin` — the section holding the untrusted
/// offset/length pairs — versus `index.bin`, so the fuzzer can reach both
/// "pointer far past a tiny postings region" and "huge postings, tiny table".
fn split(data: &[u8]) -> (&[u8], &[u8], &[u8]) {
    let Some((control, rest)) = data.split_first() else {
        return (&[], &[], &[]);
    };
    let lookup_len = (*control as usize) * rest.len() / 256;
    let (lookup, rest) = rest.split_at(lookup_len.min(rest.len()));
    // files.bin is the least interesting section here, so give it a slice small
    // enough that most of the budget stays on the lookup/postings pair.
    let files_len = rest.len() / 8;
    let (postings, files) = rest.split_at(rest.len() - files_len);
    (lookup, postings, files)
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let dir = index_dir();
    let (lookup, postings, files) = split(data);
    std::fs::write(dir.join("lookup.bin"), lookup).expect("write lookup.bin");
    std::fs::write(dir.join("index.bin"), postings).expect("write index.bin");
    std::fs::write(dir.join("files.bin"), files).expect("write files.bin");

    // `open` rejects the structurally impossible (misaligned lookup.bin,
    // truncated or non-dense files.bin) with `Error::IndexCorrupted`. Whatever
    // it accepts must then survive arbitrary queries.
    let Ok(reader) = IndexReader::open(&dir) else {
        return;
    };

    // A pure diagnostic: reports corruption as an error, never by panicking.
    let _ = reader.validate_lookup();

    let max_decodable = postings.len() / POSTING_ENTRY_SIZE;
    for i in 0..reader.num_trigrams().min(MAX_ENTRIES_DECODED) {
        let (trigram, entries) = reader.trigram_posting_at(i);
        // Every entry returned must have been decoded from inside index.bin,
        // so the count is bounded by the file — never by the `length` field.
        assert!(
            entries.len() <= max_decodable,
            "entry {i} decoded {} postings from a {}-byte index.bin",
            entries.len(),
            postings.len()
        );
        // The zero-copy path must agree about what is in range.
        if let Some((_, raw)) = reader.nth_trigram_raw(i) {
            assert!(raw.len() <= postings.len());
        }
        // Resolving the same trigram through binary search must also be safe.
        let _ = reader.lookup_trigram_with_masks(trigram);
    }

    // Misses have to be clean too, including on an unsorted table where the
    // binary search walks arbitrary entries.
    for probe in [0u32, 1, u32::MAX / 2, u32::MAX] {
        let _ = reader.lookup_trigram(probe);
    }
});
