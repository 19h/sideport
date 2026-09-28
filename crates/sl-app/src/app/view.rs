use super::{
    CancelJob, Dialog, ExportApp, OpenApp, Section, ShowAccounts, ShowApp, ShowDevices, ShowInstallations,
    ShowSettings, Sideport,
    widgets::{field, format_bytes, format_time, icon_placeholder, section_title},
};
use crate::{ExportMode, IdentifierPolicy, prompt::team_kind};
use gpui::{
    AnyElement, App, Context, Div, ExternalPaths, FontWeight, IntoElement, Render, Window, div, img, prelude::*, px,
    rgba,
};
use gpui_component::{
    ActiveTheme, Disableable, Selectable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::Input,
    progress::Progress,
};
use sl_engine::{ExtensionRemoval, LogLevel, PromptKind, PromptReply, Stage};

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
        let disabled = self.occupied() || self.mode == ExportMode::Original;
        let identifier_locked =
            disabled || (self.mode == ExportMode::AppleId && self.draft.identifier_policy != IdentifierPolicy::Custom);
        let Some(app) = &self.app else {
            return div();
        };

        let icon = match self.icon.clone() {
            Some(icon) => img(icon).size(px(80.)).rounded_2xl().into_any_element(),
            None => icon_placeholder(&app.name, 80., cx).into_any_element(),
        };
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
                    .disabled(self.occupied())
                    .on_click(cx.listener(|view, _, window, cx| view.open_app(&OpenApp, window, cx))),
            );

        let metadata = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(section_title("App details"))
            .child(div().flex().gap_4().child(field("Display name", &self.fields.name, disabled)).child(field(
                "Bundle identifier",
                &self.fields.identifier,
                identifier_locked,
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
            .child(self.render_signing(cx))
            .child(self.render_destination(cx))
            .child(card().child(metadata).child(toggles))
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
            .max_w(px(1100.))
            .mx_auto()
            .mt_5()
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

    /// Team, identifier, quota and profile validity reported by the job.
    fn render_facts(&self, cx: &App) -> Option<Div> {
        let facts = &self.facts;
        let chip = |selector: &'static str, text: String| {
            div()
                .debug_selector(move || format!("fact:{selector}"))
                .px_2()
                .py_0p5()
                .rounded_md()
                .bg(cx.theme().muted)
                .text_xs()
                .child(text)
        };

        let team = facts
            .team
            .as_ref()
            .map(|team| chip("team", format!("Team {} ({}, {})", team.name, team.team_id, team_kind(&team.kind))));
        let bundle_id = facts.bundle_id.as_ref().map(|identifier| chip("bundle-id", format!("Bundle ID {identifier}")));
        let quota = facts.quota.map(|quota| {
            let release = quota.next_release.map(|time| format!(", next frees {}", format_time(time)));

            chip("quota", format!("{} free App IDs left{}", quota.remaining, release.unwrap_or_default()))
        });
        let expires = facts.expires.map(|expires| {
            let days = facts.ttl_days.map(|days| format!(" ({days} days)")).unwrap_or_default();

            chip("expiry", format!("Profile expires {}{days}", format_time(expires)))
        });
        let anisette = facts.anisette_device.as_ref().map(|device| chip("anisette", format!("Apple sees {device}")));
        let encrypted =
            facts.encrypted.then(|| chip("encrypted", "Encrypted executable: the app will not launch".into()));

        let chips: Vec<_> = [team, bundle_id, quota, expires, anisette, encrypted].into_iter().flatten().collect();

        if chips.is_empty() {
            return None;
        }

        Some(div().flex().flex_wrap().gap_2().children(chips))
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let progress = self.progress.filter(|(_, total)| *total != 0);
        let editing = self.section == Section::App && self.app.is_some() && !self.busy;
        let blocker = if editing { self.action_blocker() } else { None };

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
            })
            .children(self.render_facts(cx))
            .when_some(blocker.clone(), |this, blocker| {
                this.child(
                    div()
                        .debug_selector(|| "action-guidance".into())
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(blocker),
                )
            });

        let action = if self.cancellation.is_some() {
            Some(
                Button::new("cancel-job")
                    .outline()
                    .label("Cancel")
                    .debug_selector(|| "cancel-job".into())
                    .on_click(cx.listener(|view, _, window, cx| view.cancel(&CancelJob, window, cx))),
            )
        } else if self.section == Section::App {
            Some(
                Button::new("primary-action")
                    .primary()
                    .label(self.primary_label())
                    .disabled(self.app.is_none() || self.occupied() || blocker.is_some())
                    .debug_selector(|| "primary-action".into())
                    .on_click(cx.listener(|view, _, window, cx| view.primary_action(&ExportApp, window, cx))),
            )
        } else {
            None
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
            .children(action);

        div()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "error".into())
                        .px_8()
                        .pt_3()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .child(div().px_8().py_5().flex().items_center().gap_5().child(status).child(controls))
    }

    fn render_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().gap_1().children(Section::ALL.into_iter().enumerate().map(|(index, section)| {
            Button::new(("nav", index))
                .ghost()
                .label(section.label())
                .selected(self.section == section)
                .debug_selector(move || format!("nav:{}", section.label()))
                .on_click(cx.listener(move |view, _, window, cx| view.show_section(section, window, cx)))
        }))
    }

    fn render_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div().flex().flex_col().gap_4();
        let mut primary = Some("Continue".to_string());
        let mut destructive = false;

        match &self.dialog {
            Some(Dialog::File(dialog)) => {
                let label = if dialog.source.is_some() { "Add replacement" } else { "Add deletion" };

                primary = Some(label.into());
                body = body
                    .child(section_title(label))
                    .child("Enter the target path inside the app.")
                    .child(Input::new(&dialog.target))
                    .when_some(dialog.source.clone(), |this, source| {
                        this.child(div().text_xs().child(format!("Source: {}", source.display())))
                    });
            }
            Some(Dialog::Confirm(confirmation)) => {
                primary = Some(confirmation.confirm_label.into());
                destructive = true;
                body = body.child(section_title(&confirmation.title)).child(confirmation.message.clone());
            }
            Some(Dialog::Prompt(dialog)) => {
                primary = dialog.primary_label();
                destructive = dialog.destructive();
                body = body.child(section_title(dialog.title())).child(dialog.message());

                match &dialog.prompt.kind {
                    PromptKind::Password { .. } => {
                        let input = Input::new(&dialog.input);
                        let input = if dialog.masked { input.mask_toggle() } else { input };

                        body = body.child(input).child(
                            Checkbox::new("remember-prompt-password")
                                .label("Remember password")
                                .checked(dialog.remember)
                                .debug_selector(|| "prompt-remember".into())
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
                            this.child(
                                Button::new("request-sms")
                                    .outline()
                                    .label("Text me a code")
                                    .debug_selector(|| "prompt-request-sms".into())
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.answer(PromptReply::RequestSms, window, cx)
                                    })),
                            )
                        });
                    }
                    PromptKind::SaveFile { .. } => {
                        body = body.child(
                            div().flex().gap_2().child(div().flex_1().child(Input::new(&dialog.input))).child(
                                Button::new("prompt-choose-path")
                                    .outline()
                                    .label("Choose…")
                                    .on_click(cx.listener(|view, _, window, cx| view.choose_prompt_path(window, cx))),
                            ),
                        );
                    }
                    PromptKind::ChooseTeam { teams, .. } => {
                        body = body.children(teams.iter().enumerate().map(|(index, choice)| {
                            let team = &choice.team;
                            let label = format!("{} ({}) · {}", team.name, team.team_id, team_kind(&team.kind));

                            Button::new(("prompt-team", index))
                                .outline()
                                .label(label)
                                .debug_selector(move || format!("prompt-team:{index}"))
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.answer(PromptReply::Choice(index), window, cx)
                                }))
                        }));
                    }
                    PromptKind::Confirm { .. } => {}
                    PromptKind::WaitForDevice { .. } => {
                        body = body.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("Sideport continues on its own as soon as the device is connected again."),
                        );
                    }
                }
            }
            None => {}
        }

        let submit = primary.map(|label| {
            let selector = if destructive { "dialog-submit-danger" } else { "dialog-submit" };
            let button = Button::new("submit-dialog").label(label).debug_selector(move || selector.into());
            let styled = if destructive { button.danger() } else { button.primary() };

            styled.on_click(cx.listener(|view, _, window, cx| view.submit_dialog(window, cx)))
        });

        div().absolute().inset_0().bg(rgba(0x00000070)).flex().items_center().justify_center().p_8().occlude().child(
            div()
                .id("dialog")
                .w(px(480.))
                .max_w_full()
                .max_h_full()
                .overflow_y_scroll()
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
                                .debug_selector(|| "dialog-cancel".into())
                                .on_click(cx.listener(|view, _, window, cx| view.dismiss_dialog(window, cx))),
                        )
                        .children(submit),
                ),
        )
    }

    fn render_section(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.section {
            Section::App if self.app.is_some() => self.render_editor(window, cx).into_any_element(),
            Section::App => self.render_empty(cx).into_any_element(),
            Section::Accounts => self.render_accounts(cx),
            Section::Devices => self.render_devices(cx),
            Section::Installations => self.render_installations(cx),
            Section::Settings => self.render_settings(cx),
        }
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

        let brand = div()
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
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Sideport"));

        let header = div()
            .h(px(76.))
            .px_8()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .border_b_1()
            .border_color(border)
            .child(brand)
            .child(self.render_navigation(cx));

        let body = div()
            .id("body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_8()
            .child(self.render_section(window, cx))
            .when(self.logs_open && !self.logs.is_empty(), |this| this.child(self.render_logs(cx)));

        div()
            .id("sideport")
            .key_context("Sideport")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::open_app))
            .on_action(cx.listener(Self::primary_action))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(|view, _: &ShowApp, window, cx| view.show_section(Section::App, window, cx)))
            .on_action(
                cx.listener(|view, _: &ShowAccounts, window, cx| view.show_section(Section::Accounts, window, cx)),
            )
            .on_action(cx.listener(|view, _: &ShowDevices, window, cx| view.show_section(Section::Devices, window, cx)))
            .on_action(cx.listener(|view, _: &ShowInstallations, window, cx| {
                view.show_section(Section::Installations, window, cx)
            }))
            .on_action(
                cx.listener(|view, _: &ShowSettings, window, cx| view.show_section(Section::Settings, window, cx)),
            )
            .capture_key_down(cx.listener(Self::dialog_key))
            .on_drop(cx.listener(|view, paths: &ExternalPaths, window, cx| {
                if paths.paths().len() == 1 {
                    view.section = Section::App;
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
            .child(header)
            .child(body)
            .child(self.render_footer(cx))
            .when_some(self.dialog.as_ref(), |element, _| element.child(self.render_dialog(cx)))
    }
}
