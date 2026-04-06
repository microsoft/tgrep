#![no_main]

use libfuzzer_sys::fuzz_target;
use tgrep_core::PostingEntry;

fuzz_target!(|data: &[u8]| {
    // Fuzz files.bin decoding — must not panic on arbitrary bytes.
    // decode_file_entries is pub(crate), so we exercise it indirectly
    // by testing the public PostingEntry encode/decode roundtrip and
    // ensuring no panics on truncated/malformed data.

    // PostingEntry roundtrip: any 6 bytes should decode without panic.
    if data.len() >= 6 {
        let buf: &[u8; 6] = data[..6].try_into().unwrap();
        let entry = PostingEntry::decode(buf);
        let re_encoded = entry.encode();
        let re_decoded = PostingEntry::decode(&re_encoded);
        assert_eq!(entry.file_id, re_decoded.file_id);
        assert_eq!(entry.loc_mask, re_decoded.loc_mask);
        assert_eq!(entry.next_mask, re_decoded.next_mask);
    }

    // Trigram extraction + roundtrip: extract trigrams from fuzzed data,
    // then verify each hash encodes/decodes consistently.
    let hashes = tgrep_core::trigram::extract(data);
    for h in hashes {
        let a = ((h >> 16) & 0xFF) as u8;
        let b = ((h >> 8) & 0xFF) as u8;
        let c = (h & 0xFF) as u8;
        assert_eq!(tgrep_core::trigram::hash(a, b, c), h);
    }
});
