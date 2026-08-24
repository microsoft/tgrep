//! Text decoding for `-E/--encoding`, mirroring ripgrep's behaviour.
//!
//! ripgrep's default is `auto`, which sniffs a UTF-8 or UTF-16 byte-order mark
//! and transcodes accordingly; no other detection is attempted. `none` disables
//! sniffing entirely, and an explicit label names a WHATWG encoding.

use std::borrow::Cow;

use anyhow::{Result, bail};
use encoding_rs::Encoding;

/// How raw file bytes are turned into searchable text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EncodingMode {
    /// Sniff a UTF-8/UTF-16 BOM, otherwise treat the bytes as UTF-8.
    #[default]
    Auto,
    /// `-E none`: no BOM sniffing, the raw bytes are searched as-is.
    None,
    /// `-E <label>`: a named encoding. A BOM still wins, matching ripgrep,
    /// whose decoder leaves `bom_override` disabled.
    Explicit(&'static Encoding),
}

impl EncodingMode {
    /// Whether text decoded in this mode can differ from what the index was
    /// built with.
    ///
    /// The index is always built in `Auto` mode, so any other mode makes the
    /// trigram plan unsound: the matcher would see different bytes than the
    /// ones that produced the postings. Callers use this to fall back to
    /// scanning every candidate.
    pub fn may_differ_from_index(self) -> bool {
        self != EncodingMode::Auto
    }

    /// The label this mode was built from, for diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            EncodingMode::Auto => "auto",
            EncodingMode::None => "none",
            EncodingMode::Explicit(enc) => enc.name(),
        }
    }
}

/// Parse an `-E/--encoding` value. Accepts `auto`, `none`, or any label the
/// WHATWG Encoding Standard recognises (`utf-16le`, `sjis`, `latin1`, ...).
pub fn parse_encoding(label: &str) -> Result<EncodingMode> {
    let label = label.trim();
    match label {
        "" | "auto" => Ok(EncodingMode::Auto),
        "none" => Ok(EncodingMode::None),
        _ => match Encoding::for_label(label.as_bytes()) {
            Some(enc) => Ok(EncodingMode::Explicit(enc)),
            None => bail!("unsupported encoding: {label}"),
        },
    }
}

/// Decode `bytes` into searchable bytes, borrowing when no transcoding is
/// needed. Plain UTF-8 input — by far the common case — is never copied.
pub fn decode_bytes(bytes: &[u8], mode: EncodingMode) -> Cow<'_, [u8]> {
    let (enc, body) = match mode {
        EncodingMode::None => return Cow::Borrowed(bytes),
        EncodingMode::Auto => match Encoding::for_bom(bytes) {
            Some((enc, bom_len)) => (enc, &bytes[bom_len..]),
            None => return Cow::Borrowed(bytes),
        },
        EncodingMode::Explicit(enc) => match Encoding::for_bom(bytes) {
            Some((bom_enc, bom_len)) => (bom_enc, &bytes[bom_len..]),
            None => (enc, bytes),
        },
    };
    if enc == encoding_rs::UTF_8 {
        // Nothing to transcode; the BOM has already been trimmed off.
        return Cow::Borrowed(body);
    }
    Cow::Owned(transcode(enc, body).into_bytes())
}

/// Decode `bytes` into searchable text.
pub fn decode(bytes: &[u8], mode: EncodingMode) -> String {
    decode_with_fixups(bytes, mode).0
}

/// Decode `bytes` and record where invalid input was replaced.
///
/// Callers that report byte offsets or columns need [`LossyFixups`] to map an
/// offset in the decoded text back to one in the file, since every repaired
/// byte turns into a three-byte `U+FFFD`.
pub fn decode_with_fixups(bytes: &[u8], mode: EncodingMode) -> (String, LossyFixups) {
    match decode_bytes(bytes, mode) {
        Cow::Borrowed(body) => lossy_utf8(body),
        // Transcoded output is always valid UTF-8, and its offsets bear no
        // relation to the source bytes anyway, so there is nothing to map.
        Cow::Owned(body) => (
            String::from_utf8(body)
                .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()),
            LossyFixups::default(),
        ),
    }
}

