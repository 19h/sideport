//! The menu-bar refresh daemon's menu, recovered from `sideloadly-daemon` (`main.TickWithForce`,
//! `main.fillJitMenu`).
//!
//! The recovered daemon is a Qt tray app. Each tracked installation is one submenu titled with
//! its remaining time: `KnownTTL` days (7 when unknown) minus the time since `LastUpdated`, shown
//! as a seven-cell bar and a day count (` [#####__] 5 days left`), with `, WARNING!` below three
//! days and ` [FAIL]` after more than two consecutive failures. The submenu offers "Refresh Now",
//! "Enable JIT" and "Forget [!]"; below the installations come "Enable JIT for Apps", "Refresh All
//! Manually", "Reset Database (!)" and "Automatically Launch on System Boot".
//!
//! This module builds that menu as data, so the labels and actions are testable without a menu
//! bar; `main.rs` renders it.

use chrono::{DateTime, Utc};
use sl_engine::Installation;

/// Cells in the remaining-time bar.
const BAR_CELLS: i64 = 7;

/// The recovered default when an installation's validity is unknown.
const DEFAULT_TTL_DAYS: i64 = 7;

/// What a menu item does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Refresh(i64),
    EnableJit(i64),
    Forget(i64),
    RefreshAll,
    ResetDatabase,
    OpenApp,
    ToggleAutostart,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayItem {
    /// A disabled line of text.
    Label(String),
    Action {
        title: String,
        action: TrayAction,
    },
    Check {
        title: String,
        checked: bool,
        action: TrayAction,
    },
    Submenu {
        title: String,
        items: Vec<TrayItem>,
    },
    Separator,
}

/// Days an installation remains valid: its profile lifetime (`KnownTTL`) minus the time since it
/// was installed, truncated toward zero as Go's `int()` does, and never below zero.
pub fn days_left(installation: &Installation, now: DateTime<Utc>) -> i64 {
    let ttl_days = known_ttl_days(installation);
    let elapsed = now - installation.installed_at;
    let hours_left = (ttl_days * 24 * 3600 - elapsed.num_seconds()) as f64 / 3600.0;

    ((hours_left / 24.0) as i64).max(0)
}

fn known_ttl_days(installation: &Installation) -> i64 {
    let ttl_days = installation.expires_at.map(|expires| (expires - installation.installed_at).num_days());

    ttl_days.filter(|days| *days > 0).unwrap_or(DEFAULT_TTL_DAYS)
}

/// The recovered remaining-time text: ` [#####__] 5 days left`, then `, WARNING!` below three
/// days and ` [FAIL]` after more than two consecutive failures.
pub fn remaining(installation: &Installation, now: DateTime<Utc>) -> String {
    let days = days_left(installation, now);
    let ttl_days = known_ttl_days(installation);

    let filled = BAR_CELLS * days / ttl_days;
    let bar: String = (0..BAR_CELLS).map(|cell| if filled > cell { '#' } else { '_' }).collect();
    let plural = if days == 1 { "" } else { "s" };

    let mut text = format!(" [{bar}] {days} day{plural} left");

    if days < 3 {
        text.push_str(", WARNING!");
    }

    if installation.consecutive_failures > 2 {
        text.push_str(" [FAIL]");
    }

    text
}

