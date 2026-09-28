//! Private services in Settings: what is configured, and an update check that only reads the
//! version manifest. Nothing is downloaded or installed, and with no services configured (the
//! default) nothing is contacted (docs/SERVICES.md).

use super::{
    Sideport,
    widgets::{card, danger_text, format_time, muted, section_title},
};
use gpui::{AnyElement, Context, IntoElement, Task, Window, div, prelude::*};
use gpui_component::{Disableable, button::Button};
use sl_engine::{FeatureState, UpdateStatus};

#[derive(Default)]
pub(crate) struct UpdateCheck {
    /// The last check's outcome, or why it failed.
    pub(crate) result: Option<Result<UpdateStatus, String>>,
    pub(crate) checking: bool,
    task: Option<Task<()>>,
}

/// The sentence shown for an update check's outcome.
pub(crate) fn update_message(status: &UpdateStatus) -> String {
    match status {
        UpdateStatus::NotConfigured => "Updates are not configured, so nothing was contacted.".into(),
        UpdateStatus::UpToDate { version } => format!("Sideport {version} is up to date."),
        UpdateStatus::Available { manifest } => {
            format!("Version {} is available. Sideport does not download or install it from here.", manifest.version)
        }
    }
}

fn configured(value: bool) -> &'static str {
    if value { "configured" } else { "not configured" }
}

/// The capabilities a verified feature token unlocks, by name.
fn unlocked(state: &FeatureState) -> Vec<String> {
    let features = &state.features;

    let flags = [
        (features.remote_anisette, "remote anisette"),
        (features.custom_entitlements, "custom entitlements"),
        (features.custom_icon, "custom icon"),
        (features.custom_info_props, "custom Info.plist properties"),
        (features.custom_upload_chunk, "custom upload chunk"),
    ];
    let refresh = features.refresh_interval_hours.map(|hours| format!("refresh after {hours} h"));

    let mut names: Vec<String> = flags.into_iter().filter(|(on, _)| *on).map(|(_, name)| name.to_owned()).collect();
    names.extend(refresh);

    names
}

impl Sideport {
    fn check_for_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.updates.checking {
            return;
        }

        let check = self.engine.check_update();
        self.updates.checking = true;
        self.updates.result = None;

        self.updates.task = Some(cx.spawn_in(window, async move |view, cx| {
            let result = check.await.map_err(|error| error.to_string());

            let _ = view.update_in(cx, |view, _, cx| {
                view.updates.checking = false;
                view.updates.result = Some(result);
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_services(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = self.engine.services_status();
        let tokens = &status.feature_state;

        let summary = format!(
            "Updates: {} · Feature tokens: {}",
            configured(status.updates_configured),
            configured(status.token_verifier_configured)
        );

        let token = tokens.token_present.then(|| {
            let features = unlocked(tokens);
            let features = if features.is_empty() { "no features".to_string() } else { features.join(", ") };
            let expires = tokens.expires.map(|time| format!(" until {}", format_time(time))).unwrap_or_default();

            muted(format!("A feature token unlocks {features}{expires}."), cx)
        });

        let nothing_configured = !status.updates_configured && !status.token_verifier_configured;
        let unconfigured_note = "This build has no update or feature-token service; nothing is contacted until one \
                                 is configured.";
        let unconfigured = nothing_configured.then(|| muted(unconfigured_note, cx));

        let check = Button::new("check-update")
            .outline()
            .label("Check for updates")
            .loading(self.updates.checking)
            .disabled(self.updates.checking)
            .debug_selector(|| "check-update".into())
            .on_click(cx.listener(|view, _, window, cx| view.check_for_updates(window, cx)));

        let result = match &self.updates.result {
            Some(Ok(outcome)) => Some(muted(update_message(outcome), cx)),
            Some(Err(error)) => Some(danger_text(format!("Update check failed: {error}"), cx)),
            None => None,
        };
        let result = result.map(|result| result.debug_selector(|| "update-result".into()));

        let scope = "Checking reads the version manifest only; updates are never downloaded or installed here.";

        card(cx)
            .child(section_title("Updates and services"))
            .child(div().debug_selector(|| "services-status".into()).text_sm().child(summary))
            .children(unconfigured)
            .children(token)
            .child(div().flex().items_center().gap_3().child(check))
            .children(result)
            .child(muted(scope, cx))
            .into_any_element()
    }
}
