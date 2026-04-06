#![no_main]

use libfuzzer_sys::fuzz_target;
use tgrep_core::trigram;

fuzz_target!(|data: &[u8]| {
    // Fuzz trigram extraction — must not panic on any input.
    let hashes = trigram::extract(data);

    // Every extracted hash should be a valid 24-bit value.
    for h in &hashes {
        assert!(*h <= 0x00FF_FFFF);
    }

    // extract_with_masks should produce the same trigram set.
    let with_masks = trigram::extract_with_masks(data);
    let mask_hashes: Vec<u32> = with_masks.iter().map(|(h, _)| *h).collect();
    assert_eq!(hashes, mask_hashes);

    // is_binary should not panic.
    let _ = trigram::is_binary(data);
});
