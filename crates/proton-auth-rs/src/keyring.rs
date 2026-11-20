use std::sync::{Arc, Mutex};

use keyring_core::{CredentialStore, Entry};
use zeroize::Zeroizing;

use crate::{Result, SecretStore};

/// Opens a connection to a credential store.
pub type Connector = Arc<dyn Fn() -> keyring_core::Result<Arc<CredentialStore>> + Send + Sync>;

/// The OS credential store: Secret Service on Linux and the BSDs, Credential
/// Manager on Windows, the login Keychain on macOS.
///
/// Every key is one entry of `service`, with the key as its user name. Those
/// are the entries `keyring` 3 wrote for the same service and user, on every
/// platform, so existing secrets still read.
///
/// The store holds one connection for the whole process. A Secret Service
/// session lives in the provider's memory: once the provider restarts (oo7's
/// unit restarts it on failure, and oo7 and gnome-keyring can both be
/// `--replace`d), every call on the old session fails for as long as the
/// process runs, and a daemon would keep refreshed tokens only in memory. A
/// platform failure therefore drops the connection and the call is made once
/// more on a new one. A locked keyring, a dismissed prompt or a missing entry
/// is not the connection's fault and comes back as it is.
///
/// On the Secret Service, a value that replaces an existing one is written as
/// a new item replacing the old item, never by editing the old item's secret:
/// oo7 before 0.7.0 keeps an edit only in memory, and after a reboot the old
/// secret is back (a spent refresh token, so the user is signed out).
pub struct KeyringStore {
    service: String,
    connect: Connector,
    store: Mutex<Option<Arc<CredentialStore>>>,
    replace_items: fn() -> bool,
}

impl std::fmt::Debug for KeyringStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyringStore")
            .field("service", &self.service)
            .field("replace_items", &(self.replace_items)())
            .finish_non_exhaustive()
    }
}

