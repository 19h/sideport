//! Appearance, Apple sign-in provider, automatic refresh and job defaults. The private-services
//! card follows the saved form (`services.rs`).

use super::{
    Sideport, apply_theme,
    widgets::{card, danger_text, field, muted, page_title, section_title},
};
use gpui::{AnyElement, App, AppContext, Context, Entity, IntoElement, Task, Window, div, prelude::*, px};
use gpui_component::{
    Disableable, Selectable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::InputState,
};
use sl_engine::{AnisetteSetting, Settings, ThemePreference};

const MAX_URL_BYTES: usize = 2048;
const THRESHOLD_HOURS: std::ops::RangeInclusive<u32> = 1..=720;
const INTERVAL_MINUTES: std::ops::RangeInclusive<u32> = 1..=1440;

/// Unsaved settings; toggles apply to this form until "Save settings".
pub(crate) struct SettingsForm {
    pub(crate) remote: bool,
    pub(crate) remote_url: Entity<InputState>,
    pub(crate) alternate_url: Entity<InputState>,
    pub(crate) refresh_enabled: bool,
    pub(crate) refresh_network: bool,
    pub(crate) threshold: Entity<InputState>,
    pub(crate) interval: Entity<InputState>,
    pub(crate) remember_passwords: bool,
    pub(crate) stream_upload: bool,
    pub(crate) notice: Option<String>,
    pub(crate) probe: Option<Result<String, String>>,
    pub(crate) probing: bool,
    probe_task: Option<Task<()>>,
}

impl SettingsForm {
    pub(crate) fn new(settings: &Settings, window: &mut Window, cx: &mut App) -> Self {
        let mut input = |placeholder: &'static str| cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let remote_url = input("https://anisette.example.com");
        let alternate_url = input("Optional second provider");
        let threshold = input("48");
        let interval = input("30");

        let mut form = Self {
            remote: false,
            remote_url,
            alternate_url,
            refresh_enabled: true,
            refresh_network: true,
            threshold,
            interval,
            remember_passwords: false,
            stream_upload: false,
            notice: None,
            probe: None,
            probing: false,
            probe_task: None,
        };

        form.load(settings, window, cx);

        form
    }

    /// Replace the form with the saved settings.
    pub(crate) fn load(&mut self, settings: &Settings, window: &mut Window, cx: &mut App) {
        let remote_url = match &settings.anisette {
            AnisetteSetting::Remote { url } => url.clone(),
            AnisetteSetting::Local => String::new(),
        };
        let alternate_url = match &settings.alternate_anisette {
            Some(AnisetteSetting::Remote { url }) => url.clone(),
            _ => String::new(),
        };

        self.remote = matches!(settings.anisette, AnisetteSetting::Remote { .. });
        self.refresh_enabled = settings.refresh.enabled;
        self.refresh_network = settings.refresh.allow_network;
        self.remember_passwords = settings.remember_passwords;
        self.stream_upload = settings.stream_upload;
        self.notice = None;
        self.probe = None;

        for (input, value) in [
            (&self.remote_url, remote_url),
            (&self.alternate_url, alternate_url),
            (&self.threshold, settings.refresh.threshold_hours.to_string()),
            (&self.interval, settings.refresh.check_interval_minutes.to_string()),
        ] {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
    }

    pub(crate) fn anisette(&self, cx: &App) -> Result<AnisetteSetting, String> {
        if !self.remote {
            return Ok(AnisetteSetting::Local);
        }

        let url = web_url(&self.remote_url.read(cx).value(), "Anisette provider")?;

        Ok(AnisetteSetting::Remote { url })
    }

    fn alternate(&self, cx: &App) -> Result<Option<AnisetteSetting>, String> {
        let text = self.alternate_url.read(cx).value().trim().to_string();

        if text.is_empty() {
            return Ok(None);
        }

        let url = web_url(&text, "Alternate provider")?;

        Ok(Some(AnisetteSetting::Remote { url }))
    }

    /// Copy the form onto `settings`, leaving the appearance untouched.
    pub(crate) fn apply(&self, settings: &mut Settings, cx: &App) -> Result<(), String> {
        let threshold = whole_number(&self.threshold.read(cx).value(), THRESHOLD_HOURS, "Refresh threshold (hours)")?;
        let interval = whole_number(&self.interval.read(cx).value(), INTERVAL_MINUTES, "Check interval (minutes)")?;
        let anisette = self.anisette(cx)?;
        let alternate = self.alternate(cx)?;

        settings.anisette = anisette;
        settings.alternate_anisette = alternate;
        settings.refresh.enabled = self.refresh_enabled;
        settings.refresh.threshold_hours = threshold;
        settings.refresh.check_interval_minutes = interval;
        settings.refresh.allow_network = self.refresh_network;
        settings.remember_passwords = self.remember_passwords;
        settings.stream_upload = self.stream_upload;

        Ok(())
    }
}

fn web_url(text: &str, label: &str) -> Result<String, String> {
    let url = text.trim();
    let scheme = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"));

    match scheme {
        Some(rest) if !rest.is_empty() && url.len() <= MAX_URL_BYTES && !url.contains(char::is_whitespace) => {
            Ok(url.to_string())
        }
        _ => Err(format!("{label}: enter an http:// or https:// URL.")),
    }
}

fn whole_number(text: &str, range: std::ops::RangeInclusive<u32>, label: &str) -> Result<u32, String> {
    match text.trim().parse::<u32>() {
        Ok(value) if range.contains(&value) => Ok(value),
        _ => Err(format!("{label}: enter a whole number from {} to {}.", range.start(), range.end())),
    }
}

impl Sideport {
    pub(super) fn theme(&mut self, preference: ThemePreference, window: &mut Window, cx: &mut Context<Self>) {
        let saved = self.engine.modify_settings(|settings| {
            settings.theme = preference;

            Ok(())
        });

        match saved {
            Ok(_) => apply_theme(preference, window, cx),
            Err(error) => self.error = Some(error.to_string()),
        }

        cx.notify();
    }

