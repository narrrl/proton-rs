//! Signing in to a Proton account (feature `session`).
//!
//! Every app ran the same steps in its own copy: password (restarted with a
//! solved CAPTCHA when Proton asks), then a second-factor code, then a
//! mailbox password on accounts that have one, then deriving the key
//! passphrases while the token still has the scope the key salts need. This
//! is that sequence once, as a small state machine each frontend drives in its
//! own way.
//!
//! What is *stored* stays with each app: its own struct and serde layout, so
//! no existing sign-in is lost. [`SessionTokens`] is the common part of those
//! layouts, with the conversions every copy had written by hand.
//!
//! ```no_run
//! # async fn demo(config: proton_sdk::config::ProtonClientConfiguration) -> Result<(), proton_auth_rs::session::SignInError> {
//! use proton_auth_rs::session::{self, Step};
//!
//! let mut step = session::begin(config, "user@proton.me", "password", None).await?;
//! let unlocked = loop {
//!     step = match step {
//!         Step::SecondFactor(pending) => pending.second_factor("123456").await?,
//!         Step::MailboxPassword(pending) => {
//!             let unlocked = pending.mailbox_password("mailbox").await?;
//!             break unlocked;
//!         }
//!         Step::Unlock(pending) => break pending.unlock().await?,
//!     };
//! };
//! let tokens = unlocked.tokens().await;
//! // Store `tokens` and `unlocked.key_passphrases`, then keep them current
//! // from `unlocked.session.http().set_on_tokens_refreshed`.
//! # Ok(()) }
//! ```

use proton_sdk::account::{AccountClient, KeyPassphrases, KeySalt};
use proton_sdk::api::{HumanVerification, HumanVerificationCredential, ResponseCode};
use proton_sdk::config::ProtonClientConfiguration;
use proton_sdk::error::{ProtonApiError, ProtonError};
use proton_sdk::http::Tokens;
use proton_sdk::session::{PasswordMode, ProtonApiSession, ResumeParameters};

/// Why a sign-in step failed, in terms a frontend can act on.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SignInError {
    /// Proton wants a CAPTCHA solved first. Show
    /// [`HumanVerification::verification_url`], then call [`begin`] again with
    /// the token (see [`verification_token`]). Only CAPTCHA gates land here;
    /// one that wants an e-mail or SMS code is [`SignInError::Rejected`].
    #[error("Proton asks to confirm this sign-in is made by a person")]
    HumanVerification(Box<HumanVerification>),
    /// The account's only second factor is a security key, which needs
    /// WebAuthn. The half-made session has been ended.
    #[error(
        "This account's only second factor is a security key, which this app cannot use yet. \
         Add an authenticator app in your Proton account settings, then sign in with its code."
    )]
    SecurityKeyOnly,
    /// Proton blocks sign-ins from this app for now. Its own message claims
    /// the app version is unsupported, which is not what is wrong, so the
    /// display text is this crate's; the response is kept for apps that word
    /// it themselves.
    #[error(
        "Proton has blocked sign-ins from this app for now. Wait a while and try again, \
         or sign in once on proton.me first."
    )]
    Blocked(ProtonApiError),
    /// Proton refused the address and password. Displays Proton's reason,
    /// which is written for people; apps that must not show server text can
    /// use their own.
    #[error("{}", rejected_text(.0))]
    Rejected(ProtonApiError),
    /// The second-factor code was not accepted. Proton does not take a second
    /// try on the same half-made session: start over with [`begin`].
    #[error("That code was not accepted.")]
    WrongCode(ProtonApiError),
    /// The mailbox password did not unlock the account. The pending sign-in
    /// survives; ask again.
    #[error("That mailbox password did not unlock the account.")]
    WrongMailboxPassword,
    /// Anything else: network, server, protocol.
    #[error(transparent)]
    Proton(#[from] ProtonError),
}

