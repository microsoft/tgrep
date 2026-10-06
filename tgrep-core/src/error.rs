use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Json(serde_json::Error),
    IndexNotFound(String),
    IndexCorrupted(String),
    Regex(String),
    Server(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
            Self::IndexNotFound(p) => write!(f, "index not found at {p}"),
            Self::IndexCorrupted(msg) => write!(f, "corrupted index: {msg}"),
            Self::Regex(msg) => write!(f, "regex error: {msg}"),
            Self::Server(msg) => write!(f, "server error: {msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    // --- Display ------------------------------------------------------------

    #[test]
    fn display_io_includes_inner_message() {
        let error = Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "missing file",
        ));
        let rendered = error.to_string();
        assert!(
            rendered.starts_with("I/O error: "),
            "Io Display must keep its prefix; got: {rendered:?}"
        );
        assert!(
            rendered.contains("missing file"),
            "Io Display must include the inner io::Error message; got: {rendered:?}"
        );
    }

    #[test]
    fn display_json_includes_inner_message() {
        // serde_json needs a real parse failure to surface a non-empty message.
        let inner: serde_json::Error =
            serde_json::from_str::<serde_json::Value>("{ not json }").unwrap_err();
        let error = Error::Json(inner);
        let rendered = error.to_string();
        assert!(
            rendered.starts_with("JSON error: "),
            "Json Display must keep its prefix; got: {rendered:?}"
        );
        assert!(
            !rendered["JSON error: ".len()..].is_empty(),
            "Json Display must include the inner serde_json::Error message; got: {rendered:?}"
        );
    }

    #[test]
    fn display_index_not_found_includes_path() {
        let error = Error::IndexNotFound("/tmp/does-not-exist".into());
        let rendered = error.to_string();
        assert!(
            rendered.starts_with("index not found at "),
            "IndexNotFound Display must keep its prefix; got: {rendered:?}"
        );
        assert!(
            rendered.contains("/tmp/does-not-exist"),
            "IndexNotFound Display must include the path; got: {rendered:?}"
        );
    }

    #[test]
    fn display_index_corrupted_includes_reason() {
        let error = Error::IndexCorrupted("truncated posting list".into());
        let rendered = error.to_string();
        assert!(
            rendered.starts_with("corrupted index: "),
            "IndexCorrupted Display must keep its prefix; got: {rendered:?}"
        );
        assert!(
            rendered.contains("truncated posting list"),
            "IndexCorrupted Display must include the reason; got: {rendered:?}"
        );
    }

    #[test]
    fn display_regex_includes_message() {
        let error = Error::Regex("unbalanced paren".into());
        let rendered = error.to_string();
        assert!(
            rendered.starts_with("regex error: "),
            "Regex Display must keep its prefix; got: {rendered:?}"
        );
        assert!(
            rendered.contains("unbalanced paren"),
            "Regex Display must include the message; got: {rendered:?}"
        );
    }

    #[test]
    fn display_server_includes_message() {
        let error = Error::Server("connection refused".into());
        let rendered = error.to_string();
        assert!(
            rendered.starts_with("server error: "),
            "Server Display must keep its prefix; got: {rendered:?}"
        );
        assert!(
            rendered.contains("connection refused"),
            "Server Display must include the message; got: {rendered:?}"
        );
    }

    // --- source() -----------------------------------------------------------
    //
    // The wrapping variants must expose their inner cause so error chains survive
    // `?`-based conversion. The other variants carry only a message string and
    // have no cause to surface, so they must return None.

    #[test]
    fn source_io_returns_inner_error() {
        let inner = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let error = Error::Io(inner);
        let source = error.source();
        assert!(source.is_some(), "Io must surface its inner io::Error");
        // std::io::Error's Display is the raw message we passed in.
        assert_eq!(source.unwrap().to_string(), "denied");
    }

    #[test]
    fn source_json_returns_inner_error() {
        let inner: serde_json::Error =
            serde_json::from_str::<serde_json::Value>("{ not json }").unwrap_err();
        // Capture the inner message before the value is moved into Error::Json.
        let inner_message = inner.to_string();
        let error = Error::Json(inner);
        let source = error.source();
        assert!(
            source.is_some(),
            "Json must surface its inner serde_json::Error"
        );
        // The surfaced source must carry the same message as the wrapped error.
        assert_eq!(source.unwrap().to_string(), inner_message);
    }

    #[test]
    fn source_string_variants_return_none() {
        assert!(Error::IndexNotFound("/x".into()).source().is_none());
        assert!(Error::IndexCorrupted("reason".into()).source().is_none());
        assert!(Error::Regex("bad pattern".into()).source().is_none());
        assert!(Error::Server("down".into()).source().is_none());
    }

    // --- From conversions ---------------------------------------------------

    #[test]
    fn from_io_error_wraps_and_preserves_source() {
        let inner = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "truncated");
        let error = Error::from(inner);
        assert!(
            matches!(error, Error::Io(_)),
            "From<io::Error> must produce Error::Io"
        );
        assert!(
            error.source().is_some(),
            "From<io::Error> must preserve the inner error for source()"
        );
        assert!(error.to_string().contains("truncated"));
    }

    #[test]
    fn from_serde_json_error_wraps_and_preserves_source() {
        let inner: serde_json::Error =
            serde_json::from_str::<serde_json::Value>("{ broken").unwrap_err();
        let error = Error::from(inner);
        assert!(
            matches!(error, Error::Json(_)),
            "From<serde_json::Error> must produce Error::Json"
        );
        assert!(
            error.source().is_some(),
            "From<serde_json::Error> must preserve the inner error for source()"
        );
    }

    // --- Result alias and Send/Sync -----------------------------------------
    //
    // `Result<T>` is the public return type for the whole crate; it must cross
    // thread boundaries (e.g. into worker pools in walker.rs and the serve
    // runtime). Pin those bounds at compile time so they cannot regress.

    #[test]
    fn result_alias_compiles() {
        let ok: Result<u8> = Ok(7);
        assert!(
            matches!(ok, Ok(7)),
            "Ok variant of the Result alias must round-trip"
        );

        let err: Result<u8> = Err(Error::Server("boom".into()));
        assert!(err.is_err());
    }

    #[test]
    fn error_is_send_and_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<Error>();
        assert_sync::<Error>();
        assert_send::<Result<u8>>();
        assert_sync::<Result<u8>>();
    }
}