/// Decode owned `bytes`, reusing the buffer instead of copying it.
///
/// [`decode_with_fixups`] borrows its input, so it has to copy the whole file
/// to produce a `String`. Searching reads entire files, which made that copy a
/// second full-size allocation on top of the read. Taking ownership lets plain
/// UTF-8 — very nearly every source file — be converted in place.
pub fn decode_owned_with_fixups(bytes: Vec<u8>, mode: EncodingMode) -> (String, LossyFixups) {
    if !borrows_whole_input(&bytes, mode) {
        return decode_with_fixups(&bytes, mode);
    }
    match String::from_utf8(bytes) {
        Ok(text) => (text, LossyFixups::default()),
        Err(err) => lossy_utf8(err.as_bytes()),
    }
}

/// Whether [`decode_bytes`] would hand back the entire input untouched.
///
/// That is the only case where the caller's buffer is exactly the bytes to be
/// searched and can therefore be reused; a trimmed BOM or a transcode means the
/// result is a different sequence of bytes.
///
/// Public because it is also the precondition for searching a file straight out
/// of a memory map: if decoding would rewrite the bytes, the mapped pages are
/// not the bytes to search and the file has to be read and decoded instead.
pub fn borrows_whole_input(bytes: &[u8], mode: EncodingMode) -> bool {
    match mode {
        EncodingMode::None => true,
        EncodingMode::Auto => Encoding::for_bom(bytes).is_none(),
        EncodingMode::Explicit(enc) => {
            Encoding::for_bom(bytes).is_none() && enc == encoding_rs::UTF_8
        }
    }
}

/// Where lossy UTF-8 decoding replaced invalid bytes with `U+FFFD`.
///
/// ripgrep reports columns and `--byte-offset` in terms of the bytes on disk.
/// tgrep searches repaired text, where each replacement is three bytes wide no
/// matter how many bytes it stood in for, so offsets have to be mapped back.
#[derive(Debug, Default, Clone)]
pub struct LossyFixups {
    /// `(offset of the replacement in the decoded text, total bytes gained up
    /// to and including it)`, ascending by offset.
    shifts: Vec<(usize, usize)>,
}

impl LossyFixups {
    pub fn is_empty(&self) -> bool {
        self.shifts.is_empty()
    }

    /// Heap bytes held by this fixup table.
    ///
    /// A file of mostly-invalid bytes produces one shift per replacement, so
    /// this can rival the decoded text itself. Callers that budget memory (the
    /// `serve` content cache) must count it.
    pub fn heap_bytes(&self) -> u64 {
        (self.shifts.capacity() * std::mem::size_of::<(usize, usize)>()) as u64
    }

    /// Map an offset in the decoded text to the corresponding offset in the
    /// source bytes.
    ///
    /// An offset may land *inside* a `U+FFFD` rather than on its boundary: the
    /// repaired text is what gets searched, so a pattern can match starting
    /// part-way through the replacement. Such an offset is clamped to the
    /// replacement's own source position, because there is no finer-grained
    /// answer and because subtracting a gain the offset has not yet realised
    /// would underflow.
    pub fn to_source_offset(&self, decoded: usize) -> usize {
        if self.shifts.is_empty() {
            return decoded;
        }
        let i = self.shifts.partition_point(|&(at, _)| at < decoded);
        if i == 0 {
            return decoded;
        }
        let (at, gained) = self.shifts[i - 1];
        let gained_before = if i >= 2 { self.shifts[i - 2].1 } else { 0 };
        if decoded < at + REPLACEMENT_LEN {
            return at - gained_before;
        }
        decoded - gained
    }
}

/// Byte length of `U+FFFD`, the character lossy decoding substitutes in.
const REPLACEMENT_LEN: usize = '\u{FFFD}'.len_utf8();

/// Decode using the same rules the index builder uses.
///
/// Indexes are always built in [`EncodingMode::Auto`]; see
/// [`EncodingMode::may_differ_from_index`].
///
/// Invalid UTF-8 is repaired here for the same reason searches repair it: a
/// search reads text where every invalid sequence has become `U+FFFD`, so the
/// index has to hold those same bytes. Otherwise a pattern containing `U+FFFD`
/// — which is three bytes, and so perfectly indexable — would find no candidate
/// postings for the file and the indexed search would silently miss a match the
/// brute-force path reports.
pub fn decode_for_index(bytes: &[u8]) -> Cow<'_, [u8]> {
    match decode_bytes(bytes, EncodingMode::Auto) {
        // Transcoded output is already valid UTF-8.
        owned @ Cow::Owned(_) => owned,
        Cow::Borrowed(body) => {
            if std::str::from_utf8(body).is_ok() {
                return Cow::Borrowed(body);
            }
            // A file that will be rejected as binary is not worth repairing,
            // and repairing it first could push a NUL past the window
            // `is_binary` looks at, changing how it is classified.
            if crate::trigram::is_binary(body) {
                return Cow::Borrowed(body);
            }
            // `lossy_utf8_into` is the same repair the search path uses, so
            // this produces the exact bytes a later search will match against.
            // The fixups it can record are for mapping match offsets back to
            // the original file, which indexing has no use for.
            Cow::Owned(lossy_utf8_into(body, None).into_bytes())
        }
    }
}

