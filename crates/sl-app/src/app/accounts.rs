//! Apple ID accounts: sign-in, session import, default team, certificates and App IDs.

use super::{
    Confirmation, PendingAction, Quota, Sideport,
    widgets::{badge, card, format_time, labelled, muted, page_title, section_title},
};
use crate::{picker, prompt::team_kind};
use gpui::{
    AnyElement, AppContext, Context, Entity, FontWeight, IntoElement, SharedString, Task, Window, div, prelude::*, px,
};
use gpui_component::{
    Disableable, Selectable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
};
use sl_engine::{AccountSummary, AppIdSummary, CertificateSummary, Engine, SessionImport};
use std::{collections::BTreeMap, path::PathBuf};

/// A per-account command started from the account's card.
type AccountAction = fn(&mut Sideport, String, &mut Window, &mut Context<Sideport>);

pub(crate) struct Accounts {
    pub(crate) list: Vec<AccountSummary>,
    pub(crate) apple_id: Entity<InputState>,
    pub(crate) password: Entity<InputState>,
    pub(crate) remember: bool,
    /// Certificates of one account, as last listed.
    pub(crate) certificates: Option<(String, Vec<CertificateSummary>)>,
    /// App IDs of one account, as last listed.
    pub(crate) app_ids: Option<(String, Vec<AppIdSummary>)>,
    /// Free-team App ID availability reported by jobs, per account.
    pub(crate) quota: BTreeMap<String, Quota>,
    /// Sideloadly's `sessions.json`; tests replace the platform default.
    pub(crate) sessions_path: Option<PathBuf>,
    pub(crate) import: Option<SessionImport>,
    pub(crate) importing: bool,
    task: Option<Task<()>>,
}

impl Accounts {
    pub(crate) fn new(remember: bool, window: &mut Window, cx: &mut gpui::App) -> Self {
        let apple_id = cx.new(|cx| InputState::new(window, cx).placeholder("name@example.com"));
        let password = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("Optional"));

        Self {
            list: Vec::new(),
            apple_id,
            password,
            remember,
            certificates: None,
            app_ids: None,
            quota: BTreeMap::new(),
            sessions_path: Engine::recovered_sessions_path(),
            import: None,
            importing: false,
            task: None,
        }
    }

    fn forget_details(&mut self, apple_id: &str) {
        if self.certificates.as_ref().is_some_and(|(owner, _)| owner == apple_id) {
            self.certificates = None;
        }
        if self.app_ids.as_ref().is_some_and(|(owner, _)| owner == apple_id) {
            self.app_ids = None;
        }

        self.quota.remove(apple_id);
    }
}

/// Enter in either sign-in field starts the sign-in.
pub(super) fn submit_on_enter(accounts: &Accounts, window: &mut Window, cx: &mut Context<Sideport>) {
    for input in [&accounts.apple_id, &accounts.password] {
        cx.subscribe_in(input, window, |view, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                view.sign_in(window, cx);
            }
        })
        .detach();
    }
}

impl Sideport {
    pub(super) fn reload_accounts(&mut self) {
        match self.engine.accounts() {
            Ok(list) => self.accounts.list = list,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        }

        let list = &self.accounts.list;
        let known = |apple_id: &String| list.iter().any(|account| &account.apple_id == apple_id);

        if !self.draft.apple_id.as_ref().is_some_and(known) {
            self.draft.apple_id = list.first().map(|account| account.apple_id.clone());
        }
    }

