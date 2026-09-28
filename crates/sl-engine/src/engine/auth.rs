//! Non-demo GSA login, session renewal and the engine prompt bridge.

use super::{Inner, LiveAccount, anisette, state};
use crate::error::{EngineError, Result};
use crate::job::{Fact, JobContext, PromptKind, PromptReply, Stage};
use crate::types::{AccountSummary, TeamKind, TeamSummary};
use chrono::Utc;
use futures::FutureExt;
use futures::future::BoxFuture;
use sl_apple::auth::{AnisetteSources, AuthClient, AuthSession, FactorDelegate, FactorPrompt, FactorReply};
use sl_apple::portal::{PortalAccess, PortalClient, TeamKind as PortalTeamKind};
use std::sync::Arc;
use zeroize::Zeroizing;

/// GSA error for an incorrect Apple ID or password (recovered: retryable, "Login or password is
/// incorrect — or maybe you should just retry").
const INCORRECT_CREDENTIALS: i64 = -22406;

/// Where the password for an authentication attempt came from.
enum PasswordSource {
    Supplied { remember: bool },
    Prompted { remember: bool },
    Remembered,
}

impl PasswordSource {
    fn remember(&self) -> bool {
        match self {
            Self::Supplied { remember } | Self::Prompted { remember } => *remember,
            Self::Remembered => true,
        }
    }
}

pub(super) async fn login(
    inner: Arc<Inner>,
    context: JobContext,
    apple_id: String,
    password: Option<String>,
    remember: bool,
) -> Result<AccountSummary> {
    context.stage(Stage::Authenticating);

    let (password, source) = match password {
        Some(value) => (Zeroizing::new(value), PasswordSource::Supplied { remember }),
        None => match state::remembered_password(&inner, &apple_id)? {
            Some(stored) => (stored, PasswordSource::Remembered),
            None => prompt_password(&context, &apple_id, remember).await?,
        },
    };

    context.checkpoint()?;

    let (session, password, source) = authenticate_with_fallback(&inner, &context, &apple_id, password, source).await?;
    let provider = anisette::for_session(&inner, &session, Some(&context)).await?;

    let portal = PortalClient::with_origin(&inner.portal_origin).map_err(auth_error)?;
    let cancellation = context.cancellation_token();
    let access = PortalAccess::new(&session, provider.as_ref(), &cancellation);

    let teams = match portal.list_teams(access).await {
        Ok(teams) => teams.into_iter().map(team_summary).collect::<Vec<_>>(),
        Err(sl_apple::Error::Cancelled) => return Err(EngineError::Cancelled),
        Err(error) => {
            context.warn(format!("Developer team listing unavailable: {error}"));

            Vec::new()
        }
    };

    context.checkpoint()?;

    let previous_default =
        inner.accounts.lock().get(&apple_id).and_then(|account| account.summary.default_team.clone());
    let default_team = match teams.as_slice() {
        [team] => Some(team.team_id.clone()),
        teams => previous_default.filter(|team_id| teams.iter().any(|team| &team.team_id == team_id)),
    };

    if let Some(team) = teams.first().filter(|_| teams.len() == 1) {
        context.fact(Fact::Team(team.clone()));
    }

    let remember = source.remember();
    let summary = AccountSummary {
        apple_id: apple_id.clone(),
        teams,
        default_team,
        has_session: true,
        remembers_password: remember,
        last_login: Some(Utc::now()),
    };

    state::save_login(&inner, &summary, &session, remember.then_some(password.as_str()))?;
    inner.accounts.lock().insert(apple_id, LiveAccount { summary: summary.clone(), session: Some(Arc::new(session)) });
    context.info("Apple ID authentication completed");

    Ok(summary)
}

/// Obtain a new session for a known account, using its remembered password or a prompt.
/// Used when the stored session is missing or the developer service reports it expired.
pub(super) async fn renew_session(
    inner: &Arc<Inner>,
    context: &JobContext,
    apple_id: &str,
) -> Result<Arc<AuthSession>> {
    context.stage(Stage::Authenticating);

    let (password, source) = match state::remembered_password(inner, apple_id)? {
        Some(stored) => (stored, PasswordSource::Remembered),
        None => prompt_password(context, apple_id, false).await?,
    };

    let (session, password, source) = authenticate_with_fallback(inner, context, apple_id, password, source).await?;
    let session = Arc::new(session);

    let summary = {
        let mut accounts = inner.accounts.lock();
        let account = accounts.get_mut(apple_id).ok_or_else(|| EngineError::Auth("account was signed out".into()))?;

        account.session = Some(session.clone());
        account.summary.last_login = Some(Utc::now());
        account.summary.remembers_password = source.remember();

        account.summary.clone()
    };

    state::save_login(inner, &summary, &session, source.remember().then_some(password.as_str()))?;
    context.info("Apple ID session renewed");

    Ok(session)
}

async fn prompt_password(
    context: &JobContext,
    apple_id: &str,
    remember: bool,
) -> Result<(Zeroizing<String>, PasswordSource)> {
    let prompt = PromptKind::Password { apple_id: apple_id.into(), remember };

    match context.ask(prompt).await? {
        PromptReply::Text { value, remember } => Ok((Zeroizing::new(value), PasswordSource::Prompted { remember })),
        _ => Err(EngineError::Cancelled),
    }
}

