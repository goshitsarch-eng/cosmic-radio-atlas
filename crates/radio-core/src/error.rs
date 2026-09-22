//! Error type shared by the core crate.
use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Json(serde_json::Error),
    Http(reqwest::Error),
    MissingRuntimeDir,
    InvalidInput(String),
    Unavailable(String),
    InvalidState(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(error) => write!(f, "{error}"),
            Error::Json(error) => write!(f, "{error}"),
            Error::Http(error) => write!(f, "{error}"),
            Error::MissingRuntimeDir => write!(f, "Radio Atlas requires XDG_RUNTIME_DIR"),
            Error::InvalidInput(message)
            | Error::Unavailable(message)
            | Error::InvalidState(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::Json(error)
    }
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Error::Http(error)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
