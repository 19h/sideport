use crate::{Draft, ExportMode, draft::validate_relative, picker, prompt::PromptDialog};
use chrono::{DateTime, Utc};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, Image, ImageFormat, Task, Window, actions, prelude::*, px, rgb,
};
use gpui_component::{Theme, ThemeMode, input::InputState};
use sl_engine::{
    AppSummary, Connection, Engine, EngineError, Fact, FileReplacement, JobEvent, JobHandle, JobOutcome, JobSpec,
    LibraryInjection, LogLevel, PromptKind, PromptReply, SigningMode, Stage, Target, TeamSummary, ThemePreference,
};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

actions!(
    sideport,
    [OpenApp, ExportApp, CancelJob, ShowApp, ShowAccounts, ShowDevices, ShowInstallations, ShowSettings]
);

mod accounts;
mod devices;
mod installations;
mod ipc;
mod settings;
mod signing;
mod view;
mod widgets;

const MAX_LOG_LINES: usize = 400;

#[cfg(test)]
mod tests;

/// Top-level areas of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Section {
    #[default]
    App,
    Accounts,
    Devices,
    Installations,
    Settings,
}

impl Section {
    pub(crate) const ALL: [Self; 5] = [Self::App, Self::Accounts, Self::Devices, Self::Installations, Self::Settings];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::App => "App",
            Self::Accounts => "Accounts",
            Self::Devices => "Devices",
            Self::Installations => "Installations",
            Self::Settings => "Settings",
        }
    }
}

/// Where the editor sends the prepared app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Destination {
    #[default]
    Export,
    Device,
}

impl Destination {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Export => "Export IPA",
            Self::Device => "Install on device",
        }
    }
}

/// Free-team App ID availability reported by a provisioning job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Quota {
    pub(crate) remaining: u32,
    pub(crate) next_release: Option<DateTime<Utc>>,
}

/// Structured facts of the current or last job, shown beside its progress.
#[derive(Debug, Clone, Default)]
pub(crate) struct JobFacts {
    pub(crate) team: Option<TeamSummary>,
    pub(crate) bundle_id: Option<String>,
    pub(crate) quota: Option<Quota>,
    pub(crate) expires: Option<DateTime<Utc>>,
    pub(crate) ttl_days: Option<u64>,
    pub(crate) anisette_device: Option<String>,
    pub(crate) encrypted: bool,
}

struct Fields {
    name: Entity<InputState>,
    identifier: Entity<InputState>,
    version: Entity<InputState>,
    short_version: Entity<InputState>,
    minimum_os: Entity<InputState>,
    overrides: Entity<InputState>,
    upload_chunk: Entity<InputState>,
}

impl Fields {
    fn new(window: &mut Window, cx: &mut App) -> Self {
        let mut input = |placeholder: &'static str| cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let name = input("App name");
        let identifier = input("com.example.app");
        let version = input("Build number");
        let short_version = input("Release version");
        let minimum_os = input("Minimum OS version");
        let upload_chunk = input("1");
        let overrides = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("{\"CustomKey\": \"value\", \"RemoveThisKey\": null}")
        });

        Self { name, identifier, version, short_version, minimum_os, overrides, upload_chunk }
    }

    fn load(&self, draft: &Draft, window: &mut Window, cx: &mut App) {
        for (field, value) in [
            (&self.name, &draft.name),
            (&self.identifier, &draft.identifier),
            (&self.version, &draft.version),
            (&self.short_version, &draft.short_version),
            (&self.minimum_os, &draft.minimum_os),
            (&self.overrides, &draft.extra_info),
            (&self.upload_chunk, &draft.upload_chunk),
        ] {
            field.update(cx, |input, cx| input.set_value(value.clone(), window, cx));
        }
    }

    fn store(&self, draft: &mut Draft, cx: &App) {
        draft.name = self.name.read(cx).value().to_string();
        draft.identifier = self.identifier.read(cx).value().to_string();
        draft.version = self.version.read(cx).value().to_string();
        draft.short_version = self.short_version.read(cx).value().to_string();
        draft.minimum_os = self.minimum_os.read(cx).value().to_string();
        draft.extra_info = self.overrides.read(cx).value().to_string();
        draft.upload_chunk = self.upload_chunk.read(cx).value().to_string();
    }
}