fn transcode(enc: &'static Encoding, bytes: &[u8]) -> String {
    // The BOM has already been consumed by the caller, so decoding must not
    // sniff for one again — a stray BOM-looking sequence mid-file is data.
    let (decoded, _had_errors) = enc.decode_without_bom_handling(bytes);
    decoded.into_owned()
}

/// `String::from_utf8_lossy`, but recording each repair.
///
/// The loop mirrors the standard library's: one `U+FFFD` per maximal invalid
/// subsequence, so the text produced here is identical.
fn lossy_utf8(bytes: &[u8]) -> (String, LossyFixups) {
    let mut shifts = Vec::new();
    let out = lossy_utf8_into(bytes, Some(&mut shifts));
    (out, LossyFixups { shifts })
}

/// The repair loop, shared so the two callers cannot drift apart.
///
/// `shifts` is `None` for indexing, which only needs the repaired bytes. That
/// is not just a micro-optimisation: one entry is recorded per repair, so a
/// large file of single-byte-invalid text (a 135 MB Latin-1 log, say) builds
/// tens of megabytes of offset table that indexing then drops on the floor.
///
/// The output is measured before it is filled. A `String` that starts at
/// `bytes.len()` is *almost* big enough, so the first repair to overflow it
/// triggers a doubling reallocation — turning a 135 MB decode into a 270 MB
/// buffer with both halves live during the copy. Sizing it exactly costs one
/// extra validation pass, which is SIMD-fast and far cheaper than that copy.
fn lossy_utf8_into(bytes: &[u8], shifts: Option<&mut Vec<(usize, usize)>>) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }

    let (out_len, repairs) = lossy_utf8_shape(bytes);
    let mut out = String::with_capacity(out_len);
    let mut shifts = shifts;
    if let Some(shifts) = shifts.as_deref_mut() {
        shifts.reserve_exact(repairs);
    }

    let mut rest = bytes;
    let mut gained = 0usize;
    loop {
        let err = match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => e,
        };
        let valid = err.valid_up_to();
        out.push_str(std::str::from_utf8(&rest[..valid]).unwrap_or_default());
        // `None` means the input ended mid-sequence: everything left is one
        // replacement and there is nothing after it.
        let replaced = err.error_len().unwrap_or(rest.len() - valid);
        if let Some(shifts) = shifts.as_deref_mut() {
            gained += REPLACEMENT_LEN - replaced.min(REPLACEMENT_LEN);
            shifts.push((out.len(), gained));
        }
        out.push(char::REPLACEMENT_CHARACTER);
        match err.error_len() {
            Some(n) => rest = &rest[valid + n..],
            None => break,
        }
    }
    debug_assert_eq!(out.len(), out_len);
    out
}

