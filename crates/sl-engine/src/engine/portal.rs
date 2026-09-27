//! Session-backed developer-portal account operations.

use super::{Inner, auth, state};
use crate::engine::auth::team_summary;
use crate::error::{EngineError, Result};
use crate::job::{Fact, JobContext, PromptKind, PromptReply, Stage, TeamChoice};
use crate::types::{AppIdSummary, CertificateSummary, RegisteredDevice, TeamSummary};
use sl_apple::auth::{AnisetteProvider, AuthSession};
use sl_apple::portal::{Platform, PortalAccess, PortalClient};
use std::sync::Arc;

/// Recovered `DevApi.action` codes: 1100 "Session expired, login required" (re-login and retry),
/// 4550 "Please accept newer License Agreement".
const SESSION_EXPIRED: i64 = 1100;
const LICENSE_AGREEMENT: i64 = 4550;

/// Run one portal request through [`PortalJob::settle`], repeating it after a session renewal.
macro_rules! portal_call {
    ($job:expr, |$client:ident, $access:ident| $request:expr) => {{
        loop {
            $job.ensure_current()?;

            let cancellation = $job.context.cancellation_token();
            let outcome = {
                let $client = &$job.client;
                let $access = $job.access(&cancellation);

                $request.await
            };

            if let Some(value) = $job.settle(outcome).await? {
                break value;
            }
        }
    }};
}

pub(super) use portal_call;

pub(super) struct PortalJob {
    pub(super) inner: Arc<Inner>,
    pub(super) context: JobContext,
    pub(super) apple_id: String,
    session: Arc<AuthSession>,
    provider: Arc<dyn AnisetteProvider>,
    pub(super) client: PortalClient,
    teams: Vec<TeamSummary>,
    default_team: Option<String>,
    renewed: bool,
}

impl PortalJob {
    pub(super) async fn new(inner: Arc<Inner>, context: JobContext, apple_id: String) -> Result<Self> {
        context.stage(Stage::Authenticating);
        context.checkpoint()?;

        let (session, teams, default_team) = {
            let accounts = inner.accounts.lock();
            let account =
                accounts.get(&apple_id).ok_or_else(|| EngineError::Auth("account is not signed in".into()))?;

            (account.session.clone(), account.summary.teams.clone(), account.summary.default_team.clone())
        };

        let (session, renewed) = match session {
            Some(session) => (session, false),
            None => (auth::renew_session(&inner, &context, &apple_id).await?, true),
        };

        let provider = auth::session_provider(&inner, &session)?;
        let client = PortalClient::with_origin(&inner.portal_origin).map_err(portal_error)?;

        Ok(Self { inner, context, apple_id, session, provider, client, teams, default_team, renewed })
    }

    pub(super) fn ensure_current(&self) -> Result<()> {
        self.context.checkpoint()?;

        let accounts = self.inner.accounts.lock();
        let current = accounts
            .get(&self.apple_id)
            .and_then(|account| account.session.as_ref())
            .is_some_and(|session| Arc::ptr_eq(session, &self.session));

        if !current {
            return Err(EngineError::Auth("account session changed; sign in again".into()));
        }

        Ok(())
    }

    pub(super) fn access<'a>(&'a self, cancellation: &'a tokio_util::sync::CancellationToken) -> PortalAccess<'a> {
        PortalAccess::new(&self.session, self.provider.as_ref(), cancellation)
    }

    /// Decide whether a request outcome is final. An expired session is renewed once
    /// (remembered password or prompt), as the recovered client does for code 1100, and
    /// `None` asks the caller to repeat the request.
    pub(super) async fn settle<T>(&mut self, outcome: sl_apple::Result<T>) -> Result<Option<T>> {
        match outcome {
            Err(sl_apple::Error::Service { code: SESSION_EXPIRED, .. }) if !self.renewed => {
                self.context.warn("Apple developer session expired; signing in again.");

                self.session = auth::renew_session(&self.inner, &self.context, &self.apple_id).await?;
                self.provider = auth::session_provider(&self.inner, &self.session)?;
                self.renewed = true;

                Ok(None)
            }

            outcome => {
                self.ensure_current()?;

                outcome.map(Some).map_err(portal_error)
            }
        }
    }

    pub(super) async fn team(&mut self) -> Result<TeamSummary> {
        self.ensure_current()?;
        self.context.stage(Stage::Provisioning);

        if self.teams.is_empty() {
            let teams = portal_call!(self, |client, access| client.list_teams(access));
            self.teams = teams.into_iter().map(team_summary).collect();

            self.update_summary(|summary, teams| summary.teams = teams.to_vec())?;
        }

        let selected = match self.teams.as_slice() {
            [] => return Err(EngineError::Auth("the account has no available developer team".into())),
            [team] => team.clone(),
            teams => {
                if let Some(team) = teams.iter().find(|team| Some(&team.team_id) == self.default_team.as_ref()) {
                    team.clone()
                } else {
                    let choices = teams.iter().cloned().map(|team| TeamChoice { team }).collect();
                    let prompt = PromptKind::ChooseTeam { apple_id: self.apple_id.clone(), teams: choices };

                    match self.context.ask(prompt).await? {
                        PromptReply::Choice(index) => teams.get(index).cloned().ok_or(EngineError::Cancelled)?,
                        _ => return Err(EngineError::Cancelled),
                    }
                }
            }
        };

        self.ensure_current()?;
        self.context.fact(Fact::Team(selected.clone()));

        let team_id = selected.team_id.clone();
        self.default_team = Some(team_id.clone());
        self.update_summary(|summary, _| summary.default_team = Some(team_id.clone()))?;

        Ok(selected)
    }

