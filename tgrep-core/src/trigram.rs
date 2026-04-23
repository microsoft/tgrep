/// Trigram extraction and hashing.
///
/// A trigram is every overlapping 3-byte window in a byte sequence.
/// We pack 3 bytes into a `u32`: `(a << 16) | (b << 8) | c`.
/// This gives us up to ~16.7M unique trigrams with zero collisions.
use std::collections::{HashMap, HashSet};

pub type TrigramHash = u32;

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
#[derive(Debug, Clone, Copy, Default)]
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
    let mut masks: HashMap<TrigramHash, TrigramMasks> = HashMap::new();
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

    order
        .into_iter()
        .map(|h| (h, masks.remove(&h).unwrap()))
        .collect()
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
pub fn extract_merged_masks(content: &[u8]) -> HashMap<TrigramHash, TrigramMasks> {
    let tri_masks = extract_with_masks(content);

    let mut per_tri: HashMap<TrigramHash, TrigramMasks> = HashMap::new();
    for &(tri, m) in tri_masks.iter() {
        let entry = per_tri.entry(tri).or_default();
        entry.loc_mask |= m.loc_mask;
        entry.next_mask |= m.next_mask;
    }

    let lower = content.to_ascii_lowercase();
    if lower != content {
        let lower_tri_masks = extract_with_masks(&lower);
        for &(tri, m) in lower_tri_masks.iter() {
            let entry = per_tri.entry(tri).or_default();
            entry.loc_mask |= m.loc_mask;
            entry.next_mask |= m.next_mask;
        }
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
}