/// `(exact output length, number of replacements)` for [`lossy_utf8_into`].
///
/// Walks the input exactly as the repair loop does, so the two agree by
/// construction; a `debug_assert` in the repair loop pins that agreement.
fn lossy_utf8_shape(bytes: &[u8]) -> (usize, usize) {
    let mut rest = bytes;
    let mut len = 0usize;
    let mut repairs = 0usize;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                len += s.len();
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                len += valid + REPLACEMENT_LEN;
                repairs += 1;
                match e.error_len() {
                    Some(n) => rest = &rest[valid + n..],
                    None => break,
                }
            }
        }
    }
    (len, repairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le_with_bom(s: &str) -> Vec<u8> {
        let mut v = vec![0xFF, 0xFE];
        for unit in s.encode_utf16() {
            v.extend_from_slice(&unit.to_le_bytes());
        }
        v
    }

    fn utf16be_with_bom(s: &str) -> Vec<u8> {
        let mut v = vec![0xFE, 0xFF];
        for unit in s.encode_utf16() {
            v.extend_from_slice(&unit.to_be_bytes());
        }
        v
    }

    #[test]
    fn parses_special_labels() {
        assert_eq!(parse_encoding("auto").unwrap(), EncodingMode::Auto);
        assert_eq!(parse_encoding("").unwrap(), EncodingMode::Auto);
        assert_eq!(parse_encoding("none").unwrap(), EncodingMode::None);
    }

    #[test]
    fn parses_whatwg_labels() {
        assert!(matches!(
            parse_encoding("utf-16le").unwrap(),
            EncodingMode::Explicit(_)
        ));
        assert!(matches!(
            parse_encoding("sjis").unwrap(),
            EncodingMode::Explicit(_)
        ));
        assert!(matches!(
            parse_encoding("latin1").unwrap(),
            EncodingMode::Explicit(_)
        ));
    }

    #[test]
    fn rejects_unknown_labels() {
        let err = parse_encoding("definitely-not-an-encoding")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsupported encoding"), "got {err}");
    }

    #[test]
    fn auto_decodes_utf16le_bom() {
        let bytes = utf16le_with_bom("hello needle\n");
        assert_eq!(decode(&bytes, EncodingMode::Auto), "hello needle\n");
    }

    #[test]
    fn auto_decodes_utf16be_bom() {
        let bytes = utf16be_with_bom("hello needle\n");
        assert_eq!(decode(&bytes, EncodingMode::Auto), "hello needle\n");
    }

    #[test]
    fn auto_strips_utf8_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"hello needle\n");
        assert_eq!(decode(&bytes, EncodingMode::Auto), "hello needle\n");
    }

    #[test]
    fn none_keeps_raw_bytes_including_bom() {
        let bytes = utf16le_with_bom("hi");
        let out = decode(&bytes, EncodingMode::None);
        // Raw UTF-16 bytes are not valid UTF-8, so this stays mangled — which
        // is exactly what `-E none` promises.
        assert_ne!(out, "hi");
        assert!(out.contains('\u{0}') || out.contains('\u{FFFD}'));
    }

    #[test]
    fn explicit_encoding_decodes_bomless_input() {
        // UTF-16LE without a BOM is invisible to auto-detection.
        let mut bytes = Vec::new();
        for unit in "needle".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_ne!(decode(&bytes, EncodingMode::Auto), "needle");
        let mode = parse_encoding("utf-16le").unwrap();
        assert_eq!(decode(&bytes, mode), "needle");
    }

    #[test]
    fn bom_wins_over_explicit_encoding() {
        // A UTF-8 BOM must not be decoded as Shift-JIS just because -E said so.
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("needle".as_bytes());
        let mode = parse_encoding("sjis").unwrap();
        assert_eq!(decode(&bytes, mode), "needle");
    }

    #[test]
    fn latin1_round_trips_high_bytes() {
        let mode = parse_encoding("latin1").unwrap();
        assert_eq!(decode(b"caf\xe9", mode), "café");
    }

    #[test]
    fn plain_utf8_is_unchanged_in_every_mode() {
        for mode in [
            EncodingMode::Auto,
            EncodingMode::None,
            parse_encoding("utf-8").unwrap(),
        ] {
            assert_eq!(decode(b"plain text\n", mode), "plain text\n");
        }
    }

    #[test]
    fn invalid_utf8_is_lossy_not_dropped() {
        let out = decode(b"caf\xe9 x", EncodingMode::Auto);
        assert!(out.contains("caf"), "got {out}");
        assert!(out.contains(" x"), "got {out}");
    }

    #[test]
    fn only_auto_matches_the_index() {
        assert!(!EncodingMode::Auto.may_differ_from_index());
        assert!(EncodingMode::None.may_differ_from_index());
        assert!(parse_encoding("utf-16le").unwrap().may_differ_from_index());
    }

    #[test]
    fn index_sees_the_same_repaired_bytes_a_search_does() {
        // The trigrams of an invalid sequence have to be the U+FFFD ones, or a
        // pattern containing U+FFFD selects no candidates and the indexed
        // search misses a file the brute-force path reports.
        let raw = b"bad: \xff\xff\xff end\n";
        let indexed = decode_for_index(raw);
        let searched = decode(raw, EncodingMode::Auto);
        assert_eq!(
            indexed.as_ref(),
            searched.as_bytes(),
            "index and search must agree byte for byte"
        );
        assert!(
            indexed.windows(3).any(|w| w == "\u{FFFD}".as_bytes()),
            "the replacement character has to be indexable"
        );
    }

    #[test]
    fn index_and_search_repairs_agree_on_every_invalid_shape() {
        // The index repair skips the offset fixups the search repair records.
        // Both walk the same loop so they cannot drift, but the exact-size
        // pre-pass they now share has to agree with it for every shape of
        // invalid input — including one truncated at EOF, where `error_len`
        // is `None` and the walk stops early.
        let cases: &[&[u8]] = &[
            b"\xff",
            b"\xffleading\n",
            b"trailing\xff",
            b"two \xff\xfe apart \xff\n",
            b"adjacent \xff\xff\xff runs\n",
            b"truncated 2-byte \xc3",
            b"truncated 3-byte \xe2\x82",
            b"truncated 4-byte \xf0\x9f\x92",
            b"mixed \xc3\xa9 valid then \xff invalid\n",
            "no repairs at all\n".as_bytes(),
        ];
        for raw in cases {
            let indexed = decode_for_index(raw);
            let searched = decode(raw, EncodingMode::Auto);
            assert_eq!(
                indexed.as_ref(),
                searched.as_bytes(),
                "index and search disagree on {raw:?}"
            );
            // Pins the pre-pass against the loop it sizes, which `debug_assert`
            // only checks in debug builds.
            let (predicted, repairs) = lossy_utf8_shape(raw);
            let (repaired, fixups) = lossy_utf8(raw);
            if std::str::from_utf8(raw).is_err() {
                assert_eq!(predicted, repaired.len(), "bad length for {raw:?}");
                assert_eq!(repairs, fixups.shifts.len(), "bad count for {raw:?}");
            }
        }
    }

    #[test]
    fn valid_utf8_is_still_borrowed_for_the_index() {
        let raw = b"plain ascii text\n";
        assert!(
            matches!(decode_for_index(raw), Cow::Borrowed(_)),
            "the common case must not allocate"
        );
    }

    #[test]
    fn binary_files_are_left_alone_for_the_index() {
        // Repairing first could shift a NUL past the window `is_binary` reads,
        // so a file that is about to be discarded is returned untouched.
        let raw = b"\x00\xff\xff binary\n";
        assert!(matches!(decode_for_index(raw), Cow::Borrowed(_)));
        assert!(crate::trigram::is_binary(&decode_for_index(raw)));
    }

    #[test]
    fn decoding_owned_bytes_matches_decoding_borrowed_ones() {
        // The owned path exists purely to skip a copy, so it has to be
        // indistinguishable from the borrowed one on every input shape.
        let utf16 = {
            let mut v = vec![0xFF, 0xFE];
            v.extend_from_slice(&[0x68, 0x00, 0x69, 0x00]);
            v
        };
        let cases: Vec<Vec<u8>> = vec![
            b"plain ascii\n".to_vec(),
            b"caf\xc3\xa9 utf8\n".to_vec(),
            b"broken \xff\xfe utf8\n".to_vec(),
            b"\xef\xbb\xbfutf8 with bom\n".to_vec(),
            utf16,
            Vec::new(),
        ];
        for mode in [
            EncodingMode::Auto,
            EncodingMode::None,
            EncodingMode::Explicit(encoding_rs::UTF_8),
            EncodingMode::Explicit(encoding_rs::WINDOWS_1252),
        ] {
            for raw in &cases {
                let (want, want_fixups) = decode_with_fixups(raw, mode);
                let (got, got_fixups) = decode_owned_with_fixups(raw.clone(), mode);
                assert_eq!(got, want, "text differs for {raw:?} in {mode:?}");
                assert_eq!(
                    got_fixups.is_empty(),
                    want_fixups.is_empty(),
                    "fixups differ for {raw:?} in {mode:?}"
                );
            }
        }
    }

    #[test]
    fn owned_valid_utf8_reuses_the_buffer() {
        let raw = b"plain ascii text\n".to_vec();
        let ptr = raw.as_ptr();
        let (text, fixups) = decode_owned_with_fixups(raw, EncodingMode::Auto);
        assert!(fixups.is_empty());
        assert_eq!(
            text.as_ptr(),
            ptr,
            "the common case must convert in place, not copy"
        );
    }
}
