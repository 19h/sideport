use super::{CancelJob, Dialog, ExportApp, OpenApp, Sideport};
use crate::ExportMode;
use gpui::{
    App, Context, Div, Entity, ExternalPaths, FontWeight, IntoElement, Render, Window, div, img, prelude::*, px, rgba,
};
use gpui_component::{
    ActiveTheme, Disableable, Selectable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputState},
    progress::Progress,
};
use sl_engine::{ExtensionRemoval, LogLevel, PromptKind, PromptReply, Stage, ThemePreference};

impl Sideport {
    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shortcut = if cfg!(target_os = "macos") { "⌘O" } else { "Ctrl+O" };

        div()
            .w_full()
            .min_h(px(450.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_5()
            .child(
                div()
                    .size(px(96.))
                    .rounded_2xl()
                    .bg(cx.theme().muted)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_3xl()
                    .text_color(cx.theme().primary)
                    .child("↓"),
            )
            .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("Start with your app"))
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("Drop an IPA, app ZIP, or .app here to inspect it."),
            )
            .child(
                Button::new("choose-app")
                    .primary()
                    .label("Choose app…")
                    .disabled(self.busy || self.picking)
                    .on_click(cx.listener(|view, _, window, cx| view.open_app(&OpenApp, window, cx))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{shortcut} to open • Your original input stays unchanged")),
            )
    }

