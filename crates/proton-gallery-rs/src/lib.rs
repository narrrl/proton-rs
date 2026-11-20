//! Platform-independent gallery behavior. No UI, credentials, or account login.

pub mod backup_albums;
pub mod bytes;
pub mod catalog;
pub mod configuration;
pub mod error;
pub mod grid;
pub mod indexer;
pub mod library;
pub mod local;
pub mod model;
pub mod outline;
pub mod phone;
pub mod photo_albums;
pub mod photos_sync;
pub mod picker;
pub mod places;
pub mod removal;
pub mod source;
pub mod synthetic;

pub use error::{Error, Result};
pub use tokio_util::sync::CancellationToken;
