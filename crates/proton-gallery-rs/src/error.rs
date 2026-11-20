#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("catalog was written by a newer application (schema {found}, supported {supported})")]
    NewerSchema { found: u32, supported: u32 },
    #[error("catalog worker is unavailable")]
    CatalogClosed,
    #[error("database operation failed")]
    Database(#[from] rusqlite::Error),
    #[error("local filesystem operation failed")]
    Io(#[from] std::io::Error),
    #[error("stored data has an invalid encoding")]
    Encoding(#[from] serde_json::Error),
    #[error("operation cancelled")]
    Cancelled,
    #[error("scan was replaced by another worker or its checkpoint changed")]
    StaleScan,
    #[error("requested local record does not exist")]
    NotFound,
    #[error("requested bytes are unavailable offline")]
    OfflineMiss,
    #[error("local media failed authentication or length validation")]
    CorruptObject,
    #[error("remote source failed: {0}")]
    Remote(&'static str),
    #[error("remote bytes do not match the selected revision")]
    RevisionMismatch,
    #[error("background operation failed")]
    Worker(#[from] tokio::task::JoinError),
    #[error("operating-system randomness is unavailable")]
    Random,
}

pub type Result<T> = std::result::Result<T, Error>;
