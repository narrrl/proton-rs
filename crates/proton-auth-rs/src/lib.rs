//! Credential storage shared by the Proton Drive app family
//! (proton-drive-linux, Proton Gallery, Proton Stream).
//!
//! - [`SecretStore`] is the one interface a signed-in session and other
//!   secrets are kept behind. Desktop apps use [`KeyringStore`] (feature
//!   `keyring`); Android and iOS apps implement the trait over their platform
//!   keystore through their language bridge.
//! - [`Chunked`] splits a value too long for one entry of a small store
//!   (Windows Credential Manager) across several entries.
//! - [`MemoryStore`] keeps secrets for this process only: for tests, and for a
//!   session the user chose not to remember.
//!
//! [`KeyringStore`] reads entries written by `keyring` 3 on every platform,
//! so moving an app from `keyring` 3 to this crate keeps its users signed in.

mod chunked;
#[cfg(all(
    feature = "keyring",
    not(any(target_os = "android", target_os = "ios"))
))]
mod keyring;
mod memory;
#[cfg(all(
    feature = "keyring",
    any(target_os = "linux", target_os = "freebsd", target_os = "openbsd")
))]
mod secret_service;
#[cfg(feature = "session")]
pub mod session;

pub use chunked::{CHUNK_HEADER, Chunked, ENTRY_UNITS};
#[cfg(all(
    feature = "keyring",
    not(any(target_os = "android", target_os = "ios"))
))]
pub use keyring::{Connector, KeyringStore};
#[cfg(feature = "keyring")]
pub use keyring_core;
pub use memory::MemoryStore;

/// Why a secret could not be read, written or deleted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The OS credential store failed. A locked keyring or a dismissed unlock
    /// prompt is `NoStorageAccess`; a missing entry is never an error here,
    /// it reads as `None`.
    #[cfg(feature = "keyring")]
    #[error("credential store: {0}")]
    Keyring(#[from] keyring_core::Error),
    /// A store supplied by the app (an Android or iOS keystore) failed.
    #[error("credential store: {0}")]
    Store(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Where an app keeps its secrets, keyed by name.
///
/// A missing key reads as `None` and deleting it succeeds, so callers need no
/// special case for "never saved".
pub trait SecretStore: Send + Sync {
    /// Save `value` under `key`, replacing what was there.
    fn set(&self, key: &str, value: &str) -> Result<()>;
    /// The value saved under `key`, if any.
    fn get(&self, key: &str) -> Result<Option<String>>;
    /// Forget `key`. Succeeds when it was not there.
    fn delete(&self, key: &str) -> Result<()>;
}

impl<S: SecretStore + ?Sized> SecretStore for std::sync::Arc<S> {
    fn set(&self, key: &str, value: &str) -> Result<()> {
        (**self).set(key, value)
    }
    fn get(&self, key: &str) -> Result<Option<String>> {
        (**self).get(key)
    }
    fn delete(&self, key: &str) -> Result<()> {
        (**self).delete(key)
    }
}

impl<S: SecretStore + ?Sized> SecretStore for Box<S> {
    fn set(&self, key: &str, value: &str) -> Result<()> {
        (**self).set(key, value)
    }
    fn get(&self, key: &str) -> Result<Option<String>> {
        (**self).get(key)
    }
    fn delete(&self, key: &str) -> Result<()> {
        (**self).delete(key)
    }
}
