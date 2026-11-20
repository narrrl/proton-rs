//! Single-account authentication. Passwords live only in a pending attempt;
//! only tokens and derived key passphrases enter the platform secret vault.
use proton_drive_rs::ProtonDriveClient;
use proton_gallery_rs::{CancellationToken, model::AccountId};
use proton_sdk::{
    api::{HumanVerificationCredential, ResponseCode},
    error::ProtonError,
    http::Tokens,
    session::{PasswordMode, ProtonApiSession, ResumeParameters},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use zeroize::{Zeroize, Zeroizing};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AuthError(pub &'static str);
pub type Result<T> = std::result::Result<T, AuthError>;
pub trait CredentialVault: Send + Sync {
    fn load(&self) -> Result<Option<Vec<u8>>>;
    fn save(&self, bytes: &[u8]) -> Result<()>;
    fn clear(&self) -> Result<()>;
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    #[default]
    SignedOut,
    SecondFactor,
    MailboxPassword,
    Verification,
    Connected,
}
#[derive(Clone, Default)]
pub struct Status {
    pub stage: Stage,
    pub username: String,
    pub verification_url: Option<String>,
    pub persistence_failed: bool,
}
#[derive(Clone)]
pub struct Account {
    pub id: AccountId,
    pub session: ProtonApiSession,
    pub drive: ProtonDriveClient,
    pub cancel: CancellationToken,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u8,
    session_id: String,
    username: String,
    user_id: String,
    access_token: String,
    refresh_token: String,
    scopes: Vec<String>,
    password_mode: u8,
    key_passphrases: HashMap<String, String>,
}
impl Drop for Saved {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
        for value in self.key_passphrases.values_mut() {
            value.zeroize();
        }
    }
}
impl Saved {
    fn restore(&self) -> Result<(ProtonApiSession, ProtonDriveClient)> {
        if self.version != 1
            || !matches!(self.password_mode, 1 | 2)
            || self.key_passphrases.is_empty()
        {
            return Err(AuthError("This saved session needs a fresh sign-in."));
        }
        let session = ProtonApiSession::resume(
            super::client_configuration(),
            ResumeParameters {
                session_id: self.session_id.clone().into(),
                username: self.username.clone(),
                user_id: self.user_id.clone().into(),
                access_token: self.access_token.clone(),
                refresh_token: self.refresh_token.clone(),
                scopes: self.scopes.clone(),
                is_waiting_for_second_factor_code: false,
                password_mode: if self.password_mode == 1 {
                    PasswordMode::Single
                } else {
                    PasswordMode::Dual
                },
            },
        )
        .map_err(|_| AuthError("The saved session couldn’t be restored. Sign in again."))?;
        let drive = ProtonDriveClient::with_key_passphrases(
            &session,
            self.key_passphrases
                .iter()
                .map(|(id, p)| (id.as_str(), p.as_bytes()))
                .collect(),
        );
        Ok((session, drive))
    }
    fn encode(&self) -> Result<Zeroizing<Vec<u8>>> {
        serde_json::to_vec(self)
            .map(Zeroizing::new)
            .map_err(|_| AuthError("The session couldn’t be saved securely."))
    }
}
struct Gate {
    generation: u64,
    failed: bool,
}
struct Pending {
    session: ProtonApiSession,
    password: Zeroizing<Vec<u8>>,
}
#[derive(Default)]
struct State {
    pending: Option<Pending>,
    verification: Option<(String, Zeroizing<Vec<u8>>)>,
    account: Option<Account>,
}
pub struct AuthService {
    vault: Arc<dyn CredentialVault>,
    gate: Arc<Mutex<Gate>>,
    state: tokio::sync::Mutex<State>,
    status: Arc<RwLock<Status>>,
    active_id: RwLock<Option<AccountId>>,
    cancel: RwLock<CancellationToken>,
}
// Does not hold a session/HTTP client: the callback cannot create an Arc cycle.
struct RefreshWriter {
    vault: Arc<dyn CredentialVault>,
    gate: Arc<Mutex<Gate>>,
    template: Saved,
    generation: u64,
}
impl RefreshWriter {
    fn write(&self, tokens: Tokens) {
        let mut guard = self.gate.lock().unwrap();
        if guard.generation != self.generation {
            return;
        }
        let mut saved = self.template.clone();
        saved.access_token = tokens.access_token;
        saved.refresh_token = tokens.refresh_token;
        guard.failed = match saved.encode() {
            Ok(bytes) => self.vault.save(&bytes).is_err(),
            Err(_) => true,
        };
    }
}
impl AuthService {
    pub fn new(vault: Arc<dyn CredentialVault>) -> Self {
        Self {
            vault,
            gate: Arc::new(Mutex::new(Gate {
                generation: 0,
                failed: false,
            })),
            state: tokio::sync::Mutex::new(State::default()),
            status: Arc::new(RwLock::new(Status::default())),
            active_id: RwLock::new(None),
            cancel: RwLock::new(CancellationToken::new()),
        }
    }
    /// Use the host's live session without installing a second token refresh callback.
    pub async fn hosted(
        vault: Arc<dyn CredentialVault>,
        session: ProtonApiSession,
        drive: ProtonDriveClient,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let service = Self::new(vault);
        let id = AccountId::new(session.user_id().as_str())
            .map_err(|_| AuthError("The host account is invalid."))?;
        let username = session.username().to_owned();
        service.state.lock().await.account = Some(Account {
            id: id.clone(),
            session,
            drive,
            cancel: cancel.clone(),
        });
        *service.active_id.write().unwrap() = Some(id);
        *service.cancel.write().unwrap() = cancel;
        service.publish(Stage::Connected, username, None);
        Ok(service)
    }

    pub fn status(&self) -> Status {
        let mut value = self.status.read().unwrap().clone();
        value.persistence_failed = self.gate.lock().unwrap().failed;
        value
    }
    pub fn account_id(&self) -> Option<AccountId> {
        let status = self.status.read().unwrap();
        if status.stage != Stage::Connected {
            return None;
        }
        // The immutable account ID is stored separately from the display username.
        self.active_id.read().unwrap().clone()
    }
    fn publish(&self, stage: Stage, username: String, url: Option<String>) {
        *self.status.write().unwrap() = Status {
            stage,
            username,
            verification_url: url,
            persistence_failed: false,
        };
    }
    fn attach_refresh(&self, session: &ProtonApiSession, template: Saved, generation: u64) {
        let writer = RefreshWriter {
            vault: self.vault.clone(),
            gate: self.gate.clone(),
            template,
            generation,
        };
        session
            .http()
            .set_on_tokens_refreshed(move |tokens| writer.write(tokens));
    }
    pub async fn restore(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.account.is_some() {
            return Ok(());
        }
        let Some(bytes) = self.vault.load()?.map(Zeroizing::new) else {
            return Ok(());
        };
        if bytes.len() > 1024 * 1024 {
            return Err(AuthError("The saved session is unreadable. Sign in again."));
        }
        let saved: Saved = serde_json::from_slice(&bytes)
            .map_err(|_| AuthError("The saved session is unreadable. Sign in again."))?;
        let (session, drive) = saved.restore()?;
        let id = AccountId::new(saved.user_id.clone())
            .map_err(|_| AuthError("The saved account is invalid."))?;
        let generation = self.gate.lock().unwrap().generation;
        self.attach_refresh(&session, saved.clone(), generation);
        let cancel = self.cancel.read().unwrap().clone();
        state.account = Some(Account {
            id: id.clone(),
            session,
            drive,
            cancel,
        });
        *self.active_id.write().unwrap() = Some(id);
        self.publish(Stage::Connected, saved.username.clone(), None);
        Ok(())
    }
    pub async fn account(&self) -> Result<Account> {
        self.state
            .lock()
            .await
            .account
            .clone()
            .filter(|a| !a.cancel.is_cancelled())
            .ok_or(AuthError("Sign in to your Proton account first."))
    }
    /// Interrupt an attempt/request immediately, before waiting for the state mutex.
    pub fn cancel(&self) {
        self.cancel.read().unwrap().cancel();
    }
    /// Interrupt a challenge without racing a successful account commit.
    pub fn cancel_pending(&self) {
        let _gate = self.gate.lock().unwrap();
        if self.status.read().unwrap().stage != Stage::Connected {
            self.cancel();
        }
    }
    pub async fn reset(&self, sign_out: bool) -> Result<()> {
        // Serialize with refresh saves. A late refresh can never recreate a cleared session.
        let mut cleanup_error = None;
        {
            let mut gate = self.gate.lock().unwrap();
            // Closing a dialog can race successful sign-in. A completed account
            // must remain usable; commit and cancellation share this gate.
            if !sign_out && self.status.read().unwrap().stage == Stage::Connected {
                return Ok(());
            }
            self.cancel();
            if sign_out {
                cleanup_error = self.vault.clear().err();
            }
            gate.generation += 1;
            gate.failed = false;
        }
        let mut state = self.state.lock().await;
        let old = state
            .account
            .take()
            .map(|a| a.session)
            .or_else(|| state.pending.take().map(|p| p.session));
        *state = State::default();
        *self.active_id.write().unwrap() = None;
        *self.cancel.write().unwrap() = CancellationToken::new();
        self.publish(Stage::SignedOut, String::new(), None);
        // Revocation is best effort, after local sign-out. Offline sign-out remains immediate.
        if let Some(session) = old {
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(8), session.end()).await;
            });
        }
        cleanup_error.map_or(Ok(()), Err)
    }
    pub async fn begin(
        &self,
        username: String,
        password: Vec<u8>,
        answer: Option<String>,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.account.is_some() {
            return Err(AuthError("Sign out before changing accounts."));
        }
        let password = Zeroizing::new(password);
        let (username, password) = if answer.is_some() {
            state
                .verification
                .take()
                .ok_or(AuthError("Start sign-in again to verify this attempt."))?
        } else {
            if username.trim().is_empty()
                || password.is_empty()
                || username.len() > 512
                || password.len() > 4096
            {
                return Err(AuthError("Enter your Proton username and password."));
            }
            state.pending = None;
            state.verification = None;
            self.publish(Stage::SignedOut, username.trim().to_owned(), None);
            (username.trim().to_owned(), password)
        };
        let mut credential = answer.map(HumanVerificationCredential::captcha);
        let cancel = self.cancel.read().unwrap().clone();
        let result = bounded(
            &cancel,
            ProtonApiSession::begin_verified(
                super::client_configuration(),
                &username,
                &password,
                credential.as_ref(),
            ),
        )
        .await;
        if let Some(ref mut c) = credential {
            c.token.zeroize();
        }
        match result {
            Ok(Ok(session)) => {
                let second = session.is_waiting_for_second_factor();
                if second && !session.second_factor_methods().totp {
                    return Err(AuthError(
                        "This account requires a security key. Enable an authenticator app in Proton settings to sign in here.",
                    ));
                }
                state.pending = Some(Pending { session, password });
                if second {
                    self.publish(Stage::SecondFactor, username, None);
                    Ok(())
                } else {
                    self.advance(&mut state, &cancel).await
                }
            }
            Ok(Err(ProtonError::Api(ref e)))
                if e.human_verification().is_some_and(|v| v.supports_captcha()) =>
            {
                let challenge = e.human_verification().unwrap();
                // Construct a captcha-only URL; never interpolate server method names into URLs.
                let challenge = proton_sdk::api::HumanVerification {
                    token: challenge.token,
                    methods: vec!["captcha".into()],
                };
                state.verification = Some((username.clone(), password));
                self.publish(
                    Stage::Verification,
                    username,
                    Some(challenge.verification_url()),
                );
                Ok(())
            }
            Ok(Err(e)) => {
                self.publish(Stage::SignedOut, username, None);
                Err(friendly(&e))
            }
            Err(e) => {
                self.publish(Stage::SignedOut, username, None);
                Err(e)
            }
        }
    }
    pub async fn second_factor(&self, code: String) -> Result<()> {
        let code = Zeroizing::new(code);
        if code.len() != 6 || !code.bytes().all(|c| c.is_ascii_digit()) {
            return Err(AuthError(
                "Enter the six-digit code from your authenticator.",
            ));
        }
        let mut state = self.state.lock().await;
        let mut pending = state
            .pending
            .take()
            .ok_or(AuthError("Start sign-in again."))?;
        let cancel = self.cancel.read().unwrap().clone();
        let result = bounded(&cancel, pending.session.apply_second_factor_code(&code)).await;
        if !matches!(result, Ok(Ok(()))) {
            self.publish(Stage::SignedOut, String::new(), None);
            return Err(AuthError(
                "The code wasn’t accepted. Sign in again with a fresh code.",
            ));
        }
        state.pending = Some(pending);
        self.advance(&mut state, &cancel).await
    }
    async fn advance(&self, state: &mut State, cancel: &CancellationToken) -> Result<()> {
        let p = state
            .pending
            .as_mut()
            .ok_or(AuthError("Start sign-in again."))?;
        if p.session.password_mode() == PasswordMode::Dual {
            p.password.zeroize();
            self.publish(
                Stage::MailboxPassword,
                p.session.username().to_owned(),
                None,
            );
            Ok(())
        } else {
            // A failed unlock or vault write can retry with the already verified session.
            self.publish(
                Stage::MailboxPassword,
                p.session.username().to_owned(),
                None,
            );
            self.finish(state, None, cancel).await
        }
    }
    pub async fn mailbox_password(&self, password: Vec<u8>) -> Result<()> {
        let password = Zeroizing::new(password);
        if password.is_empty() || password.len() > 4096 {
            return Err(AuthError("Enter your mailbox password."));
        }
        let mut state = self.state.lock().await;
        let cancel = self.cancel.read().unwrap().clone();
        self.finish(&mut state, Some(password), &cancel).await
    }
    async fn finish(
        &self,
        state: &mut State,
        password: Option<Zeroizing<Vec<u8>>>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let pending = state
            .pending
            .as_ref()
            .ok_or(AuthError("Start sign-in again."))?;
        let password = password.as_deref().unwrap_or(&pending.password);
        let session = &pending.session;
        let probe = ProtonDriveClient::new(session, password.to_vec());
        let key_passphrases = bounded(cancel, async {
            probe.account().unlock().await?;
            let phrases = probe.account().key_passphrases().await?;
            phrases
                .iter()
                .map(|(id, p)| {
                    String::from_utf8(p.to_vec())
                        .map(|p| (id.to_owned(), p))
                        .map_err(|_| ProtonError::invalid_operation("invalid derived key"))
                })
                .collect::<std::result::Result<HashMap<_, _>, _>>()
        })
        .await?
        .map_err(|e| friendly(&e))?;
        let Tokens {
            access_token,
            refresh_token,
        } = session.current_tokens().await;
        let saved = Saved {
            version: 1,
            session_id: session.session_id().as_str().to_owned(),
            username: session.username().to_owned(),
            user_id: session.user_id().as_str().to_owned(),
            access_token,
            refresh_token,
            scopes: session.scopes().to_vec(),
            password_mode: if session.password_mode() == PasswordMode::Single {
                1
            } else {
                2
            },
            key_passphrases,
        };
        let (resumed, drive) = saved.restore()?;
        let id = AccountId::new(saved.user_id.clone())
            .map_err(|_| AuthError("Invalid account identifier."))?;
        {
            let gate = self.gate.lock().unwrap();
            if cancel.is_cancelled() {
                return Err(AuthError("Sign-in cancelled."));
            }
            self.vault.save(&saved.encode()?)?;
            self.attach_refresh(&resumed, saved.clone(), gate.generation);
            state.account = Some(Account {
                id: id.clone(),
                session: resumed,
                drive,
                cancel: cancel.clone(),
            });
            state.pending = None;
            state.verification = None;
            *self.active_id.write().unwrap() = Some(id);
            self.publish(Stage::Connected, saved.username.clone(), None);
        }
        Ok(())
    }
}
async fn bounded<T>(
    cancel: &CancellationToken,
    work: impl std::future::Future<Output = T>,
) -> Result<T> {
    tokio::select! {biased; _=cancel.cancelled()=>Err(AuthError("Request cancelled.")), result=tokio::time::timeout(Duration::from_secs(90),work)=>result.map_err(|_|AuthError("Proton took too long to respond. Check your connection and try again."))}
}
/// The full cause chain of an error, for diagnostics only. Never shown to the
/// user: the friendly message is what the interface displays.
pub fn cause_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn friendly(error: &ProtonError) -> AuthError {
    // Transport and unclassified failures collapse into one generic message;
    // log the real cause (DNS, TLS, timeout, ...) so it can be diagnosed.
    // Request URLs and status codes appear here, never credentials.
    if !matches!(error, ProtonError::KeysLocked | ProtonError::Api(_)) {
        tracing::warn!("sign-in failed: {}", cause_chain(error));
    }
    AuthError(match error {
        ProtonError::Transport(_) => "Couldn’t reach Proton. Check your connection and try again.",
        ProtonError::KeysLocked => {
            "That password didn’t unlock your account. Check your mailbox password."
        }
        ProtonError::Api(e) if e.code == ResponseCode::TooManyRequests => {
            "Too many attempts. Wait a little before trying again."
        }
        ProtonError::Api(e) if e.code == ResponseCode::Blocked => {
            "Proton blocked this sign-in attempt. Try again later or check your account in Proton’s app."
        }
        ProtonError::Api(e) if e.is_human_verification_required() => {
            "Proton requires a verification method this app doesn’t support yet."
        }
        ProtonError::Api(e) if e.is_unauthorized() || e.is_invalid_refresh_token() => {
            "Your session expired. Sign out and sign in again."
        }
        _ => "Sign-in couldn’t be completed. Check your details and try again.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    #[derive(Default)]
    struct Vault {
        bytes: Mutex<Option<Vec<u8>>>,
        fail: AtomicBool,
        fail_clear: AtomicBool,
    }
    impl CredentialVault for Vault {
        fn load(&self) -> Result<Option<Vec<u8>>> {
            Ok(self.bytes.lock().unwrap().clone())
        }
        fn save(&self, b: &[u8]) -> Result<()> {
            if self.fail.load(Ordering::Relaxed) {
                return Err(AuthError("disk full"));
            }
            *self.bytes.lock().unwrap() = Some(b.to_vec());
            Ok(())
        }
        fn clear(&self) -> Result<()> {
            if self.fail_clear.load(Ordering::Relaxed) {
                return Err(AuthError("keyring locked"));
            }
            *self.bytes.lock().unwrap() = None;
            Ok(())
        }
    }
    fn saved() -> Saved {
        Saved {
            version: 1,
            session_id: "session".into(),
            username: "test-user".into(),
            user_id: "immutable-id".into(),
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            scopes: vec!["full".into()],
            password_mode: 1,
            key_passphrases: HashMap::from([("key".into(), "derived-unlock-key".into())]),
        }
    }
    fn tokens() -> Tokens {
        Tokens {
            access_token: "rotated-access".into(),
            refresh_token: "rotated-refresh".into(),
        }
    }
    #[tokio::test]
    async fn resume_is_offline_and_preserves_immutable_identity_without_a_password() {
        let vault = Arc::new(Vault::default());
        vault.save(&saved().encode().unwrap()).unwrap();
        let auth = AuthService::new(vault.clone());
        auth.restore().await.unwrap();
        assert_eq!(auth.status().stage, Stage::Connected);
        assert_eq!(auth.account_id().unwrap().as_str(), "immutable-id");
        let payload: serde_json::Value =
            serde_json::from_slice(&vault.load().unwrap().unwrap()).unwrap();
        assert!(payload.get("password").is_none());
        assert!(payload.get("mailbox_password").is_none());
        // A late dialog dismissal must not cancel a just-committed account.
        auth.cancel_pending();
        auth.reset(false).await.unwrap();
        assert!(auth.account().await.is_ok());
        assert!(vault.load().unwrap().is_some());
        auth.cancel();
        assert!(auth.account().await.is_err());
    }
    #[tokio::test]
    async fn late_token_refresh_cannot_resurrect_a_signed_out_account() {
        let vault = Arc::new(Vault::default());
        let auth = AuthService::new(vault.clone());
        let writer = RefreshWriter {
            vault: vault.clone(),
            gate: auth.gate.clone(),
            template: saved(),
            generation: 0,
        };
        writer.write(tokens());
        assert!(vault.load().unwrap().is_some());
        auth.reset(true).await.unwrap();
        writer.write(tokens());
        assert!(vault.load().unwrap().is_none());
        assert!(auth.account_id().is_none());
    }
    #[tokio::test]
    async fn refresh_persistence_failures_surface_and_a_later_save_recovers() {
        let vault = Arc::new(Vault::default());
        let auth = AuthService::new(vault.clone());
        let writer = RefreshWriter {
            vault: vault.clone(),
            gate: auth.gate.clone(),
            template: saved(),
            generation: 0,
        };
        vault.fail.store(true, Ordering::Relaxed);
        writer.write(tokens());
        assert!(auth.status().persistence_failed);
        vault.fail.store(false, Ordering::Relaxed);
        writer.write(tokens());
        assert!(!auth.status().persistence_failed);
        let restored: Saved = serde_json::from_slice(&vault.load().unwrap().unwrap()).unwrap();
        assert_eq!(restored.refresh_token, "rotated-refresh");
    }
    #[tokio::test]
    async fn corrupt_or_future_session_is_reported_without_deleting_the_blob() {
        let vault = Arc::new(Vault::default());
        vault.save(b"corrupt").unwrap();
        let auth = AuthService::new(vault.clone());
        assert!(auth.restore().await.is_err());
        assert!(vault.load().unwrap().is_some());
        let mut future = saved();
        future.version = 2;
        vault.save(&future.encode().unwrap()).unwrap();
        assert!(auth.restore().await.is_err());
        assert_eq!(auth.status().stage, Stage::SignedOut);
    }
    #[test]
    fn api_errors_never_echo_server_payloads() {
        let e = ProtonError::Api(proton_sdk::error::ProtonApiError {
            code: ResponseCode::Unknown,
            http_status: 400,
            message: "secret access token".into(),
            details: None,
        });
        assert!(!friendly(&e).to_string().contains("secret"));
    }
    #[tokio::test]
    async fn cancelling_verification_drops_its_credentials_and_rejects_late_tokens() {
        let auth = AuthService::new(Arc::new(Vault::default()));
        auth.state.lock().await.verification =
            Some(("user".into(), Zeroizing::new(b"password".to_vec())));
        auth.publish(
            Stage::Verification,
            "user".into(),
            Some("https://verify.proton.me/".into()),
        );
        let old = auth.cancel.read().unwrap().clone();
        auth.reset(false).await.unwrap();
        assert!(old.is_cancelled());
        assert!(auth.state.lock().await.verification.is_none());
        assert!(
            auth.begin(String::new(), vec![], Some("late token".into()))
                .await
                .is_err()
        );
        assert_eq!(auth.status().stage, Stage::SignedOut);
    }
    #[tokio::test]
    async fn failed_keyring_deletion_still_invalidates_old_refresh_writers() {
        let vault = Arc::new(Vault::default());
        let bytes = saved().encode().unwrap();
        vault.save(&bytes).unwrap();
        let auth = AuthService::new(vault.clone());
        auth.restore().await.unwrap();
        let old = auth.account().await.unwrap();
        let writer = RefreshWriter {
            vault: vault.clone(),
            gate: auth.gate.clone(),
            template: saved(),
            generation: 0,
        };
        vault.fail_clear.store(true, Ordering::Relaxed);
        assert!(auth.reset(true).await.is_err());
        assert!(old.cancel.is_cancelled());
        assert_eq!(auth.status().stage, Stage::SignedOut);
        writer.write(tokens());
        assert_eq!(vault.load().unwrap().unwrap().as_slice(), bytes.as_slice());
        vault.fail_clear.store(false, Ordering::Relaxed);
        auth.reset(true).await.unwrap();
        assert!(vault.load().unwrap().is_none());
    }
}
