#![doc = include_str!("../README.md")]

pub mod blocks;
pub mod disk;
pub mod flight;
pub mod images;
#[cfg(feature = "image")]
pub mod render;
pub mod stream;

pub use blocks::{BlockMap, splice};
pub use disk::{BlockId, DiskCache, DiskCacheConfig, DiskStats};
pub use flight::{Shared, SingleFlight};
pub use images::{ImageCache, ImageKey};

/// Errors from this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("block cache: {0}")]
    Io(#[from] std::io::Error),
    /// A cached entry is unreadable or disagrees with its sidecar.
    #[error("block cache: {0}")]
    Corrupt(String),
    /// A picture could not be decoded or encoded.
    #[error("image: {0}")]
    Image(String),
    /// The caller asked for something that does not exist, such as a block
    /// past the end of a file.
    #[error("{0}")]
    NotFound(String),
    /// A failure that happened in another task: a deduplicated fetch's waiters
    /// get the leader's message, since errors are not `Clone`.
    #[error("{0}")]
    Shared(String),
    /// The block source failed (network, API refusal, decryption).
    #[error(transparent)]
    Source(Box<dyn std::error::Error + Send + Sync>),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<Shared> for Error {
    fn from(shared: Shared) -> Self {
        Self::Shared(shared.0)
    }
}
