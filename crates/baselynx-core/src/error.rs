use std::fmt;

/// A server-reported error, kept with the fields the UI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbError {
    pub message: String,
    pub code: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// 1-based character position in the statement, when the server gives one.
    pub position: Option<u32>,
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)?;
        if let Some(detail) = &self.detail {
            write!(f, "\nDetail: {detail}")?;
        }
        if let Some(hint) = &self.hint {
            write!(f, "\nHint: {hint}")?;
        }
        Ok(())
    }
}

/// Which change of a commit failed: its position in the order of [`crate::edit::preview`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeIndex(pub usize);

#[derive(Debug, Clone, thiserror::Error)]
pub enum CoreError {
    #[error("{0}")]
    Db(DbError),
    /// Connecting failed before the server answered (DNS, refused, timeout, TLS).
    #[error("{0}")]
    Connect(String),
    #[error("connection closed")]
    Closed,
    #[error("cancelled")]
    Cancelled,
    /// Input the client refuses to send, such as a filter with a second statement.
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Storage(String),
    #[error("{0}")]
    Keyring(String),
    #[error("{0}")]
    Export(String),
    /// A commit failed at one change; nothing was applied.
    #[error("change {} failed: {source}", .index.0 + 1)]
    Commit { index: ChangeIndex, source: Box<CoreError> },
    #[error("{0}")]
    Internal(String),
}

impl CoreError {
    pub fn db(&self) -> Option<&DbError> {
        match self {
            CoreError::Db(e) => Some(e),
            CoreError::Commit { source, .. } => source.db(),
            _ => None,
        }
    }

    pub fn sqlstate(&self) -> Option<&str> {
        self.db().map(|e| e.code.as_str())
    }

    pub fn is_cancel(&self) -> bool {
        matches!(self, CoreError::Cancelled) || self.sqlstate() == Some("57014")
    }
}

impl From<tokio_postgres::Error> for CoreError {
    fn from(err: tokio_postgres::Error) -> Self {
        if let Some(db) = err.as_db_error() {
            let position = match db.position() {
                Some(tokio_postgres::error::ErrorPosition::Original(p)) => Some(*p),
                _ => None,
            };
            return CoreError::Db(DbError {
                message: db.message().to_string(),
                code: db.code().code().to_string(),
                detail: db.detail().map(str::to_string),
                hint: db.hint().map(str::to_string),
                position,
            });
        }
        if err.is_closed() {
            return CoreError::Closed;
        }
        // "error connecting to server" alone hides the cause (refused, timed out, DNS).
        let mut text = err.to_string();
        let mut source = std::error::Error::source(&err);
        while let Some(s) = source {
            let part = s.to_string();
            if !text.contains(&part) {
                text.push_str(": ");
                text.push_str(&part);
            }
            source = s.source();
        }
        CoreError::Connect(text)
    }
}

impl From<std::io::Error> for CoreError {
    fn from(err: std::io::Error) -> Self {
        CoreError::Io(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;
