//! Engine errors.

use std::fmt;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Msg(String),
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Bencode(String),
    Metainfo(String),
    Path(PathBuf, String),
    /// Disk worker channel closed / OS disk thread exited — may be restartable.
    DiskWorkerStopped,
    /// Restart budget exhausted; durable writes fail until process restart.
    DiskWorkerPermanent,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Msg(s) => write!(f, "{s}"),
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Sqlite(e) => write!(f, "sqlite: {e}"),
            Error::Bencode(s) => write!(f, "bencode: {s}"),
            Error::Metainfo(s) => write!(f, "metainfo: {s}"),
            Error::Path(p, s) => write!(f, "{}: {s}", p.display()),
            Error::DiskWorkerStopped => write!(f, "disk worker stopped"),
            Error::DiskWorkerPermanent => write!(f, "disk worker permanently dead"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Sqlite(e)
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error::Msg(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error::Msg(s.to_string())
    }
}
