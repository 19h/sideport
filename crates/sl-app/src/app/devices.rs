//! Attached devices: discovery, pairing, installed apps and provisioning profiles.

use super::{
    Confirmation, PendingAction, Section, Sideport,
    widgets::{badge, card, danger_text, format_time, muted, page_title, section_title},
};
use gpui::{AnyElement, Context, IntoElement, Task, Window, div, prelude::*, px};
use gpui_component::{
    Disableable, Selectable,
    button::{Button, ButtonVariants},
};
use sl_engine::{Connection, DeviceApp, DeviceInfo, DeviceProfile};

#[derive(Default)]
pub(crate) struct Devices {
    pub(crate) list: Vec<DeviceInfo>,
    /// At least one listing finished.
    pub(crate) listed: bool,
    /// Why devices cannot be listed (for example, no usbmuxd service).
    pub(crate) unavailable: Option<String>,
    /// The device used for installation and shown in the Devices section.
    pub(crate) selected: Option<String>,
    pub(crate) apps: Option<(String, Vec<DeviceApp>)>,
    pub(crate) profiles: Option<(String, Vec<DeviceProfile>)>,
    pub(crate) loading: bool,
    /// UDID of a device waiting for "Trust This Computer?".
    pub(crate) pairing: Option<String>,
    watch: Option<Task<()>>,
    listing: Option<Task<()>>,
    pair_task: Option<Task<()>>,
    contents: Option<Task<()>>,
    change: Option<Task<()>>,
}

/// "Jane's iPhone", or a placeholder for a device that has not shared its name.
pub(super) fn device_title(device: &DeviceInfo) -> String {
    if device.name.is_empty() { format!("Device {}", short_udid(&device.udid)) } else { device.name.clone() }
}

/// Model, OS and transports, e.g. "iPhone 14 Pro · iOS 18.2 · USB + Wi-Fi".
pub(super) fn device_detail(device: &DeviceInfo) -> String {
    let model = device.model_name.clone().unwrap_or_else(|| device.product_type.clone());
    let system = match device.device_class.as_str() {
        "AppleTV" => "tvOS",
        "iPad" => "iPadOS",
        _ => "iOS",
    };
    let connections: Vec<_> = device
        .connections
        .iter()
        .map(|connection| match connection {
            Connection::Usb => "USB",
            Connection::Network => "Wi-Fi",
        })
        .collect();

    let mut parts = Vec::new();

    if !model.is_empty() {
        parts.push(model);
    }
    if !device.os_version.is_empty() {
        parts.push(format!("{system} {}", device.os_version));
    }
    if !connections.is_empty() {
        parts.push(connections.join(" + "));
    }

    parts.join(" · ")
}

fn short_udid(udid: &str) -> &str {
    udid.get(udid.len().saturating_sub(8)..).unwrap_or(udid)
}

impl Sideport {
    pub(crate) fn selected_device(&self) -> Option<&DeviceInfo> {
        let udid = self.devices.selected.as_ref()?;

        self.devices.list.iter().find(|device| &device.udid == udid)
    }

    pub(super) fn device_name(&self, udid: &str) -> Option<String> {
        self.devices.list.iter().find(|device| device.udid == udid).map(device_title)
    }

    /// Start following attached devices once: an initial listing, then the engine's snapshots.
    pub(super) fn watch_devices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.devices.watch.is_some() {
            return;
        }

        let updates = self.engine.subscribe_devices();
        let initial = self.engine.devices();