    fn render_editor(&self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let disabled = self.busy || self.picking || self.dialog.is_some() || self.mode == ExportMode::Original;
        let Some(app) = &self.app else {
            return div();
        };

        let icon =
            self.icon.clone().map(|icon| img(icon).size(px(80.)).rounded_2xl().into_any_element()).unwrap_or_else(
                || {
                    div()
                        .size(px(80.))
                        .rounded_2xl()
                        .bg(cx.theme().muted)
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_3xl()
                        .child("A")
                        .into_any_element()
                },
            );
        let border = cx.theme().border;
        let background = cx.theme().popover;
        let card = || {
            div().w_full().flex().flex_col().gap_6().rounded_xl().border_1().border_color(border).bg(background).p_6()
        };

        let header = div()
            .flex()
            .items_center()
            .gap_5()
            .child(icon)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(app.name.clone()))
                    .child(div().text_color(cx.theme().muted_foreground).child(app.bundle_id.clone()))
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!(
                        "{} • {} • {}",
                        app.short_version.as_deref().unwrap_or("Version unknown"),
                        format_bytes(app.file_size),
                        app.path.file_name().unwrap_or_default().to_string_lossy()
                    ))),
            )
            .child(
                Button::new("change-app")
                    .outline()
                    .label("Change…")
                    .disabled(self.busy || self.picking || self.dialog.is_some())
                    .on_click(cx.listener(|view, _, window, cx| view.open_app(&OpenApp, window, cx))),
            );

        let mode = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title("Export format"))
            .child(div().flex().gap_2().children(
                [ExportMode::Unsigned, ExportMode::AdHoc, ExportMode::Original].into_iter().enumerate().map(
                    |(index, mode)| {
                        Button::new(("mode", index))
                            .label(mode.label())
                            .outline()
                            .selected(self.mode == mode)
                            .debug_selector(move || format!("mode:{}", mode.label()))
                            .disabled(self.busy || self.picking || self.dialog.is_some())
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.mode = mode;
                                view.error = None;
                                view.focus.focus(window);
                                cx.notify();
                            }))
                    },
                ),
            ))
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(self.mode.description()));

        let metadata = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(section_title("App details"))
            .child(div().flex().gap_4().child(field("Display name", &self.fields.name, disabled)).child(field(
                "Bundle identifier",
                &self.fields.identifier,
                disabled,
            )))
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(field("Release version", &self.fields.short_version, disabled))
                    .child(field("Build number", &self.fields.version, disabled))
                    .child(field("Minimum OS", &self.fields.minimum_os, disabled)),
            );

        let toggles = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                Checkbox::new("file-sharing")
                    .label("Enable file sharing")
                    .checked(self.draft.options.enable_file_sharing)
                    .disabled(disabled)
                    .on_click(cx.listener(|view, checked, _, cx| {
                        view.draft.options.enable_file_sharing = *checked;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("device-restrictions")
                    .label("Remove device model restrictions")
                    .checked(self.draft.options.remove_device_restrictions)
                    .disabled(disabled)
                    .on_click(cx.listener(|view, checked, _, cx| {
                        view.draft.options.remove_device_restrictions = *checked;
                        cx.notify();
                    })),
            )
            .when(app.has_watch_app, |this| {
                this.child(
                    Checkbox::new("watch-app")
                        .label("Remove Watch apps")
                        .checked(self.draft.options.remove_watch_app)
                        .disabled(disabled)
                        .on_click(cx.listener(|view, checked, _, cx| {
                            view.draft.options.remove_watch_app = *checked;
                            cx.notify();
                        })),
                )
            });

        div()
            .max_w(px(1100.))
            .mx_auto()
            .flex()
            .flex_col()
            .gap_5()
            .child(card().child(header).when(app.encrypted, |this| {
                this.child(
                    div()
                        .rounded_md()
                        .bg(cx.theme().muted)
                        .p_3()
                        .text_sm()
                        .child("Encrypted executable. Re-signing preserves its encryption."),
                )
            }))
            .child(card().child(mode).child(metadata).child(toggles))
            .when(!app.extensions.is_empty(), |this| this.child(card().child(self.render_extensions(cx, disabled))))
            .child(card().child(self.render_injections(cx, disabled)))
            .child(
                card()
                    .child(
                        Button::new("advanced-toggle")
                            .ghost()
                            .label(if self.advanced_open { "Hide advanced edits" } else { "Advanced edits" })
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.advanced_open = !view.advanced_open;
                                cx.notify();
                            })),
                    )
                    .when(self.advanced_open, |this| this.child(self.render_advanced(cx, disabled))),
            )
            .when(self.logs_open, |this| this.child(self.render_logs(cx)))
    }

    fn render_extensions(&self, cx: &mut Context<Self>, disabled: bool) -> impl IntoElement {
        let extensions = self.app.as_ref().map(|app| app.extensions.as_slice()).unwrap_or_default();
        let remove_all = self.draft.options.remove_extensions == ExtensionRemoval::All;

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title("App extensions"))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Select extensions to remove from this export."),
            )
            .child(
                Checkbox::new("all-extensions")
                    .label("Remove all extensions")
                    .checked(remove_all)
                    .disabled(disabled)
                    .on_click(cx.listener(|view, checked, _, cx| {
                        view.draft.options.remove_extensions =
                            if *checked { ExtensionRemoval::All } else { ExtensionRemoval::Keep };
                        cx.notify();
                    })),
            )
            .children(extensions.iter().enumerate().map(|(index, extension)| {
                let name = extension.file_name.clone();
                let checked = remove_all
                    || match &self.draft.options.remove_extensions {
                        ExtensionRemoval::Selected(names) => names.contains(&name),
                        _ => false,
                    };

                Checkbox::new(("extension", index))
                    .label(format!("{} ({})", extension.display_name.as_deref().unwrap_or(&name), extension.bundle_id))
                    .checked(checked)
                    .disabled(disabled || remove_all)
                    .on_click(cx.listener(move |view, checked, _, cx| {
                        let mut names = match &view.draft.options.remove_extensions {
                            ExtensionRemoval::Selected(names) => names.clone(),
                            _ => Vec::new(),
                        };
                        names.retain(|item| item != &name);
                        if *checked {
                            names.push(name.clone());
                        }
                        view.draft.options.remove_extensions =
                            if names.is_empty() { ExtensionRemoval::Keep } else { ExtensionRemoval::Selected(names) };
                        cx.notify();
                    }))
            }))
    }

    fn render_injections(&self, cx: &mut Context<Self>, disabled: bool) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div().flex().justify_between().items_center().child(section_title("Libraries & resources")).child(
                    Button::new("add-injection")
                        .outline()
                        .label("Add…")
                        .disabled(disabled)
                        .on_click(cx.listener(|view, _, window, cx| view.add_injection(window, cx))),
                ),
            )
            .when(self.draft.options.injections.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Add a dylib, framework, or resource to include in the app."),
                )
            })
            .children(self.draft.options.injections.iter().enumerate().map(|(index, injection)| {
                let path = injection.source.clone();

                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().min_w_0().text_sm().child(path.display().to_string()))
                    .child(
                        Button::new(("remove-injection", index)).ghost().label("Remove").disabled(disabled).on_click(
                            cx.listener(move |view, _, _, cx| {
                                view.draft.options.injections.retain(|injection| injection.source != path);
                                cx.notify();
                            }),
                        ),
                    )
            }))
    }

    fn render_advanced(&self, cx: &mut Context<Self>, disabled: bool) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(section_title("Info.plist overrides"))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("JSON object. Values may be text, booleans, integers, or null to remove a key."),
            )
            .child(Input::new(&self.fields.overrides).disabled(disabled).h(px(130.)))
            .child(
                div().flex().items_center().justify_between().child(section_title("File edits")).child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new("replace-file")
                                .outline()
                                .label("Replace…")
                                .disabled(disabled)
                                .on_click(cx.listener(|view, _, window, cx| view.choose_replacement(window, cx))),
                        )
                        .child(
                            Button::new("delete-file")
                                .outline()
                                .label("Delete…")
                                .disabled(disabled)
                                .on_click(cx.listener(|view, _, window, cx| view.file_edit(None, window, cx))),
                        ),
                ),
            )
            .children(self.draft.options.replacements.iter().enumerate().map(|(index, replacement)| {
                let target = replacement.target.clone();
                let description = match &replacement.source {
                    Some(source) => format!("{} ← {}", target.display(), source.display()),
                    None => format!("Delete {}", target.display()),
                };

                div().flex().items_center().gap_3().child(div().flex_1().min_w_0().text_sm().child(description)).child(
                    Button::new(("remove-file-edit", index)).ghost().label("Remove").disabled(disabled).on_click(
                        cx.listener(move |view, _, _, cx| {
                            view.draft.options.replacements.retain(|edit| edit.target != target);
                            cx.notify();
                        }),
                    ),
                )
            }))
    }

    fn render_logs(&self, cx: &App) -> impl IntoElement {
        div()
            .id("logs")
            .max_h(px(240.))
            .overflow_y_scroll()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .p_4()
            .child(section_title("Activity"))
            .children(self.logs.iter().rev().map(|(level, message)| {
                div()
                    .text_xs()
                    .py_1()
                    .text_color(if *level >= LogLevel::Warn { cx.theme().danger } else { cx.theme().muted_foreground })
                    .child(message.clone())
            }))
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let progress = self.progress.filter(|(_, total)| *total != 0);
        let status = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().flex().flex_wrap().items_center().gap_3().child(self.status.clone()).when_some(
                progress,
                |this, (done, total)| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(progress_label(self.stage, done, total)),
                    )
                },
            ))
            .when_some(progress, |this, (done, total)| {
                this.child(Progress::new().value((done as f64 / total as f64 * 100.) as f32))
            });
        let action = if self.cancellation.is_some() {
            Button::new("cancel-job")
                .outline()
                .label("Cancel")
                .on_click(cx.listener(|view, _, window, cx| view.cancel(&CancelJob, window, cx)))
        } else {
            Button::new("export-app")
                .primary()
                .label(if self.mode == ExportMode::Original { "Export original…" } else { "Export IPA…" })
                .disabled(self.app.is_none() || self.busy || self.picking || self.dialog.is_some())
                .on_click(cx.listener(|view, _, window, cx| view.export(&ExportApp, window, cx)))
        };
        let controls = div()
            .flex()
            .items_center()
            .gap_3()
            .when(!self.logs.is_empty(), |this| {
                this.child(
                    Button::new("show-logs")
                        .ghost()
                        .label(if self.logs_open { "Hide activity" } else { "Activity" })
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.logs_open = !view.logs_open;
                            cx.notify();
                        })),
                )
            })
            .when_some(self.outcome.as_ref().and_then(|outcome| outcome.exported_to.clone()), |this, path| {
                this.child(
                    Button::new("reveal-export")
                        .outline()
                        .label("Show file")
                        .on_click(move |_, _, cx| cx.reveal_path(&path)),
                )
            })
            .child(action);

        div()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .when_some(self.error.clone(), |this, error| {
                this.child(div().px_8().pt_3().text_sm().text_color(cx.theme().danger).child(error))
            })
            .child(div().px_8().py_5().flex().items_center().gap_5().child(status).child(controls))
    }

    fn render_settings(&self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.engine.settings();

        div()
            .max_w(px(720.))
            .mx_auto()
            .flex()
            .flex_col()
            .gap_6()
            .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("Settings"))
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
                            .selected(settings.theme == preference)
                            .on_click(cx.listener(move |view, _, window, cx| view.theme(preference, window, cx)))
                    }),
                ),
            )
            .child(div().text_color(cx.theme().muted_foreground).child("Appearance is saved automatically."))
            .child(Button::new("back-to-app").outline().label("Back to app").on_click(cx.listener(
                |view, _, window, cx| {
                    view.settings_open = false;
                    view.focus.focus(window);
                    cx.notify();
                },
            )))
    }

    fn render_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div().flex().flex_col().gap_4();
        let mut primary = "Continue".to_string();

        match &self.dialog {
            Some(Dialog::File(dialog)) => {
                primary = if dialog.source.is_some() { "Add replacement" } else { "Add deletion" }.into();
                body = body
                    .child(section_title(&primary))
                    .child("Enter the target path inside the app.")
                    .child(Input::new(&dialog.target))
                    .when_some(dialog.source.clone(), |this, source| {
                        this.child(div().text_xs().child(format!("Source: {}", source.display())))
                    });
            }
            Some(Dialog::Prompt(dialog)) => {
                body = body.child(section_title(dialog.title())).child(dialog.message());

                match &dialog.prompt.kind {
                    PromptKind::Password { .. } => {
                        body = body.child(Input::new(&dialog.input)).child(
                            Checkbox::new("remember-prompt-password")
                                .label("Remember password")
                                .checked(dialog.remember)
                                .on_click(cx.listener(|view, checked, _, cx| {
                                    if let Some(Dialog::Prompt(dialog)) = &mut view.dialog {
                                        dialog.remember = *checked;
                                    }
                                    cx.notify();
                                })),
                        );
                    }
                    PromptKind::SecondFactor { can_request_sms, .. } => {
                        body = body.child(Input::new(&dialog.input)).when(*can_request_sms, |this| {
                            this.child(Button::new("request-sms").outline().label("Send an SMS code").on_click(
                                cx.listener(|view, _, window, cx| view.answer(PromptReply::RequestSms, window, cx)),
                            ))
                        });
                    }
                    PromptKind::SaveFile { .. } => {
                        primary = "Save".into();
                        body = body.child(Input::new(&dialog.input));
                    }
                    PromptKind::ChooseTeam { teams, .. } => {
                        body = body.children(teams.iter().enumerate().map(|(index, team)| {
                            Button::new(("prompt-team", index))
                                .outline()
                                .label(format!("{} ({})", team.team.name, team.team.team_id))
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.answer(PromptReply::Choice(index), window, cx)
                                }))
                        }));
                        primary.clear();
                    }
                    PromptKind::Confirm { confirm_label, .. } => primary = confirm_label.clone(),
                    PromptKind::WaitForDevice { .. } => primary = "Retry".into(),
                }
            }
            None => {}
        }

        div().absolute().inset_0().bg(rgba(0x00000070)).flex().items_center().justify_center().p_8().occlude().child(
            div()
                .w(px(480.))
                .max_w_full()
                .rounded_xl()
                .border_1()
                .border_color(cx.theme().border)
                .shadow_lg()
                .bg(cx.theme().popover)
                .p_6()
                .child(body)
                .when_some(self.dialog_error.clone(), |this, error| {
                    this.child(div().pt_3().text_color(cx.theme().danger).child(error))
                })
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .pt_5()
                        .child(
                            Button::new("cancel-dialog")
                                .outline()
                                .label("Cancel")
                                .on_click(cx.listener(|view, _, window, cx| view.dismiss_dialog(window, cx))),
                        )
                        .when(!primary.is_empty(), |this| {
                            this.child(
                                Button::new("submit-dialog")
                                    .primary()
                                    .label(primary)
                                    .on_click(cx.listener(|view, _, window, cx| view.submit_dialog(window, cx))),
                            )
                        }),
                ),
        )
    }
}

