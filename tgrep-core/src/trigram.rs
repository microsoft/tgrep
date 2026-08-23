/// Trigram extraction and hashing.
///
/// A trigram is every overlapping 3-byte window in a byte sequence.
/// We pack 3 bytes into a `u32`: `(a << 16) | (b << 8) | c`.
/// This gives us up to ~16.7M unique trigrams with zero collisions.
use std::collections::{HashMap, HashSet};

pub type TrigramHash = u32;

/// Hasher for trigram-keyed maps.
///
/// A trigram key *is* its own hash: packing three bytes into 24 bits is
/// injective, so there is nothing for a cryptographic hash to protect against.
/// The default SipHash is not free, though, and extraction hashes once per
/// input byte — twice for a file containing any uppercase — which put it
/// directly on the critical path of every index build.
///
/// One multiply-xorshift replaces it. The xorshift is not optional: hashbrown
/// takes the bucket index from the *low* bits of the hash, and the low `k` bits
/// of `value * K` depend only on the low `k` bits of `value`, which for a
/// trigram is its last byte alone. Folding the high half down mixes all 24 bits
/// into both the bucket index and the top-7-bit control byte.
#[derive(Default, Clone, Copy)]
pub struct TrigramHasher(u64);

impl std::hash::Hasher for TrigramHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    /// Only reachable if a key type other than `u32` is ever used with this
    /// hasher; kept correct rather than fast.
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        let mixed = u64::from(value).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = mixed ^ (mixed >> 32);
    }
}

/// [`std::hash::BuildHasher`] for [`TrigramHasher`].
pub type BuildTrigramHasher = std::hash::BuildHasherDefault<TrigramHasher>;

/// A file's trigrams with their merged masks.
pub type TrigramMaskMap = HashMap<TrigramHash, TrigramMasks, BuildTrigramHasher>;

/// Pack three bytes into a single u32 trigram hash.
#[inline]
pub fn hash(a: u8, b: u8, c: u8) -> TrigramHash {
    (a as u32) << 16 | (b as u32) << 8 | c as u32
}

/// Hash a byte offset into a bit position in an 8-bit loc_mask.
/// Uses `offset % 8` so consecutive offsets map to adjacent bits,
/// enabling rotate-and-AND adjacency checks.
#[inline]
fn loc_bit(offset: usize) -> u8 {
    1u8 << (offset % 8)
}

/// Map a byte to one of 8 Bloom bits for next_mask.
/// Uses a multiplicative hash to spread ASCII characters evenly.
#[inline]
fn next_bit(byte: u8) -> u8 {
    1u8 << (byte.wrapping_mul(0x9E) >> 5 & 0x07)
}

/// Compute the Bloom filter bit for a byte (public, for query-time checks).
#[inline]
pub fn bloom_hash(byte: u8) -> u8 {
    next_bit(byte)
}

/// Per-trigram masks for a single file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrigramMasks {
    /// Positional mask: bit i is set if the trigram occurs at offset where offset % 8 == i.
    pub loc_mask: u8,
    /// 8-bit Bloom filter of bytes that immediately follow this trigram in the file.
    pub next_mask: u8,
}