impl KeyringStore {
    /// The platform's credential store, entries under `service`.
    ///
    /// Connects on first use, not here, so a session bus that is not up yet
    /// is tried again on the next call.
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            connect: Arc::new(connect_platform),
            store: Mutex::new(None),
            replace_items: || {
                cfg!(any(
                    target_os = "linux",
                    target_os = "freebsd",
                    target_os = "openbsd"
                ))
            },
        }
    }

    /// A store opened by `connect` instead of the platform's, for example
    /// `keyring_core::mock::Store` in tests. Items are edited in place.
    pub fn with_connector(service: impl Into<String>, connect: Connector) -> Self {
        Self {
            service: service.into(),
            connect,
            store: Mutex::new(None),
            replace_items: || false,
        }
    }

    /// Decide per write whether a changed secret replaces the Secret Service
    /// item instead of editing it. [`Self::new`] always replaces on the
    /// Secret Service; [`Self::with_connector`] never does, since it talks to
    /// the store `connect` opens rather than to the Secret Service.
    pub fn replace_items(mut self, when: fn() -> bool) -> Self {
        self.replace_items = when;
        self
    }

    /// The service every entry is stored under.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Run `op` on the entry for `user`, reconnecting once if the store fails
    /// the call.
    ///
    /// For callers that need more than [`SecretStore`] offers, such as raw
    /// secrets or the exact `keyring_core::Error`.
    pub fn with_entry<T>(
        &self,
        user: &str,
        op: impl Fn(&Entry) -> keyring_core::Result<T>,
    ) -> keyring_core::Result<T> {
        let store = self.store()?;
        match op(&store.build(&self.service, user, None)?) {
            Err(keyring_core::Error::PlatformFailure(e)) => {
                tracing::warn!(
                    error = %e,
                    service = %self.service,
                    "the credential store failed this process's connection; reconnecting"
                );
                self.drop_store(&store);
                let store = self.store()?;
                op(&store.build(&self.service, user, None)?)
            }
            result => result,
        }
    }

    /// Drop the connection, so the next call connects again.
    pub fn disconnect(&self) {
        *self.slot() = None;
    }

    /// The connected store, connecting now if there is none.
    ///
    /// Threads failing at the same time drop the broken store once and share
    /// its replacement.
    fn store(&self) -> keyring_core::Result<Arc<CredentialStore>> {
        let mut slot = self.slot();
        if let Some(store) = slot.as_ref() {
            return Ok(Arc::clone(store));
        }
        let store = (self.connect)()?;
        *slot = Some(Arc::clone(&store));
        Ok(store)
    }

    /// Forget `failed`, unless another thread has already replaced it.
    fn drop_store(&self, failed: &Arc<CredentialStore>) {
        let mut slot = self.slot();
        if slot
            .as_ref()
            .is_some_and(|store| Arc::ptr_eq(store, failed))
        {
            *slot = None;
        }
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Arc<CredentialStore>>> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
    fn replace(&self, key: &str, value: &[u8]) -> bool {
        if !(self.replace_items)() {
            return false;
        }
        match crate::secret_service::replace(&self.service, key, value) {
            crate::secret_service::Replaced::Saved => true,
            crate::secret_service::Replaced::Skipped(why) => {
                tracing::debug!(%why, "not replacing the keyring item; writing it in place");
                false
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd")))]
    fn replace(&self, _key: &str, _value: &[u8]) -> bool {
        false
    }

    /// Save raw bytes under `key`, for a secret that is not text.
    ///
    /// Bytes and text are stored differently on Windows (text as UTF-16), so
    /// read a value back the way it was written: [`Self::get_bytes`] here,
    /// [`SecretStore::get`] for [`SecretStore::set`].
    pub fn set_bytes(&self, key: &str, value: &[u8]) -> Result<()> {
        if self.replace(key, value) {
            return Ok(());
        }
        Ok(self.with_entry(key, |entry| entry.set_secret(value))?)
    }

    /// The raw bytes saved under `key` by [`Self::set_bytes`], if any.
    pub fn get_bytes(&self, key: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        match self.with_entry(key, |entry| entry.get_secret()) {
            Ok(value) => Ok(Some(Zeroizing::new(value))),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

impl SecretStore for KeyringStore {
    fn set(&self, key: &str, value: &str) -> Result<()> {
        if self.replace(key, value.as_bytes()) {
            return Ok(());
        }
        Ok(self.with_entry(key, |entry| entry.set_password(value))?)
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        match self.with_entry(key, |entry| entry.get_password()) {
            Ok(value) => Ok(Some(value)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        match self.with_entry(key, |entry| entry.delete_credential()) {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
fn connect_platform() -> keyring_core::Result<Arc<CredentialStore>> {
    Ok(dbus_secret_service_keyring_store::Store::new()?)
}

#[cfg(target_os = "windows")]
fn connect_platform() -> keyring_core::Result<Arc<CredentialStore>> {
    Ok(windows_native_keyring_store::Store::new()?)
}

#[cfg(target_os = "macos")]
fn connect_platform() -> keyring_core::Result<Arc<CredentialStore>> {
    Ok(apple_native_keyring_store::keychain::Store::new()?)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "windows",
    target_os = "macos"
)))]
fn connect_platform() -> keyring_core::Result<Arc<CredentialStore>> {
    Err(keyring_core::Error::NotSupportedByStore(
        "no OS credential store on this platform".into(),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Fixture {
        keyring: KeyringStore,
        connects: Arc<AtomicUsize>,
        backing: Arc<CredentialStore>,
    }

    /// A keyring over one mock store that survives reconnects, counting them.
    fn fixture() -> Fixture {
        let backing: Arc<CredentialStore> = keyring_core::mock::Store::new().unwrap();
        let connects = Arc::new(AtomicUsize::new(0));
        let keyring = KeyringStore::with_connector("proton-auth-rs-test", {
            let backing = Arc::clone(&backing);
            let connects = Arc::clone(&connects);
            Arc::new(move || {
                connects.fetch_add(1, Ordering::SeqCst);
                Ok(Arc::clone(&backing))
            })
        });
        Fixture {
            keyring,
            connects,
            backing,
        }
    }

    /// Make the next call on `user`'s mock entry fail with `error`.
    fn fail_next(fixture: &Fixture, user: &str, error: keyring_core::Error) {
        let entry = fixture
            .backing
            .build("proton-auth-rs-test", user, None)
            .unwrap();
        entry
            .as_any()
            .downcast_ref::<keyring_core::mock::Cred>()
            .unwrap()
            .set_error(error);
    }

    fn platform_failure() -> keyring_core::Error {
        keyring_core::Error::PlatformFailure("org.freedesktop.Secret.Error.NoSession".into())
    }

    #[test]
    fn a_saved_secret_reads_back_until_deleted() {
        let f = fixture();
        assert_eq!(f.keyring.get("session").unwrap(), None);
        f.keyring.set("session", "tokens").unwrap();
        assert_eq!(f.keyring.get("session").unwrap().as_deref(), Some("tokens"));
        f.keyring.delete("session").unwrap();
        f.keyring.delete("session").unwrap();
        assert_eq!(f.keyring.get("session").unwrap(), None);
    }

    #[test]
    fn raw_bytes_read_back_as_written() {
        let f = fixture();
        assert_eq!(f.keyring.get_bytes("vault").unwrap(), None);
        f.keyring.set_bytes("vault", &[0, 159, 146, 150]).unwrap();
        assert_eq!(
            f.keyring
                .get_bytes("vault")
                .unwrap()
                .as_deref()
                .map(|v| v.as_slice()),
            Some(&[0u8, 159, 146, 150][..])
        );
        f.keyring.delete("vault").unwrap();
        assert_eq!(f.keyring.get_bytes("vault").unwrap(), None);
    }

    #[test]
    fn connects_once_and_reuses_the_connection() {
        let f = fixture();
        f.keyring.set("a", "1").unwrap();
        f.keyring.get("a").unwrap();
        f.keyring.delete("a").unwrap();
        assert_eq!(f.connects.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_platform_failure_reconnects_and_retries_once() {
        let f = fixture();
        f.keyring.set("session", "tokens").unwrap();
        fail_next(&f, "session", platform_failure());
        assert_eq!(f.keyring.get("session").unwrap().as_deref(), Some("tokens"));
        assert_eq!(f.connects.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_locked_store_is_reported_without_reconnecting() {
        let f = fixture();
        f.keyring.set("session", "tokens").unwrap();
        fail_next(
            &f,
            "session",
            keyring_core::Error::NoStorageAccess("locked".into()),
        );
        assert!(matches!(
            f.keyring.get("session"),
            Err(crate::Error::Keyring(keyring_core::Error::NoStorageAccess(
                _
            )))
        ));
        assert_eq!(f.connects.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_connect_is_tried_again_on_the_next_call() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let backing: Arc<CredentialStore> = keyring_core::mock::Store::new().unwrap();
        let keyring = KeyringStore::with_connector("proton-auth-rs-test", {
            let attempts = Arc::clone(&attempts);
            Arc::new(move || {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(keyring_core::Error::NoStorageAccess(
                        "no session bus".into(),
                    ))
                } else {
                    Ok(Arc::clone(&backing))
                }
            })
        });
        assert!(keyring.get("session").is_err());
        assert_eq!(keyring.get("session").unwrap(), None);
    }
}
