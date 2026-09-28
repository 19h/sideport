//! Tracked installations: expiry, automatic refresh, manual refresh and forgetting.

use super::{
    Confirmation, PendingAction, Section, Sideport,
    widgets::{card, danger_text, icon_placeholder, muted, page_title},
};
use chrono::{DateTime, Utc};
use gpui::{AnyElement, Context, FontWeight, Image, ImageFormat, IntoElement, Task, Window, div, img, prelude::*, px};
use gpui_component::{
    ActiveTheme, Disableable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
};
use sl_engine::{Installation, RefreshEvent};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Default)]
pub(crate) struct Installations {
    pub(crate) list: Vec<Installation>,
    /// Decoded icons by installation; rebuilt only when the list is reloaded.
    icons: BTreeMap<i64, Arc<Image>>,
    /// The last background refresh notification.
    pub(crate) notice: Option<String>,
    watch: Option<Task<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Urgency {
    Normal,
    Soon,
    Expired,
}

/// Remaining signing time. Days follow `Installation::days_left` (whole days, negative after
/// expiry); the final day is shown in hours.
pub(crate) fn expiry_label(expires: Option<DateTime<Utc>>, now: DateTime<Utc>) -> (String, Urgency) {
    let Some(expires) = expires else {
        return ("Expiry unknown".into(), Urgency::Normal);
    };

    let remaining = expires - now;
    let days = remaining.num_days();

    if remaining <= chrono::Duration::zero() {
        let label = match -days {
            0 => "Expired today".to_string(),
            1 => "Expired 1 day ago".to_string(),
            elapsed => format!("Expired {elapsed} days ago"),
        };

        return (label, Urgency::Expired);
    }

    match days {
        0 => (format!("Expires in {} h", remaining.num_hours().max(1)), Urgency::Soon),
        1 => ("1 day left".into(), Urgency::Soon),
        2 => ("2 days left".into(), Urgency::Soon),
        days => (format!("{days} days left"), Urgency::Normal),
    }
}

impl Sideport {
    pub(super) fn reload_installations(&mut self) {
        let list = match self.engine.installations() {
            Ok(list) => list,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        };

        let icons = list
            .iter()
            .filter_map(|installation| {
                let png = installation.icon_png.clone()?;

                Some((installation.id, Arc::new(Image::from_bytes(ImageFormat::Png, png))))
            })
            .collect();

        self.installations.list = list;
        self.installations.icons = icons;
    }

    /// Follow the engine's refresh notifications (scheduler and manual refreshes).
    pub(super) fn watch_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let events = self.engine.subscribe_refresh();