/// A remembered password rejected as incorrect is forgotten, and the user is asked once.
async fn authenticate_with_fallback(
    inner: &Arc<Inner>,
    context: &JobContext,
    apple_id: &str,
    password: Zeroizing<String>,
    source: PasswordSource,
) -> Result<(AuthSession, Zeroizing<String>, PasswordSource)> {
    match authenticate(inner, context, apple_id, &password).await {
        Ok(session) => Ok((session, password, source)),

        Err(AuthFailure::Service(INCORRECT_CREDENTIALS)) if matches!(source, PasswordSource::Remembered) => {
            state::forget_password(inner, apple_id)?;
            context.warn("The remembered password was rejected; enter the current password.");

            let (password, source) = prompt_password(context, apple_id, true).await?;
            let session = authenticate(inner, context, apple_id, &password).await.map_err(EngineError::from)?;

            Ok((session, password, source))
        }

        Err(failure) => Err(failure.into()),
    }
}

enum AuthFailure {
    Service(i64),
    Other(EngineError),
}

impl From<AuthFailure> for EngineError {
    fn from(failure: AuthFailure) -> Self {
        match failure {
            AuthFailure::Service(code) => auth_error(sl_apple::Error::Service { operation: "complete", code }),
            AuthFailure::Other(error) => error,
        }
    }
}

async fn authenticate(
    inner: &Arc<Inner>,
    context: &JobContext,
    apple_id: &str,
    password: &Zeroizing<String>,
) -> std::result::Result<AuthSession, AuthFailure> {
    let primary = anisette::primary(inner, Some(context)).await.map_err(AuthFailure::Other)?;
    let alternate = anisette::alternate(inner).map_err(AuthFailure::Other)?;

    if let Ok(description) = anisette::describe(primary.as_ref(), apple_id).await {
        context.fact(Fact::AnisetteDevice(description));
    }

    let mut sources = AnisetteSources::new(primary);

    if let Some(provider) = alternate {
        sources = sources.with_alternate(provider);
    }

    let client = AuthClient::with_origin(&inner.auth_origin).map_err(|error| AuthFailure::Other(auth_error(error)))?;
    let delegate = JobFactorDelegate { context: context.clone(), apple_id: apple_id.into() };
    let cancellation = context.cancellation_token();

    let session = client.login(apple_id.into(), password.clone(), &sources, &delegate, &cancellation).await;

    match session {
        Ok(session) => {
            context.checkpoint().map_err(AuthFailure::Other)?;

            Ok(session)
        }

        Err(sl_apple::Error::Service { code, .. }) if code == INCORRECT_CREDENTIALS => Err(AuthFailure::Service(code)),
        Err(error) => Err(AuthFailure::Other(auth_error(error))),
    }
}

pub(super) fn team_summary(team: sl_apple::portal::TeamRecord) -> TeamSummary {
    let kind = match team.kind {
        PortalTeamKind::Free => TeamKind::Free,
        PortalTeamKind::Individual => TeamKind::Individual,
        PortalTeamKind::Organization => TeamKind::Organization,
        PortalTeamKind::Other(name) => TeamKind::Other(name),
    };

    TeamSummary { team_id: team.team_id, name: team.name, kind }
}

fn auth_error(error: sl_apple::Error) -> EngineError {
    match error {
        sl_apple::Error::Cancelled => EngineError::Cancelled,
        sl_apple::Error::Service { code: INCORRECT_CREDENTIALS, .. } => EngineError::Auth(format!(
            "Apple ID or password is incorrect ({INCORRECT_CREDENTIALS}); retrying can also succeed"
        )),
        other => EngineError::Auth(other.to_string()),
    }
}

struct JobFactorDelegate {
    context: JobContext,
    apple_id: String,
}

impl FactorDelegate for JobFactorDelegate {
    fn ask<'a>(&'a self, prompt: FactorPrompt) -> BoxFuture<'a, sl_apple::Result<FactorReply>> {
        async move {
            if prompt.incorrect_code {
                self.context.warn("Verification code incorrect; try entering it again.");
            }

            let kind = PromptKind::SecondFactor {
                apple_id: self.apple_id.clone(),
                destination: prompt.destination,
                code_length: prompt.code_length,
                can_request_sms: prompt.can_request_sms,
            };
            let reply = self.context.ask(kind).await.map_err(|_| sl_apple::Error::Cancelled)?;

            match reply {
                PromptReply::Text { value, .. } => Ok(FactorReply::Code(Zeroizing::new(value))),
                PromptReply::RequestSms => Ok(FactorReply::RequestSms),
                PromptReply::Cancel => Ok(FactorReply::Cancel),
                _ => Err(sl_apple::Error::Invalid("second-factor prompt reply")),
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobEvent;

    #[tokio::test]
    async fn factor_delegate_preserves_prompt_fields_and_redacts_code() {
        let (context, events, _) = JobContext::channel();
        let delegate = JobFactorDelegate { context, apple_id: "fixture@example.test".into() };
        let prompt = FactorPrompt {
            destination: "iPhone".into(),
            code_length: 6,
            can_request_sms: true,
            incorrect_code: false,
            attempt: 1,
        };
        let answer = async {
            let event = events.recv().await.expect("prompt event");
            let prompt = match event {
                JobEvent::Prompt(prompt) => prompt,
                other => panic!("expected factor prompt, got {other:?}"),
            };

            assert!(matches!(
                &prompt.kind,
                PromptKind::SecondFactor {
                    destination,
                    code_length: 6,
                    can_request_sms: true,
                    ..
                } if destination == "iPhone"
            ));

            prompt.answer(PromptReply::Text { value: "123456".into(), remember: false });
        };
        let (reply, ()) = tokio::join!(delegate.ask(prompt), answer);
        let reply = reply.expect("factor reply");

        assert!(matches!(&reply, FactorReply::Code(code) if code.as_str() == "123456"));
        assert!(!format!("{reply:?}").contains("123456"));
        let prompt_reply = PromptReply::Text { value: "123456".into(), remember: false };
        assert!(!format!("{prompt_reply:?}").contains("123456"));
    }
}
