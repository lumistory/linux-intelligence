use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("usage: {0}")]
    Usage(String),
    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("io error: {0}")]
    PlainIo(#[from] std::io::Error),
    #[error("syscall {call} failed: {source}")]
    Sys {
        call: &'static str,
        #[source]
        source: nix::errno::Errno,
    },
    #[error("json serialisation failed: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}