        self.devices.watch = Some(cx.spawn_in(window, async move |view, cx| {
            let listed = initial.await;

            if view.update_in(cx, |view, window, cx| view.apply_devices(listed, window, cx)).is_err() {
                return;
            }

            while let Ok(devices) = updates.recv().await {
                if view.update_in(cx, |view, window, cx| view.apply_devices(Ok(devices), window, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn apply_devices(
        &mut self,
        listed: sl_engine::Result<Vec<DeviceInfo>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match listed {
            Ok(list) => {
                self.devices.list = list;
                self.devices.unavailable = None;
            }
            Err(error) => {
                self.devices.list.clear();
                self.devices.unavailable = Some(error.to_string());
            }
        }

        self.devices.listed = true;

        if self.selected_device().is_none() {
            self.devices.selected = self.devices.list.first().map(|device| device.udid.clone());

            if self.section == Section::Devices {
                self.load_device_contents(window, cx);
            }
        }

        cx.notify();
    }

    fn refresh_devices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let listed = self.engine.devices();

        self.devices.listing = Some(cx.spawn_in(window, async move |view, cx| {
            let listed = listed.await;

            let _ = view.update_in(cx, |view, window, cx| {
                view.apply_devices(listed, window, cx);

                if view.section == Section::Devices {
                    view.load_device_contents(window, cx);
                }
            });
        }));
    }

    pub(super) fn pair_device(&mut self, udid: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.devices.pairing.is_some() {
            return;
        }

        let pairing = self.engine.pair_device(udid.clone());
        self.devices.pairing = Some(udid);
        self.status = "Unlock the device and tap Trust to pair".into();
        self.error = None;

        self.devices.pair_task = Some(cx.spawn_in(window, async move |view, cx| {
            let paired = pairing.await;

            let _ = view.update_in(cx, |view, window, cx| {
                view.devices.pairing = None;

                match paired {
                    Ok(()) => {
                        view.status = "Device paired".into();
                        view.refresh_devices(window, cx);
                    }
                    Err(error) => {
                        view.error = Some(format!("Pairing failed: {error}"));
                        view.status = "Pairing failed".into();
                    }
                }

                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn select_device(&mut self, udid: String, window: &mut Window, cx: &mut Context<Self>) {
        self.devices.selected = Some(udid);

        if self.section == Section::Devices {
            self.load_device_contents(window, cx);
        }

        cx.notify();
    }

    /// List the selected device's apps and profiles; unpaired devices refuse these services.
    pub(super) fn load_device_contents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(udid) = self.selected_device().filter(|device| device.paired).map(|device| device.udid.clone()) else {
            self.devices.apps = None;
            self.devices.profiles = None;
            self.devices.contents = None;
            self.devices.loading = false;
            return;
        };

        let apps = self.engine.device_apps(udid.clone());
        let profiles = self.engine.device_profiles(udid.clone());
        self.devices.loading = true;

        self.devices.contents = Some(cx.spawn_in(window, async move |view, cx| {
            let (apps, profiles) = futures::join!(apps, profiles);

            let _ = view.update_in(cx, |view, _, cx| {
                view.devices.loading = false;

                match apps {
                    Ok(apps) => view.devices.apps = Some((udid.clone(), apps)),
                    Err(error) => {
                        view.devices.apps = None;
                        view.error = Some(format!("Could not list apps: {error}"));
                    }
                }

                match profiles {
                    Ok(profiles) => view.devices.profiles = Some((udid, profiles)),
                    Err(error) => {
                        view.devices.profiles = None;
                        view.error = Some(format!("Could not list profiles: {error}"));
                    }
                }

                cx.notify();
            });
        }));
        cx.notify();
    }

    fn confirm_uninstall(&mut self, udid: String, app: &DeviceApp, window: &mut Window, cx: &mut Context<Self>) {
        let confirmation = Confirmation {
            title: format!("Uninstall {}?", app.name),
            message: format!("{} ({}) and its data will be removed from the device.", app.name, app.bundle_id),
            confirm_label: "Uninstall",
            action: PendingAction::UninstallApp { udid, bundle_id: app.bundle_id.clone() },
        };

        self.confirm(confirmation, window, cx);
    }

    fn confirm_remove_profile(
        &mut self,
        udid: String,
        profile: &DeviceProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let confirmation = Confirmation {
            title: "Remove provisioning profile?".into(),
            message: format!("Apps that rely on \"{}\" stop launching until they are signed again.", profile.name),
            confirm_label: "Remove",
            action: PendingAction::RemoveProfile { udid, uuid: profile.uuid.clone() },
        };

        self.confirm(confirmation, window, cx);
    }

    pub(super) fn uninstall_app(
        &mut self,
        udid: String,
        bundle_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let removal = self.engine.uninstall_app(udid, bundle_id.clone());
        self.status = format!("Uninstalling {bundle_id}…");
        self.error = None;

        self.devices.change = Some(cx.spawn_in(window, async move |view, cx| {
            let removed = removal.await;

            let _ = view.update_in(cx, |view, window, cx| {
                match removed {
                    Ok(()) => view.status = format!("Uninstalled {bundle_id}"),
                    Err(error) => {
                        view.error = Some(format!("Uninstall failed: {error}"));
                        view.status = "Uninstall failed".into();
                    }
                }

                view.load_device_contents(window, cx);
            });
        }));
    }

    pub(super) fn remove_profile(&mut self, udid: String, uuid: String, window: &mut Window, cx: &mut Context<Self>) {
        let removal = self.engine.remove_profile(udid, uuid.clone());
        self.status = "Removing provisioning profile…".into();
        self.error = None;

        self.devices.change = Some(cx.spawn_in(window, async move |view, cx| {
            let removed = removal.await;

            let _ = view.update_in(cx, |view, window, cx| {
                match removed {
                    Ok(()) => view.status = format!("Removed profile {uuid}"),
                    Err(error) => {
                        view.error = Some(format!("Profile removal failed: {error}"));
                        view.status = "Profile removal failed".into();
                    }
                }

                view.load_device_contents(window, cx);
            });
        }));
    }

    /// Selectable device rows with pairing controls. `prefix` keeps element IDs distinct.
    pub(super) fn render_device_list(&self, prefix: &'static str, locked: bool, cx: &mut Context<Self>) -> AnyElement {
        if !self.devices.listed {
            return muted("Looking for devices…", cx).into_any_element();
        }

        if let Some(reason) = &self.devices.unavailable {
            return danger_text(format!("Devices are unavailable: {reason}"), cx).into_any_element();
        }

        if self.devices.list.is_empty() {
            return muted(
                "No devices found. Connect an iPhone, iPad, or Apple TV, unlock it, and trust this computer.",
                cx,
            )
            .into_any_element();
        }

        let rows = self.devices.list.iter().enumerate().map(|(index, device)| {
            let udid = device.udid.clone();
            let selected = self.devices.selected.as_ref() == Some(&device.udid);
            let pairing = self.devices.pairing.as_ref() == Some(&device.udid);
            let label = format!("{} — {}", device_title(device), device_detail(device));

            let select = Button::new((prefix, index))
                .outline()
                .label(label)
                .selected(selected)
                .disabled(locked)
                .debug_selector({
                    let udid = udid.clone();
                    move || format!("{prefix}:{udid}")
                })
                .on_click(cx.listener({
                    let udid = udid.clone();
                    move |view, _, window, cx| view.select_device(udid.clone(), window, cx)
                }));

            let pair = (!device.paired).then(|| {
                Button::new(("pair", index))
                    .primary()
                    .label(if pairing { "Waiting for Trust…" } else { "Pair" })
                    .loading(pairing)
                    .disabled(self.busy || self.devices.pairing.is_some())
                    .debug_selector({
                        let udid = udid.clone();
                        move || format!("pair:{udid}")
                    })
                    .on_click(cx.listener({
                        let udid = udid.clone();
                        move |view, _, window, cx| view.pair_device(udid.clone(), window, cx)
                    }))
            });

            div()
                .flex()
                .items_center()
                .gap_3()
                .child(div().flex_1().min_w_0().child(select))
                .when(!device.paired, |this| this.child(badge("Not paired", cx)))
                .children(pair)
        });

        div().flex().flex_col().gap_2().children(rows).into_any_element()
    }

    pub(super) fn render_devices(&self, cx: &mut Context<Self>) -> AnyElement {
        let locked = self.occupied();

        let list = card(cx)
            .child(
                div().flex().items_center().justify_between().child(section_title("Connected devices")).child(
                    Button::new("refresh-devices")
                        .ghost()
                        .label("Refresh")
                        .on_click(cx.listener(|view, _, window, cx| view.refresh_devices(window, cx))),
                ),
            )
            .child(self.render_device_list("device", locked, cx));

        let details = self.selected_device().map(|device| self.render_device_contents(device, locked, cx));

        div()
            .max_w(px(1100.))
            .mx_auto()
            .flex()
            .flex_col()
            .gap_5()
            .child(page_title("Devices"))
            .child(list)
            .children(details)
            .into_any_element()
    }

    fn render_device_contents(&self, device: &DeviceInfo, locked: bool, cx: &mut Context<Self>) -> AnyElement {
        if !device.paired {
            return card(cx)
                .child(section_title(&device_title(device)))
                .child(muted("Pair this device to see its apps and provisioning profiles.", cx))
                .into_any_element();
        }

        let udid = device.udid.clone();
        let apps = self.devices.apps.as_ref().filter(|(owner, _)| owner == &udid).map(|(_, apps)| apps.as_slice());
        let profiles =
            self.devices.profiles.as_ref().filter(|(owner, _)| owner == &udid).map(|(_, profiles)| profiles.as_slice());

        let app_rows = apps.unwrap_or_default().iter().enumerate().map(|(index, app)| {
            let summary = app.clone();
            let udid = udid.clone();
            let bundle_id = app.bundle_id.clone();
            let version = app.version.as_deref().map(|version| format!(" · {version}")).unwrap_or_default();

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
                                .child(app.name.clone())
                                .when(app.is_developer_app, |this| this.child(badge("Sideloaded", cx))),
                        )
                        .child(muted(format!("{}{version}", app.bundle_id), cx)),
                )
                .child(
                    Button::new(("uninstall", index))
                        .danger()
                        .label("Uninstall")
                        .disabled(locked)
                        .debug_selector(move || format!("uninstall:{bundle_id}"))
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.confirm_uninstall(udid.clone(), &summary, window, cx)
                        })),
                )
        });

        let profile_rows = profiles.unwrap_or_default().iter().enumerate().map(|(index, profile)| {
            let summary = profile.clone();
            let udid = udid.clone();
            let uuid = profile.uuid.clone();
            let expires = profile.expires.map(|time| format!(" · expires {}", format_time(time)));
            let team = profile.team_id.as_deref().map(|team| format!(" · team {team}")).unwrap_or_default();
            let app_id = profile.app_id.clone().unwrap_or_else(|| "Unknown App ID".into());

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
                                .child(profile.name.clone())
                                .when(profile.is_free, |this| this.child(badge("Free team", cx))),
                        )
                        .child(muted(format!("{app_id}{team}{}", expires.unwrap_or_default()), cx)),
                )
                .child(
                    Button::new(("remove-profile", index))
                        .danger()
                        .label("Remove")
                        .disabled(locked)
                        .debug_selector(move || format!("remove-profile:{uuid}"))
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.confirm_remove_profile(udid.clone(), &summary, window, cx)
                        })),
                )
        });

        let loading = self.devices.loading;

        let apps_card = card(cx)
            .child(section_title(&format!("Apps on {}", device_title(device))))
            .when(loading && apps.is_none(), |this| this.child(muted("Loading apps…", cx)))
            .when(apps.is_some_and(<[DeviceApp]>::is_empty), |this| this.child(muted("No user-installed apps.", cx)))
            .children(app_rows);

        let profiles_card = card(cx)
            .child(section_title("Provisioning profiles"))
            .when(loading && profiles.is_none(), |this| this.child(muted("Loading profiles…", cx)))
            .when(profiles.is_some_and(<[DeviceProfile]>::is_empty), |this| {
                this.child(muted("No provisioning profiles are installed.", cx))
            })
            .children(profile_rows);

        div().flex().flex_col().gap_5().child(apps_card).child(profiles_card).into_any_element()
    }
}