fn section_title(title: &str) -> Div {
    div().font_weight(FontWeight::SEMIBOLD).child(title.to_owned())
}

fn field(label: &str, state: &Entity<InputState>, disabled: bool) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(div().text_sm().child(label.to_owned()))
        .child(Input::new(state).disabled(disabled))
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024. * 1024.))
    } else {
        format!("{:.2} GiB", bytes as f64 / (1024. * 1024. * 1024.))
    }
}

fn progress_label(stage: Option<Stage>, done: u64, total: u64) -> String {
    match stage {
        Some(Stage::Preparing | Stage::Packaging | Stage::Uploading) => {
            format!("{} / {}", format_bytes(done), format_bytes(total))
        }
        Some(Stage::Signing | Stage::Patching) => format!("{done} / {total} items"),
        _ => format!("{done} / {total}"),
    }
}

impl Render for Sideport {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;
        let border = cx.theme().border;

        div()
            .id("sideport")
            .key_context("Sideport")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::open_app))
            .on_action(cx.listener(Self::export))
            .on_action(cx.listener(Self::cancel))
            .capture_key_down(cx.listener(Self::dialog_key))
            .on_drop(cx.listener(|view, paths: &ExternalPaths, window, cx| {
                if paths.paths().len() == 1 {
                    view.load_path(paths.paths()[0].clone(), window, cx);
                } else {
                    view.error = Some("Drop one app at a time.".into());
                    cx.notify();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(background)
            .text_color(foreground)
            .text_sm()
            .child(
                div()
                    .h(px(76.))
                    .px_8()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .size_10()
                                    .rounded_lg()
                                    .bg(cx.theme().primary)
                                    .text_color(cx.theme().primary_foreground)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_lg()
                                    .font_weight(FontWeight::BOLD)
                                    .child("S"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Sideport"))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Prepare and export apps"),
                                    ),
                            ),
                    )
                    .child(Button::new("settings").label("Settings").ghost().on_click(cx.listener(
                        |view, _, window, cx| {
                            view.settings_open = !view.settings_open;
                            view.focus.focus(window);
                            cx.notify();
                        },
                    ))),
            )
            .child(div().id("body").flex_1().min_h_0().overflow_y_scroll().p_8().child(if self.settings_open {
                self.render_settings(window, cx).into_any_element()
            } else if self.app.is_some() {
                self.render_editor(window, cx).into_any_element()
            } else {
                self.render_empty(cx).into_any_element()
            }))
            .child(self.render_footer(cx))
            .when_some(self.dialog.as_ref(), |element, _| element.child(self.render_dialog(cx)))
    }
}