struct FileDialog {
    target: Entity<InputState>,
    source: Option<PathBuf>,
}

/// A destructive action the window asks about before performing it.
pub(crate) enum PendingAction {
    RevokeCertificate { apple_id: String, serial: String },
    UninstallApp { udid: String, bundle_id: String },
    RemoveProfile { udid: String, uuid: String },
    ForgetInstallation { id: i64 },
}

pub(crate) struct Confirmation {
    pub(crate) title: String,
    pub(crate) message: String,
    pub(crate) confirm_label: &'static str,
    pub(crate) action: PendingAction,
}

enum Dialog {
    Prompt(PromptDialog),
    File(FileDialog),
    Confirm(Confirmation),
}

impl Dialog {
    fn kind(&self) -> &'static str {
        match self {
            Self::Prompt(_) => "prompt",
            Self::File(_) => "file",
            Self::Confirm(_) => "confirmation",
        }
    }
}

/// The desktop root owns task handles so closing it cancels outstanding work.
pub struct Sideport {
    engine: Engine,
    focus: FocusHandle,
    section: Section,
    app: Option<AppSummary>,
    icon: Option<Arc<Image>>,
    draft: Draft,
    fields: Fields,
    mode: ExportMode,
    destination: Destination,
    busy: bool,
    closing: bool,
    cancellation: Option<CancellationToken>,
    operation: Option<Task<()>>,
    picker: Option<Task<()>>,
    picking: bool,
    dialog: Option<Dialog>,
    dialog_error: Option<String>,
    error: Option<String>,
    outcome: Option<JobOutcome>,
    status: String,
    stage: Option<Stage>,
    progress: Option<(u64, u64)>,
    facts: JobFacts,
    /// Apple ID of the running job, so reported quotas are attributed to its account.
    job_account: Option<String>,
    logs: VecDeque<(LogLevel, String)>,
    accounts: accounts::Accounts,
    devices: devices::Devices,
    installations: installations::Installations,
    settings: settings::SettingsForm,
    advanced_open: bool,
    logs_open: bool,
}

impl std::fmt::Debug for Sideport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Sideport")
            .field("section", &self.section)
            .field("has_app", &self.app.is_some())
            .field("mode", &self.mode)
            .field("destination", &self.destination)
            .field("busy", &self.busy)
            .field("stage", &self.stage)
            .field("dialog", &self.dialog.as_ref().map(Dialog::kind))
            .field("accounts", &self.accounts.list.len())
            .field("devices", &self.devices.list.len())
            .field("installations", &self.installations.list.len())
            .finish_non_exhaustive()
    }
}

impl Drop for Sideport {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
    }
}

