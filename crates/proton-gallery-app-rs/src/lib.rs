//! Application services shared by the gallery frontends: the account and its
//! session, Photos synchronization, library queries, and media retrieval.
//!
//! The Android app reaches these through `pg-mobile`'s UniFFI adapter; the
//! desktop app calls them directly. Everything here is asynchronous and runs
//! on the caller's Tokio runtime. Presentation state (navigation, selection,
//! scroll position, dialogs) stays in each frontend.
//!
//! Work that outlives a sign-out must not publish results for the old
//! account. Each change of session raises the [`Generation`]; long work takes
//! the generation at its start and checks it with [`App::ensure_current`]
//! before it writes or returns.
use proton_gallery_drive_rs::auth::{AuthService, CredentialVault};
use proton_gallery_rs::catalog::Catalog;
use proton_gallery_rs::library::SourceKind;
use proton_gallery_rs::local::LocalLibrary;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
};

pub mod account;
pub mod albums;
pub mod config_sync;
#[cfg(target_os = "linux")]
pub mod desktop_auth;
pub mod drive_galleries;
pub mod favorites;
pub mod fixtures;
pub mod library;
pub mod media;
pub mod places;
pub mod sharing;
pub mod sync;
pub mod thumbnail_absence;
pub mod video;

pub use account::{AccountState, SignInStage};
pub use library::{MediaItem, Outline, View};
pub use sync::{PhotosSyncStatus, SyncProgress};

/// A failure to show the user, or work that stopped on purpose.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Something went wrong; the text is written for the user.
    #[error("{0}")]
    Message(String),
    #[error("Your Proton session expired. Sign in again to sync your library.")]
    SessionExpired,
    /// The work was cancelled, for example by signing out.
    #[error("cancelled")]
    Cancelled,
    /// The work belongs to an account that is no longer signed in.
    #[error("the account changed")]
    Stale,
}

impl Error {
    pub fn message(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }
}

impl From<proton_gallery_rs::Error> for Error {
    fn from(error: proton_gallery_rs::Error) -> Self {
        match error {
            proton_gallery_rs::Error::Cancelled => Self::Cancelled,
            other => Self::Message(other.to_string()),
        }
    }
}

