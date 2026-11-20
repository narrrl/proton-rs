//! The Proton account: restoring a saved session, signing in through its
//! challenges, and signing out without leaving the account's data behind.
use super::*;
use proton_gallery_drive_rs::auth::Stage;

/// Where sign-in stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInStage {
    SignedOut,
    /// A code from the authenticator app is needed.
    SecondFactor,
    /// The account uses a separate mailbox password.
    MailboxPassword,
    /// Proton asks for a human verification (CAPTCHA) at `verification_url`.
    Verification,
    Connected,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountState {
    pub stage: SignInStage,
    pub username: String,
    pub verification_url: Option<String>,
    /// The session works but could not be saved; it ends with the app.
    pub persistence_failed: bool,
}

impl App {
    /// Resume the session saved in the vault, if there is one.
    pub async fn restore_session(&self) -> Result<AccountState> {
        let _transition = self.transition.lock().await;
        self.ensure_clean()?;
        let before = self.auth.account_id();
        self.auth.restore().await?;
        self.after_sign_in(before);
        Ok(self.account_state())
    }

    pub fn account_state(&self) -> AccountState {
        let status = self.auth.status();
        AccountState {
            stage: match status.stage {
                Stage::SignedOut => SignInStage::SignedOut,
                Stage::SecondFactor => SignInStage::SecondFactor,
                Stage::MailboxPassword => SignInStage::MailboxPassword,
                Stage::Verification => SignInStage::Verification,
                Stage::Connected => SignInStage::Connected,
            },
            username: status.username,
            verification_url: status.verification_url,
            persistence_failed: status.persistence_failed,
        }
    }

    pub async fn sign_in(
        &self,
        username: String,
        password: zeroize::Zeroizing<String>,
    ) -> Result<AccountState> {
        let _transition = self.transition.lock().await;
        self.ensure_clean()?;
        let before = self.auth.account_id();
        self.auth
            .begin(username, password.as_bytes().to_vec(), None)
            .await?;
        self.after_sign_in(before);
        Ok(self.account_state())
    }

    pub async fn submit_second_factor(&self, code: String) -> Result<AccountState> {
        let _transition = self.transition.lock().await;
        self.ensure_clean()?;
        let before = self.auth.account_id();
        self.auth.second_factor(code).await?;
        self.after_sign_in(before);
        Ok(self.account_state())
    }

    pub async fn submit_mailbox_password(
        &self,
        password: zeroize::Zeroizing<String>,
    ) -> Result<AccountState> {
        let _transition = self.transition.lock().await;
        self.ensure_clean()?;
        let before = self.auth.account_id();
        self.auth
            .mailbox_password(password.as_bytes().to_vec())
            .await?;
        self.after_sign_in(before);
        Ok(self.account_state())
    }

    /// Continue sign-in with the token from Proton's verification page.
    pub async fn submit_verification(&self, token: String) -> Result<AccountState> {
        let _transition = self.transition.lock().await;
        self.ensure_clean()?;
        let before = self.auth.account_id();
        self.auth.begin(String::new(), vec![], Some(token)).await?;
        self.after_sign_in(before);
        Ok(self.account_state())
    }

    /// Abandon a sign-in that is waiting for a challenge. A completed
    /// sign-in stays signed in.
    pub async fn cancel_sign_in(&self) -> Result<AccountState> {
        // Interrupt a network attempt before waiting for its transition.
        self.auth.cancel_pending();
        let _transition = self.transition.lock().await;
        self.auth.reset(false).await?;
        Ok(self.account_state())
    }