impl Sideport {
    pub fn new(engine: Engine, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = engine.settings();
        let focus = cx.focus_handle();
        let fields = Fields::new(window, cx);
        let accounts = accounts::Accounts::new(settings.remember_passwords, window, cx);
        let settings_form = settings::SettingsForm::new(&settings, window, cx);
        let weak = cx.weak_entity();

        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |view, cx| view.prepare_close(window, cx)).unwrap_or(true)
        });
        cx.observe_window_appearance(window, |view, window, cx| {
            if view.engine.settings().theme == ThemePreference::System {
                apply_theme(ThemePreference::System, window, cx);
                cx.notify();
            }
        })
        .detach();
        accounts::submit_on_enter(&accounts, window, cx);
        focus.focus(window);

        let mut sideport = Self {
            engine,
            focus,
            section: Section::default(),
            app: None,
            icon: None,
            draft: Draft::default(),
            fields,
            mode: ExportMode::default(),
            destination: Destination::default(),
            busy: false,
            closing: false,
            cancellation: None,
            operation: None,
            picker: None,
            picking: false,
            dialog: None,
            dialog_error: None,
            error: None,
            outcome: None,
            status: "Choose an app to begin".into(),
            stage: None,
            progress: None,
            facts: JobFacts::default(),
            job_account: None,
            logs: VecDeque::new(),
            accounts,
            devices: devices::Devices::default(),
            installations: installations::Installations::default(),
            settings: settings_form,
            advanced_open: false,
            logs_open: false,
        };

        sideport.reload_accounts();
        sideport.reload_installations();
        sideport.watch_refresh(window, cx);

        sideport
    }

    /// Whether a job, picker, or dialog currently owns the window's interaction.
    fn occupied(&self) -> bool {
        self.busy || self.picking || self.dialog.is_some()
    }

    pub(crate) fn show_section(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        self.section = section;

        match section {
            Section::App => {}
            Section::Accounts => self.reload_accounts(),
            Section::Devices => {
                self.watch_devices(window, cx);
                self.load_device_contents(window, cx);
            }
            Section::Installations => self.reload_installations(),
            Section::Settings => self.settings.load(&self.engine.settings(), window, cx),
        }

        self.focus.focus(window);
        cx.notify();
    }

    pub fn load_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }

        self.section = Section::App;
        self.app = None;
        self.icon = None;
        self.busy = true;
        self.error = None;
        self.outcome = None;
        self.status = "Reading app…".into();
        self.stage = None;
        self.progress = None;
        self.facts = JobFacts::default();
        self.logs.clear();

        let job = self.engine.inspect_job(path);
        self.cancellation = Some(job.cancellation_token());
        self.operation = Some(cx.spawn_in(window, async move |view, cx| {
            let result = job.result().await;

            let _ = view.update_in(cx, |view, window, cx| {
                match result {
                    Ok(app) => {
                        view.icon =
                            app.icon_png.as_ref().map(|png| Arc::new(Image::from_bytes(ImageFormat::Png, png.clone())));
                        view.draft = view.fresh_draft(&app);
                        view.fields.load(&view.draft, window, cx);
                        view.status = "Ready".into();

                        for warning in &app.warnings {
                            view.add_log(LogLevel::Warn, warning.clone());
                        }

                        view.app = Some(app);
                    }
                    Err(error) => view.failed(error),
                }

                view.finished(window, cx);
            });
        }));
        cx.notify();
    }

    /// A new draft for `app` that keeps the chosen account and applies the saved defaults.
    fn fresh_draft(&self, app: &AppSummary) -> Draft {
        let mut draft = Draft::for_app(app);

        draft.apple_id = self.draft.apple_id.clone();
        draft.options.stream_upload = self.engine.settings().stream_upload;
        draft.options.track_for_refresh = true;

        draft
    }

    fn open_app(&mut self, _: &OpenApp, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }

        let paths = picker::paths(window, cx, "Choose an IPA, app ZIP, or .app", false);
        self.picking = true;
        self.section = Section::App;

        self.picker = Some(cx.spawn_in(window, async move |view, cx| {
            let result = paths.await;

            let _ = view.update_in(cx, |view, window, cx| {
                view.picking = false;
                view.focus.focus(window);
                window.activate_window();

                match result {
                    Ok(Some(paths)) => {
                        if let Some(path) = paths.into_iter().next() {
                            view.load_path(path, window, cx);
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

    /// The editor's primary action: export or install, following the destination.
    fn primary_action(&mut self, _: &ExportApp, window: &mut Window, cx: &mut Context<Self>) {
        if self.section != Section::App {
            return;
        }

        match self.destination {
            Destination::Export => self.export(window, cx),
            Destination::Device => self.install(window, cx),
        }
    }

    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }
        let Some(app) = &self.app else {
            return;
        };

        if let Some(reason) = self.action_blocker() {
            self.error = Some(reason);
            cx.notify();
            return;
        }

        self.fields.store(&mut self.draft, cx);
        let spec = match self.draft.job(app, self.mode, None) {
            Ok(spec) => spec,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let label = match self.mode {
            ExportMode::Original => "Original",
            ExportMode::AppleId => "Signed",
            _ => "Prepared",
        };
        let suggested = format!("{} {label}.ipa", app.path.file_stem().unwrap_or_default().to_string_lossy());
        let parent = app.path.parent().unwrap_or(Path::new("."));
        let path = picker::destination(window, cx, parent, &suggested);
        self.error = None;
        self.busy = true;
        self.status = "Choose an output file…".into();

        self.picker = Some(cx.spawn_in(window, async move |view, cx| {
            let result = path.await;

            let _ = view.update_in(cx, |view, window, cx| {
                view.busy = false;
                view.focus.focus(window);
                window.activate_window();

                match result {
                    Ok(Some(path)) => {
                        let mut spec = spec;
                        spec.target = Target::ExportIpa { path: Some(path) };
                        view.start_job(spec, window, cx);
                    }
                    Err(error) => {
                        view.error = Some(error.to_string());
                        view.status = "Export did not start".into();
                    }
                    _ => {
                        view.status = "Ready".into();
                    }
                }

                cx.notify();
            });
        }));
        cx.notify();
    }

    fn install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }
        let Some(app) = &self.app else {
            return;
        };

        if let Some(reason) = self.action_blocker() {
            self.error = Some(reason);
            cx.notify();
            return;
        }
        let Some(device) = self.selected_device() else {
            return;
        };

        let prefer_network = !device.connections.contains(&Connection::Usb);
        let target = Target::Device { udid: device.udid.clone(), prefer_network };

        self.fields.store(&mut self.draft, cx);
        let spec = match self.draft.spec(app, self.mode, target) {
            Ok(spec) => spec,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };

        self.error = None;
        self.start_job(spec, window, cx);
    }

    fn start_job(&mut self, spec: JobSpec, window: &mut Window, cx: &mut Context<Self>) {
        let installing = matches!(spec.target, Target::Device { .. });

        self.outcome = None;
        self.job_account = match &spec.signing {
            SigningMode::AppleId { apple_id } => Some(apple_id.clone()),
            _ => None,
        };

        let job = self.engine.start(spec);
        self.run_job(job, "Preparing…", window, cx, move |view, outcome, _, _| {
            view.status = if installing { "Installed".into() } else { "Export complete".into() };
            view.outcome = Some(outcome);
        });
        self.stage = Some(Stage::Preparing);
    }

    /// Run an engine job in the window's single job slot: its events drive progress, facts and
    /// prompts; closing the window cancels it and waits for its result.
    fn run_job<T: Send + 'static>(
        &mut self,
        job: JobHandle<T>,
        status: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
        complete: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.busy = true;
        self.error = None;
        self.logs.clear();
        self.facts = JobFacts::default();
        self.stage = None;
        self.progress = None;
        self.status = status.into();

        let events = job.events();
        self.cancellation = Some(job.cancellation_token());
        self.operation = Some(cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = events.recv().await {
                if view.update_in(cx, |view, window, cx| view.job_event(event, window, cx)).is_err() {
                    break;
                }
            }

            let result = job.result().await;

            let _ = view.update_in(cx, |view, window, cx| {
                match result {
                    Ok(value) => complete(view, value, window, cx),
                    Err(error) => view.failed(error),
                }

                view.finished(window, cx);
            });
        }));
        cx.notify();
    }

    fn job_event(&mut self, event: JobEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(event, JobEvent::Prompt(_)) {
            self.dismiss_stale_prompt(&event, window);
        }

        match event {
            JobEvent::Stage(stage) => {
                self.stage = Some(stage);
                self.progress = None;
                self.status = stage.label().into();
            }
            JobEvent::Progress { done, total } => self.progress = Some((done, total)),
            JobEvent::Log { level, message } => self.add_log(level, message),
            JobEvent::Fact(fact) => self.record_fact(fact),
            JobEvent::PromptWithdrawn { .. } => {}
            JobEvent::Prompt(prompt) => {
                let device_name = match &prompt.kind {
                    PromptKind::WaitForDevice { udid, .. } => self.device_name(udid),
                    _ => None,
                };
                let dialog = PromptDialog::new(prompt, device_name, window, cx);

                if dialog.has_input() {
                    cx.focus_view(&dialog.input, window);
                } else {
                    self.focus.focus(window);
                }

                self.dialog = Some(Dialog::Prompt(dialog));
                self.dialog_error = None;
            }
        }

        cx.notify();
    }

    /// A job blocked on a question emits nothing until it is answered, except when it can
    /// continue on its own: the engine withdraws the question then (for example when a device
    /// returns). Any later event also makes a device question stale, and a new stage ends any
    /// question.
    fn dismiss_stale_prompt(&mut self, event: &JobEvent, window: &mut Window) {
        let Some(Dialog::Prompt(dialog)) = &self.dialog else {
            return;
        };

        let waiting_for_device = matches!(dialog.prompt.kind, PromptKind::WaitForDevice { .. });

        let stale = match event {
            JobEvent::PromptWithdrawn { id } => *id == dialog.prompt.id,
            JobEvent::Stage(_) => true,
            _ => waiting_for_device,
        };

        if !stale {
            return;
        }

        self.dialog = None;
        self.dialog_error = None;
        self.focus.focus(window);

        let cancelled = self.cancellation.as_ref().is_some_and(|token| token.is_cancelled());

        if waiting_for_device && !cancelled {
            self.add_log(LogLevel::Info, "The device is available again; continuing.".into());
        }
    }

    fn record_fact(&mut self, fact: Fact) {
        match fact {
            Fact::BundleId(identifier) => {
                self.add_log(LogLevel::Info, format!("Bundle identifier: {identifier}"));
                self.facts.bundle_id = Some(identifier);
            }
            Fact::Team(team) => self.facts.team = Some(team),
            Fact::AppIdQuota { remaining, next_release } => {
                let quota = Quota { remaining, next_release };

                if let Some(apple_id) = &self.job_account {
                    self.accounts.quota.insert(apple_id.clone(), quota);
                }

                self.facts.quota = Some(quota);
            }
            Fact::ProfileExpiry { expires, ttl_days } => {
                self.facts.expires = Some(expires);
                self.facts.ttl_days = ttl_days;
            }
            Fact::AnisetteDevice(description) => self.facts.anisette_device = Some(description),
            Fact::EncryptedBinary => {
                self.add_log(LogLevel::Warn, "The executable remains encrypted after signing.".into());
                self.facts.encrypted = true;
            }
        }
    }

    fn add_log(&mut self, level: LogLevel, message: String) {
        if self.logs.len() == MAX_LOG_LINES {
            self.logs.pop_front();
        }

        let message: String = message.chars().take(4096).collect();
        self.logs.push_back((level, message));
    }

    fn failed(&mut self, error: EngineError) {
        if error == EngineError::Cancelled {
            self.status = "Cancelled".into();
        } else {
            let message = error.to_string();
            self.add_log(LogLevel::Error, message.clone());
            self.error = Some(message);
            self.status = "Operation failed".into();
        }
    }

    fn finished(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.busy = false;
        self.cancellation = None;
        self.job_account = None;
        self.close_prompt();

        // Jobs can add installations, record refresh failures and choose default teams.
        self.reload_accounts();
        self.reload_installations();

        if self.closing {
            window.remove_window();
        } else {
            self.focus.focus(window);
            window.activate_window();
        }

        cx.notify();
    }

    /// Drop an unanswered job question; the job receives a cancellation.
    fn close_prompt(&mut self) {
        if matches!(self.dialog, Some(Dialog::Prompt(_))) {
            self.dialog = None;
            self.dialog_error = None;
        }
    }

    fn cancel(&mut self, _: &CancelJob, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
            self.status = "Cancelling…".into();
            self.close_prompt();
            cx.notify();
        }
    }

    fn prepare_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.cancellation.is_some() {
            self.closing = true;
            self.cancel(&CancelJob, window, cx);
            false
        } else {
            true
        }
    }

    pub fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.prepare_close(window, cx) {
            window.remove_window();
        }
    }

    fn add_injection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() || self.mode == ExportMode::Original {
            return;
        }

        let paths = picker::paths(window, cx, "Choose libraries, frameworks, or resources", true);
        self.picking = true;
        self.picker = Some(cx.spawn_in(window, async move |view, cx| {
            let result = paths.await;

            let _ = view.update_in(cx, |view, window, cx| {
                view.picking = false;
                view.focus.focus(window);
                window.activate_window();

                match result {
                    Ok(Some(paths)) => {
                        for source in paths {
                            if !view.draft.options.injections.iter().any(|item| item.source == source) {
                                view.draft.options.injections.push(LibraryInjection { source, name: None });
                            }
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

    fn file_edit(&mut self, source: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() || self.mode == ExportMode::Original {
            return;
        }

        let target = cx.new(|cx| InputState::new(window, cx).placeholder("Path relative to the app"));

        if let Some(source) = &source {
            target.update(cx, |input, cx| {
                input.set_value(source.file_name().unwrap_or_default().to_string_lossy().into_owned(), window, cx)
            });
        }

        cx.focus_view(&target, window);
        self.dialog = Some(Dialog::File(FileDialog { target, source }));
        self.dialog_error = None;
        cx.notify();
    }

    fn choose_replacement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() || self.mode == ExportMode::Original {
            return;
        }

        let paths = picker::paths(window, cx, "Choose a replacement file or folder", false);
        self.picking = true;
        self.picker = Some(cx.spawn_in(window, async move |view, cx| {
            let result = paths.await;

            let _ = view.update_in(cx, |view, window, cx| {
                view.picking = false;
                view.focus.focus(window);
                window.activate_window();

                match result {
                    Ok(Some(paths)) => {
                        if let Some(path) = paths.into_iter().next() {
                            view.file_edit(Some(path), window, cx);
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

    /// Fill a save-path question from the platform save panel.
    fn choose_prompt_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::Prompt(dialog)) = &self.dialog else {
            return;
        };
        let PromptKind::SaveFile { suggested_name } = &dialog.prompt.kind else {
            return;
        };

        let directory = self.app.as_ref().and_then(|app| app.path.parent().map(Path::to_path_buf));
        let directory = directory.or_else(dirs_home).unwrap_or_else(|| PathBuf::from("."));
        let path = picker::destination(window, cx, &directory, suggested_name);

        self.picker = Some(cx.spawn_in(window, async move |view, cx| {
            let result = path.await;

            let _ = view.update_in(cx, |view, window, cx| {
                match (result, &view.dialog) {
                    (Ok(Some(path)), Some(Dialog::Prompt(dialog))) => {
                        let text = path.to_string_lossy().into_owned();
                        dialog.input.update(cx, |input, cx| input.set_value(text, window, cx));
                    }
                    (Err(error), _) => view.dialog_error = Some(error.to_string()),
                    _ => {}
                }

                cx.notify();
            });
        }));
    }

    fn answer(&mut self, reply: PromptReply, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Dialog::Prompt(dialog)) = self.dialog.take() {
            dialog.prompt.answer(reply);
        }

        self.dialog_error = None;
        self.focus.focus(window);
        cx.notify();
    }

    fn submit_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.dialog {
            Some(Dialog::Prompt(dialog)) => match dialog.reply(cx) {
                Ok(reply) => self.answer(reply, window, cx),
                Err(error) => {
                    self.dialog_error = Some(error);
                    cx.notify();
                }
            },
            Some(Dialog::File(dialog)) => {
                let target = PathBuf::from(dialog.target.read(cx).value().as_ref());

                if let Err(error) = validate_relative(&target) {
                    self.dialog_error = Some(error);
                    cx.notify();
                    return;
                }

                let replacement = FileReplacement { target: target.clone(), source: dialog.source.clone() };
                self.draft.options.replacements.retain(|edit| edit.target != target);
                self.draft.options.replacements.push(replacement);
                self.dialog = None;
                self.dialog_error = None;
                self.focus.focus(window);
                cx.notify();
            }
            Some(Dialog::Confirm(_)) => {
                let Some(Dialog::Confirm(confirmation)) = self.dialog.take() else {
                    return;
                };

                self.dialog_error = None;
                self.focus.focus(window);
                self.perform(confirmation.action, window, cx);
                cx.notify();
            }
            None => {}
        }
    }

    fn dismiss_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Dialog::Prompt(dialog)) = &self.dialog {
            let reply = dialog.dismissal();
            self.answer(reply, window, cx);
        } else {
            self.dialog = None;
            self.dialog_error = None;
            self.focus.focus(window);
            cx.notify();
        }
    }

    /// Destructive dialogs require an explicit click; Enter does not confirm them.
    fn dialog_is_destructive(&self) -> bool {
        match &self.dialog {
            Some(Dialog::Prompt(dialog)) => dialog.destructive(),
            Some(Dialog::Confirm(_)) => true,
            _ => false,
        }
    }

    fn dialog_key(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_none() {
            return;
        }

        match event.keystroke.key.as_str() {
            "escape" => self.dismiss_dialog(window, cx),
            "enter" if event.keystroke.modifiers == gpui::Modifiers::none() => {
                if !self.dialog_is_destructive() {
                    self.submit_dialog(window, cx);
                }
            }
            _ => return,
        }

        cx.stop_propagation();
    }

    /// Ask before a destructive action.
    pub(crate) fn confirm(&mut self, confirmation: Confirmation, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }

        self.dialog = Some(Dialog::Confirm(confirmation));
        self.dialog_error = None;
        self.focus.focus(window);
        cx.notify();
    }

    fn perform(&mut self, action: PendingAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            PendingAction::RevokeCertificate { apple_id, serial } => {
                self.revoke_certificate(apple_id, serial, window, cx)
            }
            PendingAction::UninstallApp { udid, bundle_id } => self.uninstall_app(udid, bundle_id, window, cx),
            PendingAction::RemoveProfile { udid, uuid } => self.remove_profile(udid, uuid, window, cx),
            PendingAction::ForgetInstallation { id } => self.forget_installation(id, cx),
        }
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn apply_theme(preference: ThemePreference, window: &mut Window, cx: &mut App) {
    match preference {
        ThemePreference::System => Theme::sync_system_appearance(Some(window), cx),
        ThemePreference::Light => Theme::change(ThemeMode::Light, Some(window), cx),
        ThemePreference::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
    }

    let theme = Theme::global_mut(cx);
    let dark = theme.is_dark();

    theme.background = rgb(if dark { 0x121619 } else { 0xf4f7f8 }).into();
    theme.popover = rgb(if dark { 0x181e23 } else { 0xffffff }).into();
    theme.border = rgb(if dark { 0x2a3339 } else { 0xd5dee2 }).into();
    theme.foreground = rgb(if dark { 0xedf3f5 } else { 0x17252b }).into();
    theme.muted = rgb(if dark { 0x253037 } else { 0xe9eff2 }).into();
    theme.muted_foreground = rgb(if dark { 0x9aa7af } else { 0x52656e }).into();
    theme.primary = rgb(if dark { 0x52cbb6 } else { 0x147d70 }).into();
    theme.primary_foreground = rgb(if dark { 0x10211d } else { 0xffffff }).into();
    theme.progress_bar = theme.primary;
    theme.font_size = px(16.);
}

impl Focusable for Sideport {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
