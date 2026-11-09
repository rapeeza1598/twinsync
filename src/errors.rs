use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid relative path: {0}")]
    BadRelPath(String),
    #[error("I/O error ({context}): {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("drive identity: {0}")]
    Identity(String),
    #[error("path safety: {0}")]
    PathSafety(String),
    #[error("operation cancelled")]
    Cancelled,
    #[error("source changed while copying")]
    SourceChanged,
    #[error("destination changed: {0}")]
    DestChanged(String),
    #[error("verification failed: {0}")]
    VerifyFailed(String),
    #[error("not enough free space: need {need} bytes, have {have} bytes")]
    NoSpace { need: u64, have: u64 },
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("injected fault: {0}")]
    Fault(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// Errors worth retrying with backoff (device slow, busy, or briefly gone).
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Io { source, .. } => matches!(
                source.raw_os_error(),
                Some(libc::EIO | libc::ETIMEDOUT | libc::EAGAIN | libc::EBUSY | libc::EINTR)
            ),
            Error::SourceChanged => true,
            _ => false,
        }
    }

    pub fn is_no_space(&self) -> bool {
        matches!(self, Error::NoSpace { .. })
            || matches!(self, Error::Io { source, .. } if source.raw_os_error() == Some(libc::ENOSPC))
    }
}

pub trait IoContext<T> {
    fn ctx(self, context: &str) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn ctx(self, context: &str) -> Result<T> {
        self.map_err(|e| Error::io(context, e))
    }
}