        self.installations.watch = Some(cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = events.recv().await {
                if view.update_in(cx, |view, _, cx| view.refresh_event(event, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    pub(super) fn refresh_event(&mut self, event: RefreshEvent, cx: &mut Context<Self>) {
        let notice = match event {
            RefreshEvent::Started { app_name, .. } => format!("Refreshing {app_name}…"),
            RefreshEvent::Succeeded { app_name, .. } => format!("Refreshed {app_name}"),
            RefreshEvent::Failed { app_name, error, .. } => format!("Refreshing {app_name} failed: {error}"),
        };

        self.installations.notice = Some(notice);
        self.reload_installations();
        cx.notify();
    }

    fn set_auto_refresh(&mut self, id: i64, enabled: bool, cx: &mut Context<Self>) {
        if let Err(error) = self.engine.set_auto_refresh(id, enabled) {
            self.error = Some(error.to_string());
        }

        self.reload_installations();
        cx.notify();
    }

    fn refresh_installation(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }
        let Some(installation) = self.installations.list.iter().find(|installation| installation.id == id) else {
            return;
        };

        let name = installation.app_name.clone();
        self.job_account = Some(installation.apple_id.clone());
        self.outcome = None;

        let job = self.engine.refresh(id);
        self.run_job(job, &format!("Refreshing {name}…"), window, cx, move |view, outcome, _, _| {
            view.status = format!("Refreshed {name}");
            view.outcome = Some(outcome);
        });
    }

    fn confirm_forget(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(installation) = self.installations.list.iter().find(|installation| installation.id == id) else {
            return;
        };

        let message = format!(
            "Sideport stops refreshing {} on {} and deletes its stored copy. The app stays on the device.",
            installation.app_name, installation.device_name
        );
        let confirmation = Confirmation {
            title: format!("Forget {}?", installation.app_name),
            message,
            confirm_label: "Forget",
            action: PendingAction::ForgetInstallation { id },
        };

        self.confirm(confirmation, window, cx);
    }

    pub(super) fn forget_installation(&mut self, id: i64, cx: &mut Context<Self>) {
        match self.engine.forget_installation(id) {
            Ok(()) => self.status = "Installation forgotten".into(),
            Err(error) => self.error = Some(error.to_string()),
        }

        self.reload_installations();
        cx.notify();
    }

    pub(super) fn render_installations(&self, cx: &mut Context<Self>) -> AnyElement {
        let locked = self.occupied();
        let now = Utc::now();
        let refresh = self.engine.settings().refresh;

        let policy = if refresh.enabled {
            let network = if refresh.allow_network { "USB or Wi-Fi" } else { "USB" };

            format!(
                "Automatic refresh is on: apps are re-signed when fewer than {} h remain, checked every {} min over {network}.",
                refresh.threshold_hours, refresh.check_interval_minutes
            )
        } else {
            "Automatic refresh is off in Settings.".to_string()
        };

        let rows: Vec<_> = self
            .installations
            .list
            .iter()
            .map(|installation| self.render_installation(installation, now, locked, cx))
            .collect();

        div()
            .max_w(px(1100.))
            .mx_auto()
            .flex()
            .flex_col()
            .gap_5()
            .child(page_title("Installations"))
            .child(
                div().flex().items_center().gap_3().child(div().flex_1().child(muted(policy, cx))).child(
                    Button::new("refresh-settings")
                        .ghost()
                        .label("Settings…")
                        .on_click(cx.listener(|view, _, window, cx| view.show_section(Section::Settings, window, cx))),
                ),
            )
            .when_some(self.installations.notice.clone(), |this, notice| {
                this.child(div().debug_selector(|| "refresh-notice".into()).text_sm().child(notice))
            })
            .when(self.installations.list.is_empty(), |this| {
                this.child(card(cx).child(muted(
                    "No tracked installations. Install with an Apple ID and keep \"Track for automatic refresh\" \
                     selected to renew apps before they expire.",
                    cx,
                )))
            })
            .children(rows)
            .into_any_element()
    }

    fn render_installation(
        &self,
        installation: &Installation,
        now: DateTime<Utc>,
        locked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = installation.id;
        let element = id as usize;
        let (expiry, urgency) = expiry_label(installation.expires_at, now);
        let expiry_color = match urgency {
            Urgency::Normal => cx.theme().muted_foreground,
            Urgency::Soon => cx.theme().warning,
            Urgency::Expired => cx.theme().danger,
        };

        let icon = match self.installations.icons.get(&id) {
            Some(icon) => img(icon.clone()).size(px(48.)).rounded_xl().into_any_element(),
            None => icon_placeholder(&installation.app_name, 48., cx).into_any_element(),
        };

        let version = installation.version.as_deref().map(|version| format!(" {version}")).unwrap_or_default();
        let origin =
            format!("{} · {} · team {}", installation.device_name, installation.apple_id, installation.team_id);
        let failures = match installation.consecutive_failures {
            0 => None,
            1 => Some("1 failed refresh".to_string()),
            count => Some(format!("{count} failed refreshes in a row")),
        };
        let last_error = installation.last_error.as_ref().map(|error| format!("Last error: {error}"));

        let title = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("{}{version}", installation.app_name)))
            .child(
                div().debug_selector(move || format!("expiry:{id}")).text_sm().text_color(expiry_color).child(expiry),
            );

        let details = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(title)
            .child(muted(installation.bundle_id.clone(), cx))
            .child(muted(origin, cx))
            .when_some(failures, |this, failures| this.child(danger_text(failures, cx)))
            .when_some(last_error, |this, error| this.child(danger_text(error, cx)));

        let controls = div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                Checkbox::new(("auto-refresh", element))
                    .label("Refresh automatically")
                    .checked(installation.auto_refresh)
                    .disabled(locked)
                    .debug_selector(move || format!("auto-refresh:{id}"))
                    .on_click(cx.listener(move |view, checked, _, cx| view.set_auto_refresh(id, *checked, cx))),
            )
            .child(div().flex_1())
            .child(
                Button::new(("refresh-now", element))
                    .outline()
                    .label("Refresh now")
                    .disabled(locked)
                    .debug_selector(move || format!("refresh:{id}"))
                    .on_click(cx.listener(move |view, _, window, cx| view.refresh_installation(id, window, cx))),
            )
            .child(
                Button::new(("forget", element))
                    .ghost()
                    .label("Forget…")
                    .disabled(locked)
                    .debug_selector(move || format!("forget:{id}"))
                    .on_click(cx.listener(move |view, _, window, cx| view.confirm_forget(id, window, cx))),
            );

        card(cx)
            .debug_selector(move || format!("installation:{id}"))
            .child(div().flex().gap_4().child(icon).child(details))
            .child(controls)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{Urgency, expiry_label};
    use chrono::{Duration, TimeZone, Utc};

    #[test]
    fn expiry_labels_count_whole_days_and_mark_expired_installations() {
        let now = Utc.with_ymd_and_hms(2026, 9, 28, 12, 0, 0).single().expect("time");

        let cases = [
            (None, "Expiry unknown", Urgency::Normal),
            (Some(now + Duration::days(6)), "6 days left", Urgency::Normal),
            (Some(now + Duration::days(2) - Duration::hours(3)), "1 day left", Urgency::Soon),
            (Some(now + Duration::hours(5)), "Expires in 5 h", Urgency::Soon),
            (Some(now + Duration::minutes(20)), "Expires in 1 h", Urgency::Soon),
            (Some(now), "Expired today", Urgency::Expired),
            (Some(now - Duration::hours(30)), "Expired 1 day ago", Urgency::Expired),
            (Some(now - Duration::days(3)), "Expired 3 days ago", Urgency::Expired),
        ];

        for (expires, label, urgency) in cases {
            assert_eq!(expiry_label(expires, now), (label.to_string(), urgency), "{expires:?}");
        }
    }
}