/// Where a sign-in stands.
///
/// Every sign-in ends with a call that unlocks the keys by reference,
/// [`Pending::mailbox_password`] or [`Pending::unlock`], so a failed unlock (a
/// dropped connection, a mistyped mailbox password) can be retried on the
/// same session without asking for the password and code again.
pub enum Step {
    /// Ask for a second-factor code and pass it to [`Pending::second_factor`].
    SecondFactor(Pending),
    /// Ask for the mailbox password and pass it to [`Pending::mailbox_password`].
    MailboxPassword(Pending),
    /// The account has one password and it was accepted: call
    /// [`Pending::unlock`].
    Unlock(Pending),
}

/// A sign-in waiting on the user.
pub struct Pending {
    session: ProtonApiSession,
    password: zeroize::Zeroizing<String>,
}

/// A finished sign-in.
pub struct Unlocked {
    pub session: ProtonApiSession,
    /// Key id to the passphrase that unlocks it. Apps store these instead of
    /// the mailbox password: they unlock the key chain just the same, but a
    /// leaked store gives away this app's access rather than the password.
    pub key_passphrases: KeyPassphrases,
    /// The account's key salts, for apps that re-derive passphrases later
    /// (such as one that locks and unlocks with the mailbox password). Only a
    /// fresh password sign-in may read them.
    pub key_salts: Vec<KeySalt>,
}

impl Unlocked {
    /// The session's tokens as they are now, for storing.
    pub async fn tokens(&self) -> SessionTokens {
        SessionTokens::capture(&self.session, self.session.current_tokens().await)
    }
}

/// Start signing in.
///
/// `verification` is the token of a solved CAPTCHA after a first try failed
/// with [`SignInError::HumanVerification`]; the sign-in starts over with it,
/// because the gated attempt's SRP handshake is spent.
pub async fn begin(
    config: ProtonClientConfiguration,
    username: &str,
    password: &str,
    verification: Option<&HumanVerificationCredential>,
) -> Result<Step, SignInError> {
    let session = ProtonApiSession::begin_verified(
        config,
        username.trim(),
        password.as_bytes(),
        verification,
    )
    .await
    .map_err(classify_begin_failure)?;
    let pending = Pending {
        session,
        password: zeroize::Zeroizing::new(password.to_owned()),
    };
    if pending.session.is_waiting_for_second_factor() {
        let methods = pending.session.second_factor_methods();
        if methods.fido2 && !methods.totp {
            // Asking for a code would only fail as a wrong one.
            pending.session.end().await.ok();
            return Err(SignInError::SecurityKeyOnly);
        }
        return Ok(Step::SecondFactor(pending));
    }
    Ok(pending.after_second_factor())
}

impl Pending {
    /// The half-made session, for apps that need its username or want to end
    /// it when the user cancels.
    pub fn session(&self) -> &ProtonApiSession {
        &self.session
    }

    /// Submit a second-factor code. Consumes the sign-in: a wrong code means
    /// starting over (see [`SignInError::WrongCode`]).
    pub async fn second_factor(mut self, code: &str) -> Result<Step, SignInError> {
        self.session
            .apply_second_factor_code(code.trim())
            .await
            .map_err(|error| match error {
                ProtonError::Api(api) if (400..500).contains(&api.http_status) => {
                    SignInError::WrongCode(api)
                }
                other => other.into(),
            })?;
        Ok(self.after_second_factor())
    }

    /// Submit the mailbox password. By reference: a wrong one is detected on
    /// this side, the session survives it, and the user can try again.
    pub async fn mailbox_password(&self, mailbox_password: &str) -> Result<Unlocked, SignInError> {
        unlock(self.session.clone(), mailbox_password).await
    }

    /// Unlock the keys with the login password, on an account that has one
    /// password. By reference, so a failure can be retried.
    pub async fn unlock(&self) -> Result<Unlocked, SignInError> {
        unlock(self.session.clone(), &self.password).await
    }