/// Extract all unique trigrams from a byte slice.
pub fn extract(data: &[u8]) -> Vec<TrigramHash> {
    if data.len() < 3 {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for window in data.windows(3) {
        let h = hash(window[0], window[1], window[2]);
        if seen.insert(h) {
            result.push(h);
        }
    }
    result
}

/// Extract all unique trigrams with positional and next-byte masks.
///
/// For each unique trigram, computes:
/// - `loc_mask`: positional mask (offset % 8) for adjacency checks
/// - `next_mask`: Bloom filter of bytes that immediately follow this trigram
pub fn extract_with_masks(data: &[u8]) -> Vec<(TrigramHash, TrigramMasks)> {
    if data.len() < 3 {
        return Vec::new();
    }

    // Use HashMap instead of 16M arrays — much less allocation pressure
    // since typical files have far fewer than 16M unique trigrams.
    let mut masks = TrigramMaskMap::default();
    let mut order: Vec<TrigramHash> = Vec::new();

    for (i, window) in data.windows(3).enumerate() {
        let h = hash(window[0], window[1], window[2]);
        let entry = masks.entry(h).or_insert_with(|| {
            order.push(h);
            TrigramMasks::default()
        });
        entry.loc_mask |= loc_bit(i);
        if i + 3 < data.len() {
            entry.next_mask |= next_bit(data[i + 3]);
        }
    }

    order.into_iter().map(|h| (h, masks[&h])).collect()
}

/// Check whether consecutive trigrams from a literal can be adjacent based on masks.
///
/// For trigrams at offsets i and i+1 in a literal, rotating the first
/// trigram's loc_mask left by 1 bit and AND'ing with the second's loc_mask
/// should be non-zero if they appear adjacently in the file.
pub fn check_adjacency(masks: &[(TrigramHash, TrigramMasks)]) -> bool {
    if masks.len() <= 1 {
        return true;
    }
    for pair in masks.windows(2) {
        let prev_loc = pair[0].1.loc_mask;
        let next_loc = pair[1].1.loc_mask;
        // Rotate prev_loc left by 1 bit within 8-bit space
        let rotated = prev_loc.rotate_left(1);
        if rotated & next_loc == 0 {
            return false;
        }
    }
    true
}

/// Check whether a trigram's next_mask is compatible with an expected next byte.
pub fn check_next_byte(masks: &TrigramMasks, next_byte: u8) -> bool {
    masks.next_mask & next_bit(next_byte) != 0
}

/// Extract trigrams with masks from both original and lowercased content,
/// merging masks per trigram. This is the standard extraction used by both
/// the on-disk builder and the live index overlay.
pub fn extract_merged_masks(content: &[u8]) -> TrigramMaskMap {
    let mut per_tri = TrigramMaskMap::default();
    if content.len() < 3 {
        return per_tri;
    }

    // Folding the case conversion into the window rather than materialising a
    // lowercased duplicate of the file keeps peak memory independent of file
    // size, which matters because the builder holds several whole files in
    // flight at once.
    //
    // This is equivalent to extracting from a lowercased copy: ASCII
    // lowercasing is 1:1 on bytes, so every offset — and therefore every
    // `loc_bit` — is unchanged, and `|=` is associative, so merging per window
    // lands on the same masks as merging two completed sets.
    //
    // Both cases are folded into the *same* pass rather than run as two passes
    // over the file. Most windows in real source are already lowercase, so
    // their lowered trigram is the same key; recognising that collapses the
    // second pass's hash and probe into a single extra `|=` on an entry that is
    // already in hand, and leaves genuine work only for windows that actually
    // contain an uppercase byte.
    let has_upper = content.iter().any(|byte| byte.is_ascii_uppercase());
    let len = content.len();

    for (i, window) in content.windows(3).enumerate() {
        let trigram = hash(window[0], window[1], window[2]);
        let loc = loc_bit(i);
        let next = if i + 3 < len {
            next_bit(content[i + 3])
        } else {
            0
        };

        if !has_upper {
            // No uppercase anywhere means the lowercased pass would recompute
            // this identical entry for every window.
            let entry = per_tri.entry(trigram).or_default();
            entry.loc_mask |= loc;
            entry.next_mask |= next;
            continue;
        }

        let lowered = hash(
            window[0].to_ascii_lowercase(),
            window[1].to_ascii_lowercase(),
            window[2].to_ascii_lowercase(),
        );
        // The lowercased pass also lowercases the *following* byte, so an
        // otherwise-lowercase window followed by an uppercase byte still
        // contributes a second next_mask bit to the same entry.
        let next_lowered = if i + 3 < len {
            next_bit(content[i + 3].to_ascii_lowercase())
        } else {
            0
        };

        let entry = per_tri.entry(trigram).or_default();
        entry.loc_mask |= loc;
        if lowered == trigram {
            entry.next_mask |= next | next_lowered;
            continue;
        }
        entry.next_mask |= next;

        let lowered_entry = per_tri.entry(lowered).or_default();
        lowered_entry.loc_mask |= loc;
        lowered_entry.next_mask |= next_lowered;
    }

    per_tri
}

/// Extract trigrams from a string pattern (for query planning).
pub fn extract_from_literal(s: &str) -> Vec<TrigramHash> {
    extract(s.as_bytes())
}

/// Check if a file is likely binary by scanning the first 8KB for NUL bytes.
pub fn is_binary(data: &[u8]) -> bool {
    let check_len = data.len().min(8192);
    data[..check_len].contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_packing() {
        assert_eq!(hash(b't', b'h', b'e'), 0x746865);
        assert_eq!(hash(0, 0, 0), 0);
        assert_eq!(hash(0xFF, 0xFF, 0xFF), 0x00FFFFFF);
    }

    #[test]
    fn test_extract_basic() {
        let trigrams = extract(b"the cat");
        // "the", "he ", "e c", " ca", "cat"
        assert_eq!(trigrams.len(), 5);
        assert!(trigrams.contains(&hash(b't', b'h', b'e')));
        assert!(trigrams.contains(&hash(b'c', b'a', b't')));
    }

    #[test]
    fn test_extract_short() {
        assert!(extract(b"ab").is_empty());
        assert!(extract(b"").is_empty());
    }

    #[test]
    fn test_extract_dedup() {
        // "aaa" has trigram "aaa" appearing twice, but should be deduped
        let trigrams = extract(b"aaaa");
        assert_eq!(trigrams.len(), 1);
    }

    #[test]
    fn test_is_binary() {
        assert!(!is_binary(b"hello world"));
        assert!(is_binary(b"hello\0world"));
    }

    /// `extract_merged_masks` folds the lowercase pass into the window instead
    /// of materialising a lowercased copy of the file. Pin it against the
    /// formulation it replaced, which is the definition of what it must produce.
    fn merged_masks_via_materialised_copy(content: &[u8]) -> TrigramMaskMap {
        let mut expected = TrigramMaskMap::default();
        for (tri, m) in extract_with_masks(content) {
            expected.insert(tri, m);
        }
        let lower = content.to_ascii_lowercase();
        for (tri, m) in extract_with_masks(&lower) {
            let entry = expected.entry(tri).or_default();
            entry.loc_mask |= m.loc_mask;
            entry.next_mask |= m.next_mask;
        }
        expected
    }

    #[test]
    fn merged_masks_match_a_materialised_lowercase_pass() {
        // Mixed case, repeats across the 8-byte `loc_mask` period, non-ASCII,
        // and a trailing window whose next byte does not exist.
        for content in [
            b"Foo BAR foo Baz QUX quux Foo".as_slice(),
            b"AAAAAAAAAAAAAAAAA".as_slice(),
            b"MixedCase\nMIXEDCASE\nmixedcase\r\n".as_slice(),
            b"\xc3\x9cber STRASSE \xc3\xbcber strasse".as_slice(),
            b"ABC".as_slice(),
            b"ab".as_slice(),
            b"".as_slice(),
            b"no uppercase here at all".as_slice(),
            // An all-lowercase window whose *next* byte is uppercase. The
            // lowercased pass folds onto the same key, so its next_mask bit has
            // to reach the entry the original pass already created.
            b"abcD abcd".as_slice(),
            b"zzzZzzz".as_slice(),
        ] {
            assert_eq!(
                extract_merged_masks(content),
                merged_masks_via_materialised_copy(content),
                "mismatch for {:?}",
                String::from_utf8_lossy(content)
            );
        }
    }

    #[test]
    fn merged_masks_cover_both_cases_of_a_trigram() {
        let merged = extract_merged_masks(b"Foo");
        // The literal bytes and their lowercased form are both present, so a
        // case-insensitive query planned on either spelling finds the file.
        assert!(merged.contains_key(&hash(b'F', b'o', b'o')));
        assert!(merged.contains_key(&hash(b'f', b'o', b'o')));
    }

    #[test]
    fn test_extract_with_masks_basic() {
        let results = extract_with_masks(b"abcde");
        // Trigrams: "abc", "bcd", "cde" → 3 unique
        assert_eq!(results.len(), 3);
        let abc = results
            .iter()
            .find(|(h, _)| *h == hash(b'a', b'b', b'c'))
            .unwrap();
        // "abc" is followed by 'd'
        assert!(check_next_byte(&abc.1, b'd'));
    }

    #[test]
    fn test_extract_with_masks_short() {
        assert!(extract_with_masks(b"ab").is_empty());
        assert!(extract_with_masks(b"").is_empty());
    }

    #[test]
    fn test_next_mask_filters_false_positive() {
        // File contains "abcXe" — trigram "abc" is followed by 'X', not 'd'
        let results = extract_with_masks(b"abcXe");
        let abc = results
            .iter()
            .find(|(h, _)| *h == hash(b'a', b'b', b'c'))
            .unwrap();
        // 'X' should be in the mask
        assert!(check_next_byte(&abc.1, b'X'));
        // 'd' should NOT be in the mask (different bloom_hash bit)
        // bloom_hash('X'=88): 88*0x9E=0x3530, >>5=0x1A9, &7=1 → bit 1
        // bloom_hash('d'=100): 100*0x9E=0x3E18, >>5=0x1F0, &7=0 → bit 0
        assert!(!check_next_byte(&abc.1, b'd'));
    }

    #[test]
    fn test_loc_mask_nonzero() {
        let results = extract_with_masks(b"hello world");
        for (_, masks) in &results {
            assert_ne!(
                masks.loc_mask, 0,
                "loc_mask should have at least one bit set"
            );
        }
    }

    #[test]
    fn test_extract_merged_masks_ors_across_cases() {
        // "Hello" produces trigrams for original ("Hel","ell","llo") and
        // lowercase ("hel","ell","llo"). "ell"/"llo" appear in both passes
        // so their masks should be OR'd together. The lowercase pass adds
        // the new trigram "hel".
        let merged = extract_merged_masks(b"Hello");

        // "hel" only comes from lowercase pass
        let hel = hash(b'h', b'e', b'l');
        assert!(
            merged.contains_key(&hel),
            "should contain lowercase trigram 'hel'"
        );

        // "Hel" only comes from original pass
        let big_hel = hash(b'H', b'e', b'l');
        assert!(
            merged.contains_key(&big_hel),
            "should contain original trigram 'Hel'"
        );

        // "ell" appears in both passes — masks should be superset
        let ell = hash(b'e', b'l', b'l');
        let merged_ell = merged.get(&ell).unwrap();

        let orig_masks = extract_with_masks(b"Hello");
        let orig_ell = orig_masks
            .iter()
            .find(|(h, _)| *h == ell)
            .map(|(_, m)| *m)
            .unwrap();

        let lower_masks = extract_with_masks(b"hello");
        let lower_ell = lower_masks
            .iter()
            .find(|(h, _)| *h == ell)
            .map(|(_, m)| *m)
            .unwrap();

        assert_eq!(
            merged_ell.loc_mask,
            orig_ell.loc_mask | lower_ell.loc_mask,
            "loc_mask should be OR of both passes"
        );
        assert_eq!(
            merged_ell.next_mask,
            orig_ell.next_mask | lower_ell.next_mask,
            "next_mask should be OR of both passes"
        );
    }

    #[test]
    fn test_extract_merged_masks_all_lowercase_no_dup() {
        // All-lowercase input: lowercase pass should be skipped (content == lower).
        // Result should match a single extract_with_masks pass.
        let merged = extract_merged_masks(b"abcde");
        let single = extract_with_masks(b"abcde");

        assert_eq!(merged.len(), single.len());
        for &(tri, masks) in &single {
            let m = merged.get(&tri).unwrap();
            assert_eq!(m.loc_mask, masks.loc_mask);
            assert_eq!(m.next_mask, masks.next_mask);
        }
    }

    #[test]
    fn test_extract_merged_masks_matches_lowercase_copy_semantics() {
        let content = b"AbCDef abcDEF\nXYZ xyz";
        let mut expected = HashMap::new();
        for (tri, masks) in extract_with_masks(content) {
            expected.insert(tri, masks);
        }
        let lower = content.to_ascii_lowercase();
        for (tri, masks) in extract_with_masks(&lower) {
            let entry = expected.entry(tri).or_insert_with(TrigramMasks::default);
            entry.loc_mask |= masks.loc_mask;
            entry.next_mask |= masks.next_mask;
        }

        let merged = extract_merged_masks(content);
        assert_eq!(merged.len(), expected.len());
        for (tri, expected_masks) in expected {
            let actual = merged
                .get(&tri)
                .unwrap_or_else(|| panic!("missing trigram {tri:#010x}"));
            assert_eq!(actual.loc_mask, expected_masks.loc_mask);
            assert_eq!(actual.next_mask, expected_masks.next_mask);
        }
    }
}