    /// Install or remove the login item that runs the refresh scheduler.
    pub(super) fn set_autostart(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let Some(program) = self.daemon_program.clone() else {
            return;
        };

        match self.engine.set_autostart(enabled, &program) {
            Ok(()) => {
                let state = if enabled { "starts" } else { "no longer starts" };
                self.settings.notice = Some(format!("Refresh {state} at login."));
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }

        cx.notify();
    }

    pub(super) fn save_settings(&mut self, cx: &mut Context<Self>) {
        // The form applies to the stored settings as they are now; an invalid form saves nothing.
        let form = &self.settings;
        let saved =
            self.engine.modify_settings(|settings| form.apply(settings, cx).map_err(sl_engine::EngineError::Other));

        match saved {
            Ok(settings) => {
                self.settings.notice = Some("Settings saved.".into());
                self.accounts.remember = settings.remember_passwords;
                self.error = None;
            }
            Err(error) => {
                self.settings.notice = None;
                self.error = Some(error.to_string());
            }
        }

        cx.notify();
    }

    fn test_anisette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let setting = match self.settings.anisette(cx) {
            Ok(setting) => setting,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };

        let probe = self.engine.test_anisette(setting);
        self.settings.probe = None;
        self.settings.probing = true;

        self.settings.probe_task = Some(cx.spawn_in(window, async move |view, cx| {
            let result = probe.await.map_err(|error| error.to_string());

            let _ = view.update_in(cx, |view, _, cx| {
                view.settings.probing = false;
                view.settings.probe = Some(result);
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let saved = self.engine.settings();
        let form = &self.settings;

        let appearance = card(cx)
            .child(section_title("Appearance"))
            .child(
                div().flex().gap_2().children(
                    [
                        (ThemePreference::System, "System"),
                        (ThemePreference::Light, "Light"),
                        (ThemePreference::Dark, "Dark"),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(index, (preference, label))| {
                        Button::new(("theme", index))
                            .outline()
                            .label(label)
                            .selected(saved.theme == preference)
                            .on_click(cx.listener(move |view, _, window, cx| view.theme(preference, window, cx)))
                    }),
                ),
            )
            .child(muted("Appearance is saved automatically.", cx));

        let provider_choice =
            div().flex().gap_2().children([(false, "This Mac"), (true, "Remote server")].map(|(remote, label)| {
                Button::new(if remote { "anisette-remote" } else { "anisette-local" })
                    .outline()
                    .label(label)
                    .selected(form.remote == remote)
                    .debug_selector(move || format!("anisette:{}", if remote { "remote" } else { "local" }))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.settings.remote = remote;
                        view.settings.notice = None;
                        cx.notify();
                    }))
            }));

        let probe = match &form.probe {
            Some(Ok(description)) => Some(muted(format!("Apple will see: {description}"), cx)),
            Some(Err(error)) => Some(danger_text(format!("Provider test failed: {error}"), cx)),
            None => None,
        };

        let anisette = card(cx)
            .child(section_title("Apple sign-in provider (anisette)"))
            .child(muted(
                "Apple requires machine identification data with every sign-in. Use this Mac's own data or a \
                 remote anisette server.",
                cx,
            ))
            .child(provider_choice)
            .when(!form.remote && !self.engine.is_demo(), |this| {
                this.child(muted(
                    "This Mac's own provisioning is tried first. Recent macOS versions can refuse it; \
                     sign-in then uses the alternate server below, so set one.",
                    cx,
                ))
            })
            .when(form.remote, |this| this.child(field("Server URL", &form.remote_url, false)))
            .child(field("Alternate server URL (tried after a provider mismatch)", &form.alternate_url, false))
            .child(
                div().flex().items_center().gap_3().child(
                    Button::new("test-anisette")
                        .outline()
                        .label("Test provider")
                        .loading(form.probing)
                        .disabled(form.probing)
                        .on_click(cx.listener(|view, _, window, cx| view.test_anisette(window, cx))),
                ),
            )
            .children(probe);

        let autostart_note = match (&self.daemon_program, self.engine.is_demo()) {
            (_, true) => "Starting at login is unavailable in the demo.".to_owned(),
            (Some(program), false) => format!("Runs {} daemon at login; saved immediately.", program.display()),
            (None, false) => "Starting at login needs the sideport command-line tool beside this app.".to_owned(),
        };

        let refresh = card(cx)
            .child(section_title("Automatic refresh"))
            .child(checkbox(
                "refresh-enabled",
                "Refresh tracked apps automatically",
                form.refresh_enabled,
                cx,
                |form, checked| form.refresh_enabled = checked,
            ))
            .child(
                Checkbox::new("autostart")
                    .label("Keep refreshing after this window closes (start at login)")
                    .checked(self.engine.autostart())
                    .disabled(self.daemon_program.is_none() || self.engine.is_demo())
                    .debug_selector(|| "autostart".into())
                    .on_click(cx.listener(|view, checked, _, cx| view.set_autostart(*checked, cx))),
            )
            .child(muted(autostart_note, cx))
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(field("Refresh when fewer hours remain than", &form.threshold, !form.refresh_enabled))
                    .child(field("Check every (minutes)", &form.interval, !form.refresh_enabled)),
            )
            .child(checkbox(
                "refresh-network",
                "Allow refreshing over Wi-Fi (the device screen must be on)",
                form.refresh_network,
                cx,
                |form, checked| form.refresh_network = checked,
            ));

        let defaults = card(cx)
            .child(section_title("Defaults"))
            .child(checkbox(
                "default-remember",
                "Remember passwords when signing in",
                form.remember_passwords,
                cx,
                |form, checked| form.remember_passwords = checked,
            ))
            .child(checkbox(
                "default-stream",
                "Stream uploads to devices without a temporary IPA",
                form.stream_upload,
                cx,
                |form, checked| form.stream_upload = checked,
            ));

        let actions = div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                Button::new("save-settings")
                    .primary()
                    .label("Save settings")
                    .debug_selector(|| "save-settings".into())
                    .on_click(cx.listener(|view, _, _, cx| view.save_settings(cx))),
            )
            .child(Button::new("revert-settings").ghost().label("Revert").on_click(cx.listener(
                |view, _, window, cx| {
                    let saved = view.engine.settings();
                    view.settings.load(&saved, window, cx);
                    cx.notify();
                },
            )))
            .when_some(form.notice.clone(), |this, notice| this.child(muted(notice, cx)));

        div()
            .max_w(px(760.))
            .mx_auto()
            .flex()
            .flex_col()
            .gap_5()
            .child(page_title("Settings"))
            .child(appearance)
            .child(anisette)
            .child(refresh)
            .child(defaults)
            .child(actions)
            .child(self.render_services(cx))
            .into_any_element()
    }
}

/// A form checkbox whose click updates the unsaved settings.
fn checkbox(
    id: &'static str,
    label: &'static str,
    checked: bool,
    cx: &mut Context<Sideport>,
    change: impl Fn(&mut SettingsForm, bool) + 'static,
) -> impl IntoElement {
    Checkbox::new(id).label(label).checked(checked).debug_selector(move || id.into()).on_click(cx.listener(
        move |view, checked, _, cx| {
            change(&mut view.settings, *checked);
            view.settings.notice = None;
            cx.notify();
        },
    ))
}

/// The program the login item runs, beside this executable (`Contents/MacOS` in the packaged
/// app): the menu-bar daemon `sideport-tray` when present, else the `sideport` tool. Both accept
/// the login item's `daemon` argument.
pub(super) fn daemon_program() -> Option<std::path::PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let names: [&str; 2] =
        if cfg!(windows) { ["sideport-tray.exe", "sideport.exe"] } else { ["sideport-tray", "sideport"] };

    names.into_iter().map(|name| executable.with_file_name(name)).find(|program| program.is_file())
}

#[cfg(test)]
mod tests {
    use super::{INTERVAL_MINUTES, THRESHOLD_HOURS, web_url, whole_number};

    #[test]
    fn provider_urls_and_refresh_numbers_are_validated() {
        assert_eq!(web_url(" https://ani.example/v3 ", "URL"), Ok("https://ani.example/v3".into()));
        assert!(web_url("http://127.0.0.1:6969", "URL").is_ok());

        for invalid in ["", "https://", "ftp://ani.example", "https://ani example", "ani.example"] {
            assert!(web_url(invalid, "URL").is_err(), "{invalid}");
        }

        assert_eq!(whole_number("48", THRESHOLD_HOURS, "hours"), Ok(48));
        assert_eq!(whole_number(" 1440 ", INTERVAL_MINUTES, "minutes"), Ok(1440));

        for invalid in ["0", "721", "-1", "1.5", "", "many"] {
            assert!(whole_number(invalid, THRESHOLD_HOURS, "hours").is_err(), "{invalid}");
        }
    }
}
