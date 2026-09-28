//! The editor's signing and destination choices, and the guidance that gates its action.

use super::{
    Destination, Section, Sideport,
    devices::device_title,
    widgets::{card, field, muted, section_title},
};
use crate::{ExportMode, IdentifierPolicy, picker};
use gpui::{AnyElement, Context, IntoElement, Window, div, prelude::*};
use gpui_component::{
    Disableable, Selectable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
};

impl Sideport {
    /// Why the editor's primary action cannot run now, phrased as the next step to take.
    pub(crate) fn action_blocker(&self) -> Option<String> {
        if self.mode == ExportMode::AppleId {
            if self.accounts.list.is_empty() {
                return Some("Sign in with an Apple ID under Accounts to use Apple ID signing.".into());
            }
            if self.draft.apple_id.is_none() {
                return Some("Choose the Apple ID account to sign with.".into());
            }
        }

        if self.destination == Destination::Export {
            return None;
        }

        if self.mode == ExportMode::Unsigned {
            return Some("Unsigned apps can only be exported. Choose another signing mode to install.".into());
        }

        match self.selected_device() {
            None => Some("Connect a device and choose it under Destination.".into()),
            Some(device) if !device.paired => Some(format!("Pair {} before installing.", device_title(device))),
            Some(_) => None,
        }
    }