impl From<proton_gallery_drive_rs::auth::AuthError> for Error {
    fn from(error: proton_gallery_drive_rs::auth::AuthError) -> Self {
        Self::Message(error.to_string())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Which session a piece of work started under. Compare with
/// [`App::ensure_current`] before publishing its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(u64);

/// The shared application state. One per data directory.
pub struct App {
    pub catalog: Catalog,
    pub local: LocalLibrary,
    pub auth: AuthService,
    /// The Photos source of the signed-in account, once opened.
    pub photos: RwLock<Option<Arc<proton_gallery_drive_rs::photos::ProtonPhotosSource>>>,
    // Photos and Drive galleries sync independently; each has its own progress.
    pub photos_sync: tokio::sync::Mutex<()>,
    pub indexed: AtomicU64,
    pub drive_sync: tokio::sync::Mutex<()>,
    configuration_sync: tokio::sync::Mutex<()>,
    configuration_poll: tokio::sync::Mutex<()>,
    pub drive_indexed: AtomicU64,
    pub backup_check: tokio::sync::Mutex<()>,
    /// Serializes restore, challenge continuations and account cleanup.
    transition: tokio::sync::Mutex<()>,
    /// Cleanup waits for operations to finish before forgetting catalog/files.
    operations: tokio::sync::RwLock<()>,
    cleanup: RwLock<Option<Option<proton_gallery_rs::model::AccountId>>>,
    generation: AtomicU64,
    /// Photos Proton said have no thumbnail ([`thumbnail_absence`]).
    thumbnail_absence: std::sync::Mutex<thumbnail_absence::ThumbnailAbsence>,
    root: PathBuf,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("data_dir", &self.root)
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

impl App {
    /// Open (or create) the catalog in `data_dir`. Saved sessions are kept in
    /// `vault`; call [`App::restore_session`] to resume one.
    pub async fn open(
        data_dir: impl Into<PathBuf>,
        vault: Arc<dyn CredentialVault>,
    ) -> Result<Self> {
        Self::open_with_auth(data_dir.into(), AuthService::new(vault)).await
    }

    /// Open gallery services for an application that already owns its session.
    pub async fn open_hosted(
        data_dir: PathBuf,
        session: proton_sdk::session::ProtonApiSession,
        drive: proton_drive_rs::ProtonDriveClient,
        cancel: proton_gallery_rs::CancellationToken,
    ) -> Result<Self> {
        struct HostVault;
        impl CredentialVault for HostVault {
            fn load(&self) -> proton_gallery_drive_rs::auth::Result<Option<Vec<u8>>> {
                Ok(None)
            }
            fn save(&self, _: &[u8]) -> proton_gallery_drive_rs::auth::Result<()> {
                Err(proton_gallery_drive_rs::auth::AuthError(
                    "The host owns the session.",
                ))
            }
            fn clear(&self) -> proton_gallery_drive_rs::auth::Result<()> {
                Ok(())
            }
        }
        let auth = AuthService::hosted(Arc::new(HostVault), session, drive, cancel).await?;
        Self::open_with_auth(data_dir, auth).await
    }

    async fn open_with_auth(root: PathBuf, auth: AuthService) -> Result<Self> {
        std::fs::create_dir_all(&root).map_err(|e| Error::message(e.to_string()))?;
        let catalog = Catalog::open(root.join("catalog.db")).await?;
        let local = LocalLibrary::open(root.join("device"), catalog.clone()).await?;
        catalog.register_source(LocalLibrary::source()?).await?;
        catalog
            .set_source_kind(LocalLibrary::source()?.id, SourceKind::Device)
            .await?;
        catalog
            .register_source(proton_gallery_rs::phone::phone_source()?)
            .await?;
        let cleanup = match std::fs::read(root.join("signout-pending.json")) {
            Ok(bytes) => Some(
                serde_json::from_slice(&bytes)
                    .map_err(|_| Error::message("Sign-out cleanup record is unreadable."))?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(Error::message("Sign-out cleanup record couldn’t be read.")),
        };
        Ok(Self {
            catalog,
            local,
            auth,
            photos: RwLock::new(None),
            photos_sync: tokio::sync::Mutex::new(()),
            indexed: AtomicU64::new(0),
            drive_sync: tokio::sync::Mutex::new(()),
            configuration_sync: tokio::sync::Mutex::new(()),
            configuration_poll: tokio::sync::Mutex::new(()),
            drive_indexed: AtomicU64::new(0),
            backup_check: tokio::sync::Mutex::new(()),
            transition: tokio::sync::Mutex::new(()),
            operations: tokio::sync::RwLock::new(()),
            cleanup: RwLock::new(cleanup),
            generation: AtomicU64::new(0),
            thumbnail_absence: Default::default(),
            root,
        })
    }

    /// The directory this app's data lives in.
    pub fn data_dir(&self) -> &Path {
        &self.root
    }

    /// The current session's generation.
    pub fn generation(&self) -> Generation {
        Generation(self.generation.load(Ordering::SeqCst))
    }

    /// `Err(Stale)` when the session changed since `generation` was taken.
    pub fn ensure_current(&self, generation: Generation) -> Result<()> {
        if self.generation() == generation {
            Ok(())
        } else {
            Err(Error::Stale)
        }
    }

    /// Start a new generation: every result of the previous one is stale.
    fn advance(&self) -> Generation {
        self.thumbnail_absence().clear();
        Generation(self.generation.fetch_add(1, Ordering::SeqCst) + 1)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::Mutex;

    /// A vault in memory, which can be made to fail.
    #[derive(Default)]
    pub struct MemoryVault {
        pub saved: Mutex<Option<Vec<u8>>>,
        pub fail_clear: std::sync::atomic::AtomicBool,
    }

    impl CredentialVault for MemoryVault {
        fn load(&self) -> proton_gallery_drive_rs::auth::Result<Option<Vec<u8>>> {
            Ok(self.saved.lock().unwrap().clone())
        }
        fn save(&self, bytes: &[u8]) -> proton_gallery_drive_rs::auth::Result<()> {
            *self.saved.lock().unwrap() = Some(bytes.to_vec());
            Ok(())
        }
        fn clear(&self) -> proton_gallery_drive_rs::auth::Result<()> {
            if self.fail_clear.load(Ordering::Relaxed) {
                return Err(proton_gallery_drive_rs::auth::AuthError(
                    "Keyring locked. Retry cleanup.",
                ));
            }
            *self.saved.lock().unwrap() = None;
            Ok(())
        }
    }

    /// A saved session in the vault's format. Restoring it needs no network.
    pub fn saved_session(user: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "session_id": "session",
            "username": "someone",
            "user_id": user,
            "access_token": "access",
            "refresh_token": "refresh",
            "scopes": ["full"],
            "password_mode": 1,
            "key_passphrases": { "key": "passphrase" }
        }))
        .unwrap()
    }

    pub async fn app(vault: Arc<MemoryVault>) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let app = App::open(dir.path(), vault).await.unwrap();
        (dir, app)
    }
}