    fn after_second_factor(self) -> Step {
        match self.session.password_mode() {
            PasswordMode::Dual => Step::MailboxPassword(self),
            PasswordMode::Single => Step::Unlock(self),
        }
    }
}

/// Unlock the key chain with `mailbox_password` and derive the passphrases.
///
/// Unlocking the user keys is what proves the mailbox password: a wrong one
/// unlocks none of them.
async fn unlock(
    session: ProtonApiSession,
    mailbox_password: &str,
) -> Result<Unlocked, SignInError> {
    let account = AccountClient::new(&session, mailbox_password.as_bytes().to_vec());
    // Salts first: they need the scope a password sign-in grants, which the
    // first token refresh drops.
    let key_salts = account.key_salts().await?;
    account.unlock().await.map_err(|error| match error {
        ProtonError::KeysLocked => SignInError::WrongMailboxPassword,
        other => other.into(),
    })?;
    let key_passphrases = account.key_passphrases().await?;
    Ok(Unlocked {
        session,
        key_passphrases,
        key_salts,
    })
}

/// Lift a CAPTCHA gate, a block and a refused password out of the generic
/// API error.
fn classify_begin_failure(error: ProtonError) -> SignInError {
    if let ProtonError::Api(api) = &error
        && let Some(challenge) = api.human_verification()
        && challenge.supports_captcha()
    {
        return SignInError::HumanVerification(Box::new(challenge));
    }
    match error {
        ProtonError::Api(api) if api.code == ResponseCode::Blocked => SignInError::Blocked(api),
        ProtonError::Api(api)
            if api.http_status == 422 && !api.is_human_verification_required() =>
        {
            SignInError::Rejected(api)
        }
        other => other.into(),
    }
}

fn rejected_text(api: &ProtonApiError) -> &str {
    if api.message.is_empty() {
        "Proton did not accept that address and password"
    } else {
        &api.message
    }
}

/// The part of a stored session every app keeps: enough to resume it without
/// signing in again. Apps embed these fields in their own stored struct.
#[derive(Clone)]
pub struct SessionTokens {
    pub session_id: String,
    pub username: String,
    pub user_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub scopes: Vec<String>,
    pub password_mode: PasswordMode,
}

impl std::fmt::Debug for SessionTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTokens")
            .field("session_id", &self.session_id)
            .field("username", &self.username)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

impl SessionTokens {
    /// `session` with `tokens`: the pair from sign-in, or the pair a refresh
    /// handed to `set_on_tokens_refreshed`.
    pub fn capture(session: &ProtonApiSession, tokens: Tokens) -> Self {
        Self {
            session_id: session.session_id().as_str().to_owned(),
            username: session.username().to_owned(),
            user_id: session.user_id().as_str().to_owned(),
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            scopes: session.scopes().to_vec(),
            password_mode: session.password_mode(),
        }
    }

    /// What [`ProtonApiSession::resume`] needs.
    pub fn resume_parameters(&self) -> ResumeParameters {
        ResumeParameters {
            session_id: self.session_id.clone().into(),
            username: self.username.clone(),
            user_id: self.user_id.clone().into(),
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            scopes: self.scopes.clone(),
            is_waiting_for_second_factor_code: false,
            password_mode: self.password_mode,
        }
    }
}

/// Proton's number for a password mode, as the apps store it: `1` single
/// password, `2` separate mailbox password.
pub fn password_mode_to_wire(mode: PasswordMode) -> u8 {
    match mode {
        PasswordMode::Single => 1,
        PasswordMode::Dual => 2,
    }
}

/// The inverse of [`password_mode_to_wire`]. Anything but `1` reads as dual,
/// which at worst asks for a mailbox password that is the login password.
pub fn password_mode_from_wire(value: u8) -> PasswordMode {
    match value {
        1 => PasswordMode::Single,
        _ => PasswordMode::Dual,
    }
}