/// The whole menu for `installations` at `now`.
pub fn menu(
    installations: &[Installation],
    now: DateTime<Utc>,
    autostart: Option<bool>,
    status: Option<&str>,
) -> Vec<TrayItem> {
    let version = env!("CARGO_PKG_VERSION");
    let mut items = vec![TrayItem::Label(format!("Sideport refresh v{version} is running"))];

    if let Some(status) = status {
        items.push(TrayItem::Label(status.to_owned()));
    }

    items.push(TrayItem::Separator);

    if installations.is_empty() {
        items.push(TrayItem::Label("No tracked installations".into()));
    }

    for installation in installations {
        let id = installation.id;
        let title =
            format!("{} on {}{}", installation.app_name, installation.device_name, remaining(installation, now));

        let actions = vec![
            TrayItem::Action { title: "Refresh Now".into(), action: TrayAction::Refresh(id) },
            TrayItem::Action { title: "Enable JIT".into(), action: TrayAction::EnableJit(id) },
            TrayItem::Action { title: "Forget [!]".into(), action: TrayAction::Forget(id) },
        ];

        items.push(TrayItem::Submenu { title, items: actions });
    }

    let jit: Vec<TrayItem> = installations
        .iter()
        .map(|installation| TrayItem::Action {
            title: format!("{} on {}", installation.app_name, installation.device_name),
            action: TrayAction::EnableJit(installation.id),
        })
        .collect();

    items.push(TrayItem::Separator);

    if !jit.is_empty() {
        items.push(TrayItem::Submenu { title: "Enable JIT for Apps".into(), items: jit });
    }

    items.push(TrayItem::Action { title: "Refresh All Manually".into(), action: TrayAction::RefreshAll });
    items.push(TrayItem::Action { title: "Open Sideport".into(), action: TrayAction::OpenApp });
    items.push(TrayItem::Action { title: "Reset Database (!)".into(), action: TrayAction::ResetDatabase });

    if let Some(checked) = autostart {
        let title = "Automatically Launch on System Boot".into();
        items.push(TrayItem::Check { title, checked, action: TrayAction::ToggleAutostart });
    }

    items.push(TrayItem::Separator);
    items.push(TrayItem::Action { title: "Quit".into(), action: TrayAction::Quit });

    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use sl_engine::{AppOptions, JobSpec, SigningMode, Target};

    fn installation(installed: &str, expires: Option<&str>, failures: u32) -> Installation {
        Installation {
            id: 7,
            app_name: "App".into(),
            bundle_id: "com.example.app".into(),
            original_bundle_id: "com.example.app".into(),
            version: None,
            device_udid: "UDID".into(),
            device_name: "Phone".into(),
            apple_id: "fixture@example.test".into(),
            team_id: "TEAM123456".into(),
            installed_at: installed.parse().expect("date"),
            expires_at: expires.map(|expires| expires.parse().expect("date")),
            auto_refresh: true,
            last_error: None,
            consecutive_failures: failures,
            icon_png: None,
            spec: JobSpec {
                source: "/a.ipa".into(),
                target: Target::Device { udid: "UDID".into(), prefer_network: false },
                signing: SigningMode::AdHoc,
                options: AppOptions::default(),
            },
        }
    }

    fn at(time: &str) -> DateTime<Utc> {
        time.parse().expect("date")
    }

    #[test]
    fn remaining_time_uses_the_recovered_bar_plural_warning_and_failure_text() {
        let seven_days = installation("2026-09-01T00:00:00Z", Some("2026-09-08T00:00:00Z"), 0);

        assert_eq!(remaining(&seven_days, at("2026-09-01T00:00:00Z")), " [#######] 7 days left");
        assert_eq!(remaining(&seven_days, at("2026-09-03T12:00:00Z")), " [####___] 4 days left");
        assert_eq!(remaining(&seven_days, at("2026-09-06T01:00:00Z")), " [#______] 1 day left, WARNING!");
        assert_eq!(remaining(&seven_days, at("2026-09-07T23:00:00Z")), " [_______] 0 days left, WARNING!");
        assert_eq!(remaining(&seven_days, at("2026-09-20T00:00:00Z")), " [_______] 0 days left, WARNING!", "clamped");

        let failing = installation("2026-09-01T00:00:00Z", Some("2026-09-08T00:00:00Z"), 3);
        assert_eq!(remaining(&failing, at("2026-09-06T01:00:00Z")), " [#______] 1 day left, WARNING! [FAIL]");
        assert!(
            !remaining(&installation("2026-09-01T00:00:00Z", None, 2), at("2026-09-02T00:00:00Z")).contains("FAIL")
        );

        let year = installation("2026-09-01T00:00:00Z", Some("2027-09-01T00:00:00Z"), 0);
        assert_eq!(remaining(&year, at("2027-03-02T00:00:00Z")), " [###____] 183 days left", "paid-team profile");

        let unknown = installation("2026-09-01T00:00:00Z", None, 0);
        assert_eq!(days_left(&unknown, at("2026-09-02T00:00:00Z")), 6, "the recovered seven-day default");
    }

    #[test]
    fn the_menu_lists_each_installation_with_its_actions_and_the_global_items() {
        let installations = [installation("2026-09-01T00:00:00Z", Some("2026-09-08T00:00:00Z"), 0)];
        let items = menu(&installations, at("2026-09-02T00:00:00Z"), Some(true), Some("Refreshed App"));

        assert_eq!(items[0], TrayItem::Label(format!("Sideport refresh v{} is running", env!("CARGO_PKG_VERSION"))));
        assert_eq!(items[1], TrayItem::Label("Refreshed App".into()));

        let TrayItem::Submenu { title, items: actions } = &items[3] else { panic!("installation submenu") };
        assert_eq!(title, "App on Phone [######_] 6 days left");

        let actions: Vec<_> = actions
            .iter()
            .filter_map(|item| match item {
                TrayItem::Action { action, .. } => Some(*action),
                _ => None,
            })
            .collect();
        assert_eq!(actions, [TrayAction::Refresh(7), TrayAction::EnableJit(7), TrayAction::Forget(7)]);

        assert!(items.contains(&TrayItem::Check {
            title: "Automatically Launch on System Boot".into(),
            checked: true,
            action: TrayAction::ToggleAutostart
        }));
        assert_eq!(items.last(), Some(&TrayItem::Action { title: "Quit".into(), action: TrayAction::Quit }));

        let empty = menu(&[], at("2026-09-02T00:00:00Z"), None, None);
        assert!(empty.contains(&TrayItem::Label("No tracked installations".into())));
        assert!(!empty.iter().any(|item| matches!(item, TrayItem::Check { .. } | TrayItem::Submenu { .. })));
    }
}
