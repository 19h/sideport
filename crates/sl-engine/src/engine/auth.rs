//! Non-demo GSA login and the engine prompt bridge.

use super::{Inner, LiveAccount};
use crate::error::{EngineError, Result};
use crate::job::{Fact, JobContext, PromptKind, PromptReply, Stage};
use crate::types::{AccountSummary, AnisetteSetting, TeamKind, TeamSummary};
use chrono::Utc;
use futures::FutureExt;
use futures::future::BoxFuture;
use sl_apple::anisette::RemoteAnisette;
use sl_apple::auth::{AnisetteProvider, AnisetteSources, AuthClient, FactorDelegate, FactorPrompt, FactorReply};
use sl_apple::portal::{PortalAccess, PortalClient, TeamKind as PortalTeamKind};
use std::sync::Arc;
use zeroize::Zeroizing;

pub(super) async fn login(
    inner: Arc<Inner>,
    context: JobContext,
    apple_id: String,
    password: Option<String>,
    remember: bool,
) -> Result<AccountSummary> {
    context.stage(Stage::Authenticating);

    let (password, remember) = match password {
        Some(value) => (Zeroizing::new(value), remember),
        None => {
            let prompt = PromptKind::Password { apple_id: apple_id.clone(), remember };

            match context.ask(prompt).await? {
                PromptReply::Text { value, remember } => (Zeroizing::new(value), remember),
                _ => return Err(EngineError::Cancelled),
            }
        }
    };

    if remember {
        return Err(EngineError::Unsupported("password storage is not implemented yet".into()));
    }

    context.checkpoint()?;

    let settings = inner.settings.read().clone();
    let primary = provider(&settings.anisette)?;
    let alternate = settings.alternate_anisette.as_ref().map(provider).transpose()?;
    let mut sources = AnisetteSources::new(primary.clone());
    if let Some(provider) = &alternate {
        sources = sources.with_alternate(provider.clone());
    }

    let client = AuthClient::with_origin(&inner.auth_origin).map_err(auth_error)?;
    let delegate = JobFactorDelegate { context: context.clone(), apple_id: apple_id.clone() };
    let session = client
        .login(apple_id.clone(), password, &sources, &delegate, &context.cancellation_token())
        .await
        .map_err(auth_error)?;

    context.checkpoint()?;

    let provider = if session.using_alternate() {
        alternate.as_deref().ok_or(EngineError::Auth("alternate anisette provider is missing".into()))?
    } else {
        primary.as_ref()
    };
    let portal = PortalClient::with_origin(&inner.portal_origin).map_err(auth_error)?;
    let cancellation = context.cancellation_token();
    let access = PortalAccess::new(&session, provider, &cancellation);
    let teams = match portal.list_teams(access).await {
        Ok(teams) => teams.into_iter().map(team_summary).collect::<Vec<_>>(),
        Err(sl_apple::Error::Cancelled) => return Err(EngineError::Cancelled),
        Err(error) => {
            context.warn(format!("Developer team listing unavailable: {error}"));

            Vec::new()
        }
    };
    context.checkpoint()?;

    let default_team = (teams.len() == 1).then(|| teams[0].team_id.clone());

    if let Some(team) = teams.first().filter(|_| teams.len() == 1) {
        context.fact(Fact::Team(team.clone()));
    }

    let summary = AccountSummary {
        apple_id: apple_id.clone(),
        teams,
        default_team,
        has_session: true,
        remembers_password: false,
        last_login: Some(Utc::now()),
    };
    inner.accounts.lock().insert(apple_id, LiveAccount { summary: summary.clone(), session: Arc::new(session) });
    context.info("Apple ID authentication completed");

    Ok(summary)
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

pub(super) fn provider(setting: &AnisetteSetting) -> Result<Arc<dyn AnisetteProvider>> {
    match setting {
        AnisetteSetting::Remote { url } => {
            let remote = RemoteAnisette::new(url).map_err(|error| EngineError::Anisette(error.to_string()))?;

            Ok(Arc::new(remote))
        }
        AnisetteSetting::Local => Err(EngineError::Unsupported("local anisette bridge is not implemented yet".into())),
    }
}

fn auth_error(error: sl_apple::Error) -> EngineError {
    match error {
        sl_apple::Error::Cancelled => EngineError::Cancelled,
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