/// The token out of a message posted by Proton's verification page, or `None`
/// for anything that is not a completion.
///
/// The page posts plenty besides (resize requests, readiness pings), and
/// taking those for success would retry the sign-in with a garbage token.
/// Both spellings of the success type have shipped, and some hosts hand the
/// message over stringified twice.
pub fn verification_token(raw: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(raw).ok()?;
    if let Some(inner) = value.as_str()
        && let Ok(parsed) = serde_json::from_str(inner)
    {
        value = parsed;
    }
    let kind = value.get("type")?.as_str()?;
    if !matches!(
        kind,
        "HUMAN_VERIFICATION_SUCCESS" | "human_verification_success"
    ) {
        return None;
    }
    let token = value.get("payload")?.get("token")?.as_str()?;
    (!token.is_empty()).then(|| token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_modes_round_trip_through_proton_numbers() {
        for mode in [PasswordMode::Single, PasswordMode::Dual] {
            assert_eq!(password_mode_from_wire(password_mode_to_wire(mode)), mode);
        }
        assert_eq!(password_mode_from_wire(0), PasswordMode::Dual);
    }

    #[test]
    fn resume_parameters_carry_every_field() {
        let tokens = SessionTokens {
            session_id: "sid".into(),
            username: "user".into(),
            user_id: "uid".into(),
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            scopes: vec!["full".into()],
            password_mode: PasswordMode::Single,
        };
        let params = tokens.resume_parameters();
        assert_eq!(params.session_id.as_str(), "sid");
        assert_eq!(params.user_id.as_str(), "uid");
        assert_eq!(params.username, "user");
        assert_eq!(params.access_token, "access");
        assert_eq!(params.refresh_token, "refresh");
        assert_eq!(params.scopes, ["full"]);
        assert_eq!(params.password_mode, PasswordMode::Single);
        assert!(!params.is_waiting_for_second_factor_code);
        let shown = format!("{tokens:?}");
        assert!(
            !shown.contains("access") && !shown.contains("refresh"),
            "{shown}"
        );
    }

    fn api(code: ResponseCode, message: &str) -> ProtonError {
        ProtonError::Api(ProtonApiError {
            code,
            http_status: 422,
            message: message.to_owned(),
            details: None,
        })
    }

    #[test]
    fn a_blocked_sign_in_keeps_the_response_but_not_its_wording() {
        let error = classify_begin_failure(api(ResponseCode::Blocked, "Please update the app"));
        assert!(matches!(&error, SignInError::Blocked(api) if api.code == ResponseCode::Blocked));
        assert!(!error.to_string().contains("update"), "{error}");
    }

    #[test]
    fn a_refused_password_shows_protons_reason() {
        let error =
            classify_begin_failure(api(ResponseCode::Unknown, "Incorrect login credentials"));
        assert_eq!(error.to_string(), "Incorrect login credentials");
        let silent = classify_begin_failure(api(ResponseCode::Unknown, ""));
        assert!(matches!(silent, SignInError::Rejected(_)));
        assert!(!silent.to_string().is_empty());
    }

    #[test]
    fn only_completion_messages_yield_a_token() {
        let done = r#"{"type":"HUMAN_VERIFICATION_SUCCESS","payload":{"token":"abc"}}"#;
        assert_eq!(verification_token(done).as_deref(), Some("abc"));
        let twice = serde_json::to_string(done).unwrap();
        assert_eq!(verification_token(&twice).as_deref(), Some("abc"));
        let lower = r#"{"type":"human_verification_success","payload":{"token":"x"}}"#;
        assert_eq!(verification_token(lower).as_deref(), Some("x"));
        assert_eq!(
            verification_token(r#"{"type":"RESIZE","payload":{"height":3}}"#),
            None
        );
        assert_eq!(
            verification_token(r#"{"type":"HUMAN_VERIFICATION_SUCCESS","payload":{"token":""}}"#),
            None
        );
        assert_eq!(verification_token("not json"), None);
    }
}
