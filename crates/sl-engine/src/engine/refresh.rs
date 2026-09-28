//! Installation refresh and the background scheduler.
//!
//! Recovered daemon: `[N days left] = KnownTTL − (now − LastUpdated)`, refresh when fewer than
//! `refresh_at_hours` remain, `[FAIL]` after three failures, and a queue ordered by
//! `enqueued_at` that the GUI drains without interaction. Sideport keeps the queue in the state
//! database; a process claims an entry before running it.

use super::{Inner, RefreshEvent, devices, sideload};
use crate::error::{EngineError, Result};
use crate::job::{JobContext, JobEvent, PromptReply};
use crate::types::{Connection, Installation, JobOutcome};
use chrono::{Duration, Utc};
use std::sync::{Arc, Weak};

/// A claim older than this is considered abandoned by a crashed process.
const CLAIM_TIMEOUT: Duration = Duration::hours(1);

/// Re-run an installation's stored job and record the outcome.
pub(super) async fn refresh(inner: Arc<Inner>, context: JobContext, id: i64) -> Result<JobOutcome> {
    let installation = inner
        .store
        .installation(id)?
        .ok_or_else(|| EngineError::Storage(format!("installation {id} does not exist")))?;

    inner.publish_refresh(&RefreshEvent::Started { installation_id: id, app_name: installation.app_name.clone() });

    let mut spec = installation.spec.clone();
    spec.options.track_for_refresh = true;

    let result = sideload::run(inner.clone(), context, spec).await;

    match &result {
        Ok(outcome) => {
            let event = RefreshEvent::Succeeded {
                installation_id: outcome.installation_id.unwrap_or(id),
                app_name: installation.app_name.clone(),
                expires: outcome.expires,
            };

            inner.publish_refresh(&event);
        }

        Err(error) => {
            record_failure(&inner, installation.clone(), error)?;

            let event =
                RefreshEvent::Failed { installation_id: id, app_name: installation.app_name, error: error.to_string() };
            inner.publish_refresh(&event);
        }
    }

    result
}

fn record_failure(inner: &Inner, mut installation: Installation, error: &EngineError) -> Result<()> {
    if matches!(error, EngineError::Cancelled) {
        return Ok(());
    }

    installation.last_error = Some(error.to_string());
    installation.consecutive_failures = installation.consecutive_failures.saturating_add(1);

    inner.store.update_installation(&installation)
}

/// Installations to refresh now: automatic refresh enabled and expiring within the threshold.
pub(super) fn due(installations: &[Installation], threshold_hours: u32, now: chrono::DateTime<Utc>) -> Vec<i64> {
    let horizon = now + Duration::hours(i64::from(threshold_hours));

    installations
        .iter()
        .filter(|installation| installation.auto_refresh)
        .filter(|installation| installation.expires_at.is_some_and(|expires| expires <= horizon))
        .map(|installation| installation.id)
        .collect()
}

/// One scheduler pass: queue due installations whose device is reachable, then run the queue.
pub(super) async fn tick(inner: &Arc<Inner>) -> Result<usize> {
    let settings = inner.settings().refresh;

    if !settings.enabled {
        return Ok(0);
    }

    let now = Utc::now();
    let installations = inner.store.installations()?;
    let devices = devices::list(inner).await.unwrap_or_default();

    for id in due(&installations, settings.threshold_hours, now) {
        let Some(installation) = installations.iter().find(|installation| installation.id == id) else {
            continue;
        };

        let reachable = devices.iter().any(|device| {
            device.udid == installation.device_udid
                && device.paired
                && (device.connections.contains(&Connection::Usb)
                    || settings.allow_network && device.connections.contains(&Connection::Network))
        });

        if reachable {
            inner.store.enqueue_refresh(id, &uuid::Uuid::new_v4().to_string(), now)?;
        }
    }

    run_queue(inner).await
}

/// Claim and run queued refreshes one at a time without interaction.
pub(super) async fn run_queue(inner: &Arc<Inner>) -> Result<usize> {
    let claim = format!("claim:{}", uuid::Uuid::new_v4());
    let mut completed = 0;

    loop {
        let now = Utc::now();
        let Some(entry) = inner.store.claim_refresh(&claim, now, now - CLAIM_TIMEOUT)? else {
            return Ok(completed);
        };

        let outcome = unattended(inner.clone(), entry.installation_id).await;
        inner.store.dequeue_refresh(&entry)?;

        if outcome.is_ok() {
            completed += 1;
        }
    }
}

/// Run a refresh with no front end: prompts are declined, so steps needing a person fail.
pub(super) async fn unattended(inner: Arc<Inner>, id: i64) -> Result<JobOutcome> {
    let (context, events, _cancel) = JobContext::channel();

    let drain = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            if let JobEvent::Prompt(prompt) = event {
                prompt.answer(PromptReply::Cancel);
            }
        }
    });

    let result = refresh(inner, context, id).await;
    let _ = drain.await;

    result
}

/// Start the periodic scheduler; it stops when the last engine handle is dropped.
pub(super) fn start(inner: &Arc<Inner>) {
    let weak: Weak<Inner> = Arc::downgrade(inner);
    let handle = inner.runtime.handle.clone();

    handle.spawn(async move {
        loop {
            let interval = {
                let Some(inner) = weak.upgrade() else {
                    return;
                };

                if inner.subscribers.upgrade().is_none() {
                    return;
                }

                let _ = tick(&inner).await;
                let minutes = inner.settings().refresh.check_interval_minutes.max(1);

                std::time::Duration::from_secs(u64::from(minutes) * 60)
            };

            tokio::time::sleep(interval).await;
        }
    });
}