    /// Apply a change to the live account summary for this session and persist it.
    fn update_summary(&self, change: impl FnOnce(&mut crate::types::AccountSummary, &[TeamSummary])) -> Result<()> {
        let summary = {
            let mut accounts = self.inner.accounts.lock();

            let Some(account) = accounts.get_mut(&self.apple_id) else {
                return Ok(());
            };

            if !account.session.as_ref().is_some_and(|session| Arc::ptr_eq(session, &self.session)) {
                return Ok(());
            }

            change(&mut account.summary, &self.teams);
            account.summary.clone()
        };

        state::save_summary(&self.inner, &summary)
    }
}

pub(super) async fn certificates(
    inner: Arc<Inner>,
    context: JobContext,
    apple_id: String,
) -> Result<Vec<CertificateSummary>> {
    let mut job = PortalJob::new(inner, context, apple_id).await?;
    let team_id = job.team().await?.team_id;

    let records = portal_call!(job, |client, access| client.list_certificates(&team_id, Platform::Ios, access));
    let key = state::signing_key(&job.inner, false)?;

    records
        .into_iter()
        .map(|record| {
            let name = match &record.content_der {
                Some(der) => sl_codesign::identity::certificate_common_name(der)
                    .map_err(|error| EngineError::Signing(error.to_string()))?,
                None => record.serial_number.clone(),
            };

            Ok(CertificateSummary {
                is_ours: state::owns_certificate(key.as_ref(), record.content_der.as_deref()),
                serial: record.serial_number,
                name,
                machine_name: record.machine_name,
                expires: record.expiration,
            })
        })
        .collect()
}

pub(super) async fn app_ids(inner: Arc<Inner>, context: JobContext, apple_id: String) -> Result<Vec<AppIdSummary>> {
    let mut job = PortalJob::new(inner, context, apple_id).await?;
    let team_id = job.team().await?.team_id;

    let records = portal_call!(job, |client, access| client.list_app_ids(&team_id, Platform::Ios, access));

    Ok(records
        .into_iter()
        .map(|record| AppIdSummary {
            app_id_id: record.app_id_id,
            identifier: record.identifier,
            name: record.name,
            expires: record.expiration,
        })
        .collect())
}

pub(super) async fn registered_devices(
    inner: Arc<Inner>,
    context: JobContext,
    apple_id: String,
) -> Result<Vec<RegisteredDevice>> {
    let mut job = PortalJob::new(inner, context, apple_id).await?;
    let team_id = job.team().await?.team_id;

    let records = portal_call!(job, |client, access| client.list_devices(&team_id, Platform::Ios, access));

    Ok(records.into_iter().map(|record| RegisteredDevice { udid: record.device_number, name: record.name }).collect())
}

pub(super) async fn revoke_certificate(
    inner: Arc<Inner>,
    context: JobContext,
    apple_id: String,
    serial: String,
) -> Result<()> {
    let mut job = PortalJob::new(inner, context, apple_id).await?;
    let team_id = job.team().await?.team_id;

    let records = portal_call!(job, |client, access| client.list_certificates(&team_id, Platform::Ios, access));

    if !records.iter().any(|record| record.serial_number == serial) {
        return Err(EngineError::Signing("certificate serial was not found in the selected team".into()));
    }

    portal_call!(job, |client, access| client.revoke_development_certificate(&team_id, Platform::Ios, &serial, access));

    let stored = job.inner.store.certificate(&team_id)?;

    if stored.is_some_and(|certificate| certificate.serial == serial) {
        job.inner.store.delete_certificate(&team_id)?;
    }

    Ok(())
}

pub(super) fn portal_error(error: sl_apple::Error) -> EngineError {
    match error {
        sl_apple::Error::Cancelled => EngineError::Cancelled,
        sl_apple::Error::Service { code: SESSION_EXPIRED, .. } => {
            EngineError::Auth("Apple developer session expired; sign in again".into())
        }
        sl_apple::Error::Service { operation, code: LICENSE_AGREEMENT } => EngineError::Portal {
            code: LICENSE_AGREEMENT,
            message: format!(
                "{operation} failed; accept the newer license agreement at https://developer.apple.com/account/"
            ),
        },
        sl_apple::Error::Service { operation, code } => {
            EngineError::Portal { code, message: format!("{operation} failed") }
        }
        other => EngineError::Other(format!("Developer portal request failed: {other}")),
    }
}

#[cfg(test)]
mod tests;
