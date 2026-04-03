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
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Json(e) => write!(f, "JSON error: {e}"),
            Error::IndexNotFound(p) => write!(f, "index not found at {p}"),
            Error::IndexCorrupted(msg) => write!(f, "corrupted index: {msg}"),
            Error::Regex(msg) => write!(f, "regex error: {msg}"),
            Error::Server(msg) => write!(f, "server error: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
