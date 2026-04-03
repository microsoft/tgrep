/// Trigram extraction and hashing.
///
/// A trigram is every overlapping 3-byte window in a byte sequence.
/// We pack 3 bytes into a `u32`: `(a << 16) | (b << 8) | c`.
/// This gives us up to ~16.7M unique trigrams with zero collisions.
pub type TrigramHash = u32;

/// Pack three bytes into a single u32 trigram hash.
#[inline]
pub fn hash(a: u8, b: u8, c: u8) -> TrigramHash {
    (a as u32) << 16 | (b as u32) << 8 | c as u32
}

/// Extract all unique trigrams from a byte slice.
pub fn extract(data: &[u8]) -> Vec<TrigramHash> {
    if data.len() < 3 {
        return Vec::new();
    }
    let mut seen = vec![false; 1 << 24]; // 16MB bitmap — faster than HashSet
    let mut result = Vec::new();
    for window in data.windows(3) {
        let h = hash(window[0], window[1], window[2]);
        if !seen[h as usize] {
            seen[h as usize] = true;
            result.push(h);
        }
    }
    result
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
}
