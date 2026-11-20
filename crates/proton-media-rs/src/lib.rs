#![doc = include_str!("../README.md")]

pub mod blocks;
pub mod disk;
pub mod flight;
pub mod images;
#[cfg(feature = "image")]
pub mod render;

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
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
