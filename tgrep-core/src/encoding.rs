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
    match decode_bytes(bytes, mode) {
        Cow::Borrowed(body) => lossy_utf8(body),
        // Transcoded output is always valid UTF-8, so this is a move.
        Cow::Owned(body) => String::from_utf8(body)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()),
    }
}

/// Decode using the same rules the index builder uses.
///
/// Indexes are always built in [`EncodingMode::Auto`]; see
/// [`EncodingMode::may_differ_from_index`].
pub fn decode_for_index(bytes: &[u8]) -> Cow<'_, [u8]> {
    decode_bytes(bytes, EncodingMode::Auto)
}

fn transcode(enc: &'static Encoding, bytes: &[u8]) -> String {
    // The BOM has already been consumed by the caller, so decoding must not
    // sniff for one again — a stray BOM-looking sequence mid-file is data.
    let (decoded, _had_errors) = enc.decode_without_bom_handling(bytes);
    decoded.into_owned()
}

fn lossy_utf8(bytes: &[u8]) -> String {
    match String::from_utf8(bytes.to_vec()) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
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
}