    pub(super) fn sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }

        let apple_id = self.accounts.apple_id.read(cx).value().trim().to_string();
        let password = self.accounts.password.read(cx).value().to_string();

        if apple_id.is_empty() || apple_id.contains(char::is_whitespace) {
            self.error = Some("Enter your Apple ID email address or phone number.".into());
            cx.notify();
            return;
        }

        self.accounts.password.update(cx, |input, cx| input.set_value("", window, cx));

        let password = (!password.is_empty()).then_some(password);
        let job = self.engine.login(apple_id.clone(), password, self.accounts.remember);
        self.job_account = Some(apple_id);

        self.run_job(job, "Signing in…", window, cx, |view, account, window, cx| {
            view.status = format!("Signed in as {}", account.apple_id);
            view.accounts.apple_id.update(cx, |input, cx| input.set_value("", window, cx));
            view.draft.apple_id = Some(account.apple_id);
        });
    }

    fn sign_out(&mut self, apple_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }

        let logout = self.engine.logout(apple_id.clone());
        self.status = format!("Signing out {apple_id}…");
        self.error = None;

        self.accounts.task = Some(cx.spawn_in(window, async move |view, cx| {
            let result = logout.await;

            let _ = view.update_in(cx, |view, _, cx| {
                match result {
                    Ok(()) => {
                        view.status = format!("Signed out {apple_id}");
                        view.accounts.forget_details(&apple_id);
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }

                view.reload_accounts();
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Import Sideloadly sessions from `path`, or from the recovered default location.
    pub(super) fn import_sessions(&mut self, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() || self.accounts.importing {
            return;
        }

        let Some(path) = path.or_else(|| self.accounts.sessions_path.clone()) else {
            self.error = Some("Sideloadly's session file location is unknown here. Choose the file instead.".into());
            cx.notify();
            return;
        };

        if !path.is_file() {
            self.error = Some(format!("No Sideloadly sessions were found at {}.", path.display()));
            cx.notify();
            return;
        }

        let engine = self.engine.clone();
        let import = cx.background_spawn(async move { engine.import_sessions(path) });
        self.accounts.importing = true;
        self.accounts.import = None;
        self.error = None;
        self.status = "Importing Sideloadly sessions…".into();

        self.accounts.task = Some(cx.spawn_in(window, async move |view, cx| {
            let imported = import.await;

            let _ = view.update_in(cx, |view, _, cx| {
                view.accounts.importing = false;

                match imported {
                    Ok(report) => {
                        view.status = format!("Imported {} Sideloadly session(s)", report.imported.len());
                        view.accounts.import = Some(report);
                    }
                    Err(error) => {
                        view.error = Some(error.to_string());
                        view.status = "Import failed".into();
                    }
                }

                view.reload_accounts();
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn choose_sessions_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }

        let paths = picker::paths(window, cx, "Choose Sideloadly's sessions.json", false);
        self.picking = true;

        self.picker = Some(cx.spawn_in(window, async move |view, cx| {
            let result = paths.await;

            let _ = view.update_in(cx, |view, window, cx| {
                view.picking = false;
                view.focus.focus(window);

                match result {
                    Ok(Some(paths)) => {
                        if let Some(path) = paths.into_iter().next() {
                            view.import_sessions(Some(path), window, cx);
                        }
                    }
                    Err(error) => view.error = Some(error.to_string()),
                    _ => {}
                }

                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Choose the team jobs use without asking, or `None` to be asked at the next job. The engine
    /// keeps the team answered there as the new default, as the recovered client chooses once.
    fn choose_default_team(&mut self, apple_id: String, team_id: Option<String>, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }

        let saved = self.engine.set_default_team(&apple_id, team_id.clone());

        match (saved, team_id) {
            (Ok(()), Some(team_id)) => {
                self.status = format!("{apple_id} signs with team {team_id} without asking");
                self.error = None;
            }
            (Ok(()), None) => {
                self.status = format!("Sideport asks which team {apple_id} signs with at the next job");
                self.error = None;
            }
            (Err(error), _) => self.error = Some(format!("Could not change the default team: {error}")),
        }

        self.reload_accounts();
        cx.notify();
    }

    fn load_certificates(&mut self, apple_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }

        let job = self.engine.certificates(apple_id.clone());
        self.job_account = Some(apple_id.clone());

        self.run_job(job, "Listing certificates…", window, cx, move |view, certificates, _, _| {
            view.status = format!("{} certificate(s) for {apple_id}", certificates.len());
            view.accounts.certificates = Some((apple_id, certificates));
        });
    }

    fn load_app_ids(&mut self, apple_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }

        let job = self.engine.app_ids(apple_id.clone());
        self.job_account = Some(apple_id.clone());

        self.run_job(job, "Listing App IDs…", window, cx, move |view, app_ids, _, _| {
            view.status = format!("{} App ID(s) for {apple_id}", app_ids.len());
            view.accounts.app_ids = Some((apple_id, app_ids));
        });
    }

    fn confirm_revoke(
        &mut self,
        apple_id: String,
        certificate: &CertificateSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message = format!(
            "Revoke \"{}\" (serial {})? Apps signed with it stop launching, and other computers using it must \
             create a new certificate.",
            certificate.name, certificate.serial
        );
        let confirmation = Confirmation {
            title: "Revoke certificate?".into(),
            message,
            confirm_label: "Revoke",
            action: PendingAction::RevokeCertificate { apple_id, serial: certificate.serial.clone() },
        };

        self.confirm(confirmation, window, cx);
    }

    pub(super) fn revoke_certificate(
        &mut self,
        apple_id: String,
        serial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let job = self.engine.revoke_certificate(apple_id.clone(), serial.clone());
        self.job_account = Some(apple_id.clone());

        self.run_job(job, "Revoking certificate…", window, cx, move |view, (), _, _| {
            view.status = format!("Revoked certificate {serial}");

            if let Some((owner, certificates)) = &mut view.accounts.certificates
                && owner == &apple_id
            {
                certificates.retain(|certificate| certificate.serial != serial);
            }
        });
    }

    pub(super) fn render_accounts(&self, cx: &mut Context<Self>) -> AnyElement {
        let locked = self.occupied();

        let sign_in = card(cx)
            .child(section_title("Sign in with an Apple ID"))
            .child(muted(
                "Leave the password empty to use a remembered password or to be asked for it. \
                 Verification codes are requested when Apple sends them.",
                cx,
            ))
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(labelled("Apple ID", Input::new(&self.accounts.apple_id).disabled(locked)))
                    .child(labelled("Password", Input::new(&self.accounts.password).mask_toggle().disabled(locked))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        Checkbox::new("remember-password")
                            .label("Remember password (enables unattended refresh)")
                            .checked(self.accounts.remember)
                            .disabled(locked)
                            .debug_selector(|| "remember-password".into())
                            .on_click(cx.listener(|view, checked, _, cx| {
                                view.accounts.remember = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("sign-in")
                            .primary()
                            .label("Sign in")
                            .disabled(locked)
                            .debug_selector(|| "sign-in".into())
                            .on_click(cx.listener(|view, _, window, cx| view.sign_in(window, cx))),
                    ),
            );

        let default_path = self.accounts.sessions_path.as_ref().map(|path| path.display().to_string());
        let import_description = match &default_path {
            Some(path) => format!("Reuse sessions saved by Sideloadly at {path}."),
            None => "Reuse sessions saved by Sideloadly.".into(),
        };

        let import = card(cx)
            .child(section_title("Import from Sideloadly"))
            .child(muted(import_description, cx))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("import-sessions")
                            .outline()
                            .label("Import Sideloadly sessions")
                            .loading(self.accounts.importing)
                            .disabled(locked || self.accounts.importing || default_path.is_none())
                            .debug_selector(|| "import-sessions".into())
                            .on_click(cx.listener(|view, _, window, cx| view.import_sessions(None, window, cx))),
                    )
                    .child(
                        Button::new("choose-sessions")
                            .ghost()
                            .label("Choose file…")
                            .disabled(locked || self.accounts.importing)
                            .on_click(cx.listener(|view, _, window, cx| view.choose_sessions_file(window, cx))),
                    ),
            )
            .when_some(self.accounts.import.as_ref(), |this, report| this.child(import_report(report, cx)));

        let accounts = div()
            .flex()
            .flex_col()
            .gap_4()
            .when(self.accounts.list.is_empty(), |this| {
                this.child(card(cx).child(section_title("No accounts yet")).child(muted(
                    "Sign in below or import Sideloadly sessions to provision and install apps with an Apple ID.",
                    cx,
                )))
            })
            .children(
                self.accounts
                    .list
                    .iter()
                    .enumerate()
                    .map(|(index, account)| self.render_account(index, account, locked, cx)),
            );

        div()
            .max_w(px(1100.))
            .mx_auto()
            .flex()
            .flex_col()
            .gap_5()
            .child(page_title("Accounts"))
            .child(accounts)
            .child(sign_in)
            .child(import)
            .into_any_element()
    }

    fn render_account(
        &self,
        index: usize,
        account: &AccountSummary,
        locked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let apple_id = account.apple_id.clone();
        let selector = format!("account:{apple_id}");
        let session = if account.has_session { "Signed in" } else { "Sign-in required" };
        let last_login = account.last_login.map(|time| format!("Last sign-in {}", format_time(time)));

        let teams: Vec<_> = account
            .teams
            .iter()
            .map(|team| {
                let default = account.default_team.as_ref() == Some(&team.team_id);
                let description = format!("{} ({}) · {}", team.name, team.team_id, team_kind(&team.kind));

                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .child(description)
                    .when(default, |this| this.child(badge("Default", cx)))
            })
            .collect();

        let default_team = (account.teams.len() > 1).then(|| render_default_team(account, locked, cx));

        let quota = self.accounts.quota.get(&apple_id).map(|quota| {
            let release = quota.next_release.map(|time| format!(" · next frees {}", format_time(time)));

            muted(format!("Free-team App IDs left: {}{}", quota.remaining, release.unwrap_or_default()), cx)
        });

        let action = |id: &'static str, label: &'static str, run: AccountAction, cx: &mut Context<Self>| {
            let selector_id = apple_id.clone();
            let target = apple_id.clone();

            Button::new((id, index))
                .outline()
                .label(label)
                .disabled(locked)
                .debug_selector(move || format!("{id}:{selector_id}"))
                .on_click(cx.listener(move |view, _, window, cx| run(view, target.clone(), window, cx)))
        };

        let actions = div()
            .flex()
            .gap_2()
            .child(action("certificates", "Certificates", Self::load_certificates, cx))
            .child(action("app-ids", "App IDs", Self::load_app_ids, cx))
            .child(action("sign-out", "Sign out", Self::sign_out, cx));

        let certificates = self
            .accounts
            .certificates
            .as_ref()
            .filter(|(owner, _)| owner == &apple_id)
            .map(|(_, certificates)| self.render_certificates(&apple_id, certificates, locked, cx));
        let app_ids = self
            .accounts
            .app_ids
            .as_ref()
            .filter(|(owner, _)| owner == &apple_id)
            .map(|(_, app_ids)| render_app_ids(app_ids, cx));

        card(cx)
            .debug_selector(move || selector)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(apple_id.clone()))
                    .child(badge(session, cx))
                    .when(account.remembers_password, |this| this.child(badge("Password saved", cx))),
            )
            .when_some(last_login, |this, text| this.child(muted(text, cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(section_title("Teams"))
                    .when(account.teams.is_empty(), |this| {
                        this.child(muted("Teams are listed after the developer service is reached.", cx))
                    })
                    .children(teams),
            )
            .children(default_team)
            .children(quota)
            .child(actions)
            .children(certificates)
            .children(app_ids)
            .into_any_element()
    }

    fn render_certificates(
        &self,
        apple_id: &str,
        certificates: &[CertificateSummary],
        locked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = certificates.iter().enumerate().map(|(index, certificate)| {
            let apple_id = apple_id.to_owned();
            let serial = certificate.serial.clone();
            let summary = certificate.clone();
            let machine = certificate.machine_name.as_deref().unwrap_or("Unknown machine");
            let expires = certificate.expires.map(|time| format!(" · expires {}", format_time(time)));
            let detail = format!("Serial {} · {machine}{}", certificate.serial, expires.unwrap_or_default());

            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_sm()
                                .child(certificate.name.clone())
                                .when(certificate.is_ours, |this| this.child(badge("This Mac", cx))),
                        )
                        .child(muted(detail, cx)),
                )
                .child(
                    Button::new(("revoke", index))
                        .danger()
                        .label("Revoke")
                        .disabled(locked)
                        .debug_selector(move || format!("revoke:{serial}"))
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.confirm_revoke(apple_id.clone(), &summary, window, cx)
                        })),
                )
        });

        div()
            .flex()
            .flex_col()
            .gap_3()
            .pt_2()
            .child(section_title("Development certificates"))
            .when(certificates.is_empty(), |this| this.child(muted("No development certificates.", cx)))
            .children(rows)
            .into_any_element()
    }
}

/// The team jobs use without asking, or "Ask at next job"; shown for accounts with several teams.
fn render_default_team(account: &AccountSummary, locked: bool, cx: &mut Context<Sideport>) -> AnyElement {
    let apple_id = account.apple_id.clone();
    let scope = SharedString::from(format!("default-team:{apple_id}"));

    let teams = account.teams.iter().map(|team| (Some(team.team_id.clone()), team.name.clone()));
    let choices = teams.chain([(None, "Ask at next job".to_string())]);

    let buttons = choices.enumerate().map(|(index, (team_id, label))| {
        let selected = account.default_team == team_id;
        let selector = format!("{scope}:{}", team_id.as_deref().unwrap_or("ask"));
        let apple_id = apple_id.clone();
        let choose = cx.listener(move |view, _, _, cx| view.choose_default_team(apple_id.clone(), team_id.clone(), cx));

        Button::new((scope.clone(), index))
            .outline()
            .label(label)
            .selected(selected)
            .disabled(locked)
            .debug_selector(move || selector.clone())
            .on_click(choose)
    });

    let explanation = match &account.default_team {
        Some(_) => "Jobs sign with this team without asking.",
        None => "The next job asks which team to sign with and keeps the answer as the default.",
    };

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(section_title("Default team"))
        .child(div().flex().flex_wrap().gap_2().children(buttons))
        .child(muted(explanation, cx))
        .into_any_element()
}

fn render_app_ids(app_ids: &[AppIdSummary], cx: &mut Context<Sideport>) -> AnyElement {
    let rows = app_ids.iter().map(|app_id| {
        let expires = app_id.expires.map(|time| format!(" · expires {}", format_time(time)));

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().child(app_id.identifier.clone()))
            .child(muted(format!("{}{}", app_id.name, expires.unwrap_or_default()), cx))
    });

    div()
        .flex()
        .flex_col()
        .gap_3()
        .pt_2()
        .child(section_title("App IDs"))
        .when(app_ids.is_empty(), |this| this.child(muted("No App IDs are registered.", cx)))
        .children(rows)
        .into_any_element()
}

fn import_report(report: &SessionImport, cx: &mut Context<Sideport>) -> AnyElement {
    let imported = if report.imported.is_empty() {
        "No sessions were imported.".to_string()
    } else {
        format!("Imported: {}", report.imported.join(", "))
    };
    let skipped = report.skipped.iter().map(|(entry, reason)| muted(format!("Skipped {entry}: {reason}"), cx));

    div()
        .debug_selector(|| "import-report".into())
        .flex()
        .flex_col()
        .gap_1()
        .text_sm()
        .child(imported)
        .children(skipped)
        .into_any_element()
}