    pub(crate) fn primary_label(&self) -> &'static str {
        match (self.destination, self.mode) {
            (Destination::Device, _) => "Install",
            (Destination::Export, ExportMode::Original) => "Export original…",
            (Destination::Export, _) => "Export IPA…",
        }
    }

    pub(super) fn set_destination(&mut self, destination: Destination, window: &mut Window, cx: &mut Context<Self>) {
        self.destination = destination;
        self.error = None;

        if destination == Destination::Device {
            self.watch_devices(window, cx);
        }

        self.focus.focus(window);
        cx.notify();
    }

    fn choose_entitlements(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }

        let paths = picker::paths(window, cx, "Choose an entitlements plist", false);
        self.picking = true;

        self.picker = Some(cx.spawn_in(window, async move |view, cx| {
            let result = paths.await;

            let _ = view.update_in(cx, |view, window, cx| {
                view.picking = false;
                view.focus.focus(window);
                window.activate_window();

                match result {
                    Ok(Some(paths)) => view.draft.options.entitlements = paths.into_iter().next(),
                    Err(error) => view.error = Some(error.to_string()),
                    _ => {}
                }

                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_signing(&self, cx: &mut Context<Self>) -> AnyElement {
        let locked = self.occupied();

        let modes = div().flex().gap_2().children(ExportMode::ALL.into_iter().enumerate().map(|(index, mode)| {
            Button::new(("mode", index))
                .label(mode.label())
                .outline()
                .selected(self.mode == mode)
                .debug_selector(move || format!("mode:{}", mode.label()))
                .disabled(locked)
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.mode = mode;
                    view.error = None;
                    view.focus.focus(window);
                    cx.notify();
                }))
        }));

        card(cx)
            .child(section_title("Signing"))
            .child(modes)
            .child(muted(self.mode.description(), cx))
            .when(self.mode == ExportMode::AppleId, |this| this.child(self.render_apple_id(locked, cx)))
            .into_any_element()
    }

    fn render_apple_id(&self, locked: bool, cx: &mut Context<Self>) -> AnyElement {
        let accounts = if self.accounts.list.is_empty() {
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .child(muted("No Apple ID is signed in. Sign in under Accounts to provision this app.", cx)),
                )
                .child(
                    Button::new("open-accounts")
                        .outline()
                        .label("Open Accounts")
                        .debug_selector(|| "open-accounts".into())
                        .on_click(cx.listener(|view, _, window, cx| view.show_section(Section::Accounts, window, cx))),
                )
                .into_any_element()
        } else {
            let choices = self.accounts.list.iter().enumerate().map(|(index, account)| {
                let apple_id = account.apple_id.clone();
                let label =
                    if account.has_session { apple_id.clone() } else { format!("{apple_id} (sign-in required)") };

                Button::new(("signing-account", index))
                    .outline()
                    .label(label)
                    .selected(self.draft.apple_id.as_ref() == Some(&apple_id))
                    .disabled(locked)
                    .debug_selector({
                        let apple_id = apple_id.clone();
                        move || format!("signing-account:{apple_id}")
                    })
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.draft.apple_id = Some(apple_id.clone());
                        view.error = None;
                        cx.notify();
                    }))
            });

            div().flex().flex_wrap().gap_2().children(choices).into_any_element()
        };

        let policies =
            div().flex().gap_2().children(IdentifierPolicy::ALL.into_iter().enumerate().map(|(index, policy)| {
                Button::new(("identifier-policy", index))
                    .outline()
                    .label(policy.label())
                    .selected(self.draft.identifier_policy == policy)
                    .disabled(locked)
                    .debug_selector(move || format!("identifier-policy:{}", policy.label()))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.draft.identifier_policy = policy;
                        cx.notify();
                    }))
            }));

        let entitlements = self.draft.options.entitlements.as_ref().map(|path| path.display().to_string());
        let entitlements_label =
            entitlements.clone().unwrap_or_else(|| "Use the provisioning profile's entitlements.".into());

        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(div().flex().flex_col().gap_2().child(div().text_sm().child("Account")).child(accounts))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_sm().child("Bundle identifier"))
                    .child(policies)
                    .child(muted(self.draft.identifier_policy.description(), cx)),
            )
            .child(
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
                            .child(div().text_sm().child("Entitlements"))
                            .child(muted(entitlements_label, cx)),
                    )
                    .child(
                        Button::new("choose-entitlements")
                            .outline()
                            .label("Choose…")
                            .disabled(locked)
                            .on_click(cx.listener(|view, _, window, cx| view.choose_entitlements(window, cx))),
                    )
                    .when(entitlements.is_some(), |this| {
                        this.child(Button::new("clear-entitlements").ghost().label("Clear").disabled(locked).on_click(
                            cx.listener(|view, _, _, cx| {
                                view.draft.options.entitlements = None;
                                cx.notify();
                            }),
                        ))
                    }),
            )
            .child(
                Checkbox::new("provision-extensions")
                    .label("Register each extension separately (uses one free-team App ID per extension)")
                    .checked(self.draft.options.provision_extensions)
                    .disabled(locked)
                    .on_click(cx.listener(|view, checked, _, cx| {
                        view.draft.options.provision_extensions = *checked;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    pub(super) fn render_destination(&self, cx: &mut Context<Self>) -> AnyElement {
        let locked = self.occupied();
        let apple_id = self.mode == ExportMode::AppleId;

        let choices = div().flex().gap_2().children(
            [Destination::Export, Destination::Device].into_iter().enumerate().map(|(index, destination)| {
                Button::new(("destination", index))
                    .outline()
                    .label(destination.label())
                    .selected(self.destination == destination)
                    .disabled(locked)
                    .debug_selector(move || format!("destination:{}", destination.label()))
                    .on_click(cx.listener(move |view, _, window, cx| view.set_destination(destination, window, cx)))
            }),
        );

        let device = self.destination == Destination::Device;

        let options = device.then(|| {
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(section_title("Install options"))
                .child(
                    Checkbox::new("stream-upload")
                        .label("Stream the upload (no temporary IPA)")
                        .checked(self.draft.options.stream_upload)
                        .disabled(locked)
                        .on_click(cx.listener(|view, checked, _, cx| {
                            view.draft.options.stream_upload = *checked;
                            cx.notify();
                        })),
                )
                .child(
                    Checkbox::new("track-refresh")
                        .label("Track for automatic refresh (Apple ID)")
                        .checked(apple_id && self.draft.options.track_for_refresh)
                        .disabled(locked || !apple_id)
                        .debug_selector(|| "track-refresh".into())
                        .on_click(cx.listener(|view, checked, _, cx| {
                            view.draft.options.track_for_refresh = *checked;
                            cx.notify();
                        })),
                )
                .child(
                    Checkbox::new("tvos-apple-tv")
                        .label("Provision Apple TV targets for tvOS (Apple ID)")
                        .checked(apple_id && self.draft.options.tvos_for_apple_tv)
                        .disabled(locked || !apple_id)
                        .on_click(cx.listener(|view, checked, _, cx| {
                            view.draft.options.tvos_for_apple_tv = *checked;
                            cx.notify();
                        })),
                )
                .child(div().w_48().child(field("Upload chunk (MiB, 1–64)", &self.fields.upload_chunk, locked)))
        });

        card(cx)
            .child(section_title("Destination"))
            .child(choices)
            .when(device, |this| this.child(self.render_device_list("target-device", locked, cx)))
            .children(options)
            .into_any_element()
    }
}
