use crate::{Draft, ExportMode, draft::validate_relative, picker, prompt::PromptDialog};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, Image, ImageFormat, Task, Window, actions, prelude::*, px, rgb,
};
use gpui_component::{Theme, ThemeMode, input::InputState};
use sl_engine::{
    AppSummary, Engine, EngineError, Fact, FileReplacement, JobEvent, JobOutcome, LibraryInjection, LogLevel,
    PromptReply, Stage, ThemePreference,
};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

actions!(sideport, [OpenApp, ExportApp, CancelJob]);

mod view;

const MAX_LOG_LINES: usize = 400;

#[cfg(test)]
mod tests;

struct Fields {
    name: Entity<InputState>,
    identifier: Entity<InputState>,
    version: Entity<InputState>,
    short_version: Entity<InputState>,
    minimum_os: Entity<InputState>,
    overrides: Entity<InputState>,
}

impl Fields {
    fn new(window: &mut Window, cx: &mut App) -> Self {
        let mut input = |placeholder: &'static str| cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let name = input("App name");
        let identifier = input("com.example.app");
        let version = input("Build number");
        let short_version = input("Release version");
        let minimum_os = input("Minimum OS version");
        let overrides = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("{\"CustomKey\": \"value\", \"RemoveThisKey\": null}")
        });

        Self { name, identifier, version, short_version, minimum_os, overrides }
    }

    fn load(&self, draft: &Draft, window: &mut Window, cx: &mut App) {
        for (field, value) in [
            (&self.name, &draft.name),
            (&self.identifier, &draft.identifier),
            (&self.version, &draft.version),
            (&self.short_version, &draft.short_version),
            (&self.minimum_os, &draft.minimum_os),
            (&self.overrides, &draft.extra_info),
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
    }
}

struct FileDialog {
    target: Entity<InputState>,
    source: Option<PathBuf>,
}

enum Dialog {
    Prompt(PromptDialog),
    File(FileDialog),
}

/// The desktop root owns task handles so closing it cancels outstanding work.
pub struct Sideport {
    engine: Engine,
    focus: FocusHandle,
    app: Option<AppSummary>,
    icon: Option<Arc<Image>>,
    draft: Draft,
    fields: Fields,
    mode: ExportMode,
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
    logs: VecDeque<(LogLevel, String)>,
    settings_open: bool,
    advanced_open: bool,
    logs_open: bool,
}

impl std::fmt::Debug for Sideport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Sideport")
            .field("has_app", &self.app.is_some())
            .field("busy", &self.busy)
            .field("stage", &self.stage)
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
        let focus = cx.focus_handle();
        let fields = Fields::new(window, cx);
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
        focus.focus(window);

        Self {
            engine,
            focus,
            app: None,
            icon: None,
            draft: Draft::default(),
            fields,
            mode: ExportMode::default(),
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
            logs: VecDeque::new(),
            settings_open: false,
            advanced_open: false,
            logs_open: false,
        }
    }

    pub fn load_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.picking || self.dialog.is_some() {
            return;
        }

        self.app = None;
        self.icon = None;
        self.busy = true;
        self.error = None;
        self.outcome = None;
        self.status = "Reading app…".into();
        self.stage = None;
        self.progress = None;
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
                        view.draft = Draft::for_app(&app);
                        view.fields.load(&view.draft, window, cx);
                        view.status = "Ready to export".into();

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

    fn open_app(&mut self, _: &OpenApp, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.picking || self.dialog.is_some() {
            return;
        }

        let paths = picker::paths(window, cx, "Choose an IPA, app ZIP, or .app", false);
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

    fn export(&mut self, _: &ExportApp, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.picking || self.dialog.is_some() {
            return;
        }
        let Some(app) = &self.app else {
            return;
        };

        self.fields.store(&mut self.draft, cx);
        let spec = match self.draft.job(app, self.mode, None) {
            Ok(spec) => spec,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let suggested = format!(
            "{} {}.ipa",
            app.path.file_stem().unwrap_or_default().to_string_lossy(),
            if self.mode == ExportMode::Original { "Original" } else { "Prepared" }
        );
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
                        spec.target = sl_engine::Target::ExportIpa { path: Some(path) };
                        view.start_job(spec, window, cx);
                    }
                    Err(error) => {
                        view.error = Some(error.to_string());
                        view.status = "Export did not start".into();
                    }
                    _ => {
                        view.status = "Ready to export".into();
                    }
                }

                cx.notify();
            });
        }));
        cx.notify();
    }

    fn start_job(&mut self, spec: sl_engine::JobSpec, window: &mut Window, cx: &mut Context<Self>) {
        self.busy = true;
        self.outcome = None;
        self.logs.clear();
        self.stage = Some(Stage::Preparing);
        self.progress = None;
        self.status = "Preparing…".into();

        let job = self.engine.start(spec);
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
                    Ok(outcome) => {
                        view.status = "Export complete".into();
                        view.outcome = Some(outcome);
                    }
                    Err(error) => view.failed(error),
                }

                view.finished(window, cx);
            });
        }));
    }

    fn job_event(&mut self, event: JobEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            JobEvent::Stage(stage) => {
                self.stage = Some(stage);
                self.progress = None;
                self.status = stage.label().into();
                self.dialog = None;
            }
            JobEvent::Progress { done, total } => self.progress = Some((done, total)),
            JobEvent::Log { level, message } => self.add_log(level, message),
            JobEvent::Fact(Fact::BundleId(identifier)) => {
                self.add_log(LogLevel::Info, format!("Bundle identifier: {identifier}"))
            }
            JobEvent::Fact(Fact::EncryptedBinary) => {
                self.add_log(LogLevel::Warn, "The executable remains encrypted after signing.".into())
            }
            JobEvent::Fact(_) => {}
            JobEvent::Prompt(prompt) => {
                let dialog = PromptDialog::new(prompt, window, cx);
                cx.focus_view(&dialog.input, window);
                self.dialog = Some(Dialog::Prompt(dialog));
                self.dialog_error = None;
            }
        }

        cx.notify();
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
        self.dialog = None;
        self.dialog_error = None;

        if self.closing {
            window.remove_window();
        } else {
            self.focus.focus(window);
            window.activate_window();
        }

        cx.notify();
    }

    fn cancel(&mut self, _: &CancelJob, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
            self.status = "Cancelling…".into();
            self.dialog = None;
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
        if self.busy || self.picking || self.dialog.is_some() || self.mode == ExportMode::Original {
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
        if self.busy || self.picking || self.dialog.is_some() || self.mode == ExportMode::Original {
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
        if self.busy || self.picking || self.dialog.is_some() || self.mode == ExportMode::Original {
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
            None => {}
        }
    }

    fn dismiss_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.dialog, Some(Dialog::Prompt(_))) {
            self.answer(PromptReply::Cancel, window, cx);
        } else {
            self.dialog = None;
            self.dialog_error = None;
            self.focus.focus(window);
            cx.notify();
        }
    }

    fn dialog_key(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_none() {
            return;
        }

        match event.keystroke.key.as_str() {
            "escape" => self.dismiss_dialog(window, cx),
            "enter" if event.keystroke.modifiers == gpui::Modifiers::none() => self.submit_dialog(window, cx),
            _ => return,
        }

        cx.stop_propagation();
    }

    fn theme(&mut self, preference: ThemePreference, window: &mut Window, cx: &mut Context<Self>) {
        let mut settings = self.engine.settings();
        settings.theme = preference;

        match self.engine.update_settings(settings) {
            Ok(()) => apply_theme(preference, window, cx),
            Err(error) => self.error = Some(error.to_string()),
        }

        cx.notify();
    }
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