    /// Sign out and forget what the catalog knew of the account: decrypted
    /// names, album titles and backup state must not outlive the session.
    pub async fn sign_out(&self) -> Result<()> {
        self.auth.cancel();
        let _transition = self.transition.lock().await;
        let account = self
            .cleanup
            .read()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.auth.account_id());
        *self.cleanup.write().unwrap() = Some(account.clone());
        self.advance();
        // Persist intent before deleting credentials; a restart must not restore a
        // credential that the platform vault refused to remove.
        let marker = self.root.join("signout-pending.json");
        let record = (|| -> Result<()> {
            use std::io::Write;
            let mut file = tempfile::NamedTempFile::new_in(&self.root)
                .map_err(|e| Error::message(e.to_string()))?;
            file.write_all(
                &serde_json::to_vec(&account).map_err(|e| Error::message(e.to_string()))?,
            )
            .map_err(|e| Error::message(e.to_string()))?;
            file.as_file()
                .sync_all()
                .map_err(|e| Error::message(e.to_string()))?;
            file.persist(&marker)
                .map_err(|e| Error::message(e.to_string()))?;
            Ok(())
        })();
        // Always detach the in-memory account, even when the vault is locked.
        let reset = self.auth.reset(true).await.map_err(Error::from);
        let _operations = self.operations.write().await;
        let _photos = self.photos_sync.lock().await;
        let _drive = self.drive_sync.lock().await;
        let _check = self.backup_check.lock().await;
        *self.photos.write().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(account) = account {
            self.catalog.forget_account(account).await?;
        }
        let originals = self.root.join("cache").join("originals");
        match std::fs::remove_dir_all(originals) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(Error::message(
                    "Cached originals couldn’t be removed. Retry sign-out cleanup.",
                ));
            }
        }
        reset?;
        record?;
        std::fs::remove_file(marker).map_err(|_| {
            Error::message("Sign-out cleanup record couldn’t be removed. Retry cleanup.")
        })?;
        *self.cleanup.write().unwrap() = None;
        Ok(())
    }

    pub fn cleanup_pending(&self) -> bool {
        self.cleanup.read().unwrap().is_some()
    }

    fn ensure_clean(&self) -> Result<()> {
        if self.cleanup_pending() {
            Err(Error::message(
                "Sign-out cleanup is incomplete. Retry cleanup before signing in.",
            ))
        } else {
            Ok(())
        }
    }

    fn after_sign_in(&self, before: Option<proton_gallery_rs::model::AccountId>) {
        if self.auth.account_id() != before {
            self.advance();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;

    #[tokio::test]
    async fn without_a_saved_session_the_app_stays_signed_out() {
        let (_dir, app) = app(Arc::default()).await;
        let state = app.restore_session().await.unwrap();
        assert_eq!(state.stage, SignInStage::SignedOut);
        assert!(app.auth.account_id().is_none());
    }

    #[tokio::test]
    async fn a_saved_session_is_restored_and_starts_a_generation() {
        let vault = Arc::new(MemoryVault::default());
        *vault.saved.lock().unwrap() = Some(saved_session("user-1"));
        let (_dir, app) = app(vault).await;
        let before = app.generation();
        let state = app.restore_session().await.unwrap();
        assert_eq!(state.stage, SignInStage::Connected);
        assert_eq!(state.username, "someone");
        assert_ne!(app.generation(), before);
        // Restoring again changes nothing.
        let now = app.generation();
        app.restore_session().await.unwrap();
        assert_eq!(app.generation(), now);
    }

    #[tokio::test]
    async fn an_unreadable_session_is_reported_and_leaves_the_app_signed_out() {
        let vault = Arc::new(MemoryVault::default());
        *vault.saved.lock().unwrap() = Some(b"not a session".to_vec());
        let (_dir, app) = app(vault).await;
        let error = app.restore_session().await.unwrap_err();
        assert!(matches!(error, Error::Message(text) if text.contains("Sign in again")));
        assert_eq!(app.account_state().stage, SignInStage::SignedOut);
    }

    #[tokio::test]
    async fn signing_out_clears_the_vault_and_makes_earlier_work_stale() {
        let vault = Arc::new(MemoryVault::default());
        *vault.saved.lock().unwrap() = Some(saved_session("user-1"));
        let (_dir, app) = app(vault.clone()).await;
        app.restore_session().await.unwrap();
        let during = app.generation();
        let account = app.auth.account().await.unwrap();
        app.sign_out().await.unwrap();
        assert_eq!(app.account_state().stage, SignInStage::SignedOut);
        assert!(vault.saved.lock().unwrap().is_none());
        assert!(account.cancel.is_cancelled());
        assert_eq!(app.ensure_current(during), Err(Error::Stale));
        assert!(app.auth.account().await.is_err());
    }

    #[tokio::test]
    async fn cancelling_a_challenge_never_signs_out_a_connected_account() {
        let vault = Arc::new(MemoryVault::default());
        *vault.saved.lock().unwrap() = Some(saved_session("user-1"));
        let (_dir, app) = app(vault).await;
        app.restore_session().await.unwrap();
        let state = app.cancel_sign_in().await.unwrap();
        assert_eq!(state.stage, SignInStage::Connected);
    }
    #[tokio::test]
    async fn keyring_failure_detaches_account_cleans_catalog_and_blocks_restore_until_retry() {
        let vault = Arc::new(MemoryVault::default());
        *vault.saved.lock().unwrap() = Some(saved_session("synthetic"));
        let (_dir, app) = app(vault.clone()).await;
        app.restore_session().await.unwrap();
        let source = proton_gallery_rs::synthetic::SyntheticSource::new(20).unwrap();
        app.index_source(
            &source,
            app.generation(),
            &proton_gallery_rs::CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap();
        let old = app.auth.account().await.unwrap();
        vault.fail_clear.store(true, Ordering::Relaxed);
        assert!(app.sign_out().await.is_err());
        assert!(old.cancel.is_cancelled());
        assert_eq!(app.account_state().stage, SignInStage::SignedOut);
        assert!(app.auth.account().await.is_err());
        assert!(app.cleanup_pending());
        assert!(
            app.outline(crate::View::Source("demo".into()))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(app.restore_session().await.is_err());
        vault.fail_clear.store(false, Ordering::Relaxed);
        app.sign_out().await.unwrap();
        assert!(!app.cleanup_pending());
        assert!(vault.saved.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn signout_waits_for_operation_publication_before_removing_account_data() {
        let vault = Arc::new(MemoryVault::default());
        *vault.saved.lock().unwrap() = Some(saved_session("user-1"));
        let (_dir, app) = app(vault).await;
        app.restore_session().await.unwrap();
        let guard = app.operations.read().await;
        let mut signout = std::pin::pin!(app.sign_out());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut signout)
                .await
                .is_err()
        );
        assert!(app.auth.account().await.is_err());
        assert!(app.cleanup_pending());
        drop(guard);
        signout.await.unwrap();
        assert!(!app.cleanup_pending());
    }
    #[tokio::test]
    async fn failed_keyring_cleanup_survives_restart_and_cannot_restore_the_saved_account() {
        let vault = Arc::new(MemoryVault::default());
        *vault.saved.lock().unwrap() = Some(saved_session("user-1"));
        let (dir, app) = app(vault.clone()).await;
        app.restore_session().await.unwrap();
        vault.fail_clear.store(true, Ordering::Relaxed);
        assert!(app.sign_out().await.is_err());
        drop(app);
        let reopened = App::open(dir.path(), vault.clone()).await.unwrap();
        assert!(reopened.cleanup_pending());
        assert!(reopened.restore_session().await.is_err());
        assert_eq!(reopened.account_state().stage, SignInStage::SignedOut);
        vault.fail_clear.store(false, Ordering::Relaxed);
        reopened.sign_out().await.unwrap();
        assert!(!dir.path().join("signout-pending.json").exists());
    }
}
