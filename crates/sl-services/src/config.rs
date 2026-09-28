//! Deployment configuration and the runtime facade the engine drives.
//!
//! [`ServiceConfig`] is construction-time data: which update endpoints to use, which public key
//! verifies feature tokens, and which version is running. All of it is optional and caller-supplied;
//! an empty config yields a [`Services`] that reports everything as not configured and grants no
//! features. See `docs/SERVICES.md`.

use crate::token::{FeatureToken, Features, TokenVerifier};
use crate::update::{UpdateEndpoints, UpdateStatus, Updater};
use crate::{Error, Result};
use chrono::{DateTime, Utc};
use std::sync::Mutex;

/// How to reach and trust the private services. Every field is optional.
#[derive(Debug, Clone, Default)]
pub struct ServiceConfig {
    /// Update endpoints; `None` disables the update check.
    pub updates: Option<UpdateEndpoints>,
    /// PEM `SubjectPublicKeyInfo` that verifies feature tokens; `None` disables token verification.
    pub token_public_key_pem: Option<String>,
    /// The running version compared against the update manifest.
    pub current_version: String,
    /// Overrides the generic `sideport/<version>` user agent for update requests.
    pub user_agent: Option<String>,
}

/// The features currently unlocked, and the token they came from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FeatureState {
    pub token_present: bool,
    pub subject: Option<String>,
    pub expires: Option<DateTime<Utc>>,
    pub features: Features,
}

impl FeatureState {
    fn from_token(token: FeatureToken) -> Self {
        Self { token_present: true, subject: token.subject, expires: token.expires, features: token.features }
    }
}

/// A snapshot of what is configured and what is currently unlocked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicesStatus {
    pub updates_configured: bool,
    pub token_verifier_configured: bool,
    pub feature_state: FeatureState,
}

/// The runtime object the engine holds. Built once from a [`ServiceConfig`].
#[derive(Debug)]
pub struct Services {
    updater: Option<Updater>,
    verifier: Option<TokenVerifier>,
    current_version: String,
    feature_state: Mutex<FeatureState>,
}

impl Services {
    /// Build the runtime services. Fails only when a configured public key does not parse.
    pub fn new(config: ServiceConfig) -> Result<Self> {
        let user_agent = config.user_agent.unwrap_or_else(default_user_agent);

        let updater = config.updates.map(|endpoints| Updater::new(endpoints, &user_agent)).transpose()?;
        let verifier = config.token_public_key_pem.as_deref().map(TokenVerifier::from_public_key_pem).transpose()?;

        Ok(Self {
            updater,
            verifier,
            current_version: config.current_version,
            feature_state: Mutex::new(FeatureState::default()),
        })
    }

    /// A clonable updater for callers that drive it on their own executor; `None` when updates are
    /// not configured.
    pub fn updater(&self) -> Option<Updater> {
        self.updater.clone()
    }

    /// The running version this deployment reports to the update manifest.
    pub fn current_version(&self) -> &str {
        &self.current_version
    }

    /// A configuration and feature snapshot.
    pub fn status(&self) -> ServicesStatus {
        ServicesStatus {
            updates_configured: self.updater.is_some(),
            token_verifier_configured: self.verifier.is_some(),
            feature_state: self.feature_state(),
        }
    }

    /// The features currently unlocked.
    pub fn feature_state(&self) -> FeatureState {
        self.feature_state.lock().expect("feature state mutex").clone()
    }

    /// Check for an update, or report [`UpdateStatus::NotConfigured`] when no endpoints are set.
    pub async fn check_update(&self) -> Result<UpdateStatus> {
        match &self.updater {
            Some(updater) => updater.check(&self.current_version).await,
            None => Ok(UpdateStatus::NotConfigured),
        }
    }

    /// Validate a received token into feature state and remember it. This is the services-side
    /// entry point the engine's local IPC `/tokens` route calls with the returned `user_token`.
    /// Fails when no verifier is configured or the token does not verify.
    pub fn apply_token(&self, token: &str, now: DateTime<Utc>) -> Result<FeatureState> {
        let verifier = self.verifier.as_ref().ok_or(Error::Token("no verifier configured"))?;
        let verified = verifier.verify(token, now)?;

        let state = FeatureState::from_token(verified);
        *self.feature_state.lock().expect("feature state mutex") = state.clone();

        Ok(state)
    }

    /// Forget the current token (e.g. on logout), reverting to the built-in feature set.
    pub fn clear_token(&self) {
        *self.feature_state.lock().expect("feature state mutex") = FeatureState::default();
    }
}

fn default_user_agent() -> String {
    concat!("sideport/", env!("CARGO_PKG_VERSION")).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_config_reports_nothing_configured_and_no_features() {
        let services = Services::new(ServiceConfig::default()).expect("services");
        let status = services.status();

        assert!(!status.updates_configured);
        assert!(!status.token_verifier_configured);
        assert_eq!(status.feature_state, FeatureState::default());
    }

    #[tokio::test]
    async fn an_unconfigured_update_check_reports_not_configured() {
        let services = Services::new(ServiceConfig::default()).expect("services");

        assert_eq!(services.check_update().await.expect("check"), UpdateStatus::NotConfigured);
    }

    #[test]
    fn applying_a_token_without_a_verifier_is_an_error() {
        let services = Services::new(ServiceConfig::default()).expect("services");
        let error = services.apply_token("a.b.c", Utc::now()).expect_err("no verifier");

        assert!(matches!(error, Error::Token("no verifier configured")));
    }
}
