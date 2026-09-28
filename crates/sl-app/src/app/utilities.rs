//! Device utilities for the selected device: its connection and a heartbeat check, the Developer
//! Disk Image, JIT for developer-signed apps (the recovered "Enable JIT for Apps" menu), pairing
//! repair and app-change notifications. docs/DEVICE.md records their engine contracts.

use super::{
    Confirmation, PendingAction, Sideport,
    devices::device_title,
    widgets::{badge, card, muted, section_title},
};
use gpui::{AnyElement, App, Context, Div, IntoElement, SharedString, Task, Window, div, prelude::*};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
};
use sl_engine::{Connection, DdiMount, DeviceApp, DeviceInfo, EngineError, JobEvent, LogLevel};

/// Notifications that change the installed apps, as `sideport device notifications` defaults.
const APP_CHANGES: [&str; 2] = ["com.apple.mobile.application_installed", "com.apple.mobile.application_uninstalled"];

/// A device utility running in the window's job slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Utility {
    MountImage { udid: String },
    Jit { udid: String, bundle_id: String },
}

impl Utility {
    fn udid(&self) -> &str {
        match self {
            Self::MountImage { udid } | Self::Jit { udid, .. } => udid,
        }
    }
}

#[derive(Default)]
pub(crate) struct DeviceTools {
    /// The utility job in the window's job slot; its button shows progress.
    pub(crate) running: Option<Utility>,
    /// UDID of a device answering a heartbeat.
    pub(crate) checking: Option<String>,
    /// The last heartbeat interval in seconds, per device.
    pub(crate) reachable: Option<(String, u64)>,
    /// UDID of the device whose app installs and removals reload its app list.
    pub(crate) following: Option<String>,
    /// The last utility result, per device.
    pub(crate) notice: Option<(String, String)>,
    heartbeat: Option<Task<()>>,
    /// Owns the notification job: dropping the task drops its handle, which cancels the job.
    follow: Option<Task<()>>,
}

/// "USB", "Wi-Fi", "USB + Wi-Fi", or "Not attached" (this Mac lists no transport).
pub(crate) fn connection_label(connections: &[Connection]) -> String {
    let kinds: Vec<_> = connections
        .iter()
        .map(|connection| match connection {
            Connection::Usb => "USB",
            Connection::Network => "Wi-Fi",
        })
        .collect();

    if kinds.is_empty() { "Not attached".into() } else { kinds.join(" + ") }
}

/// Only developer-signed apps can be debugged, so only they are offered JIT, as in the recovered
/// menu; the recovered client also disables JIT for this Mac.
pub(crate) fn jit_eligible(app: &DeviceApp, device: &DeviceInfo) -> bool {
    app.is_developer_app && !is_mac(device)
}

fn is_mac(device: &DeviceInfo) -> bool {
    device.device_class == "Mac"
}

fn mount_message(mount: DdiMount, name: &str) -> String {
    match (mount.already_mounted, mount.personalized) {
        (true, _) => format!("A Developer Disk Image was already mounted on {name}."),
        (false, true) => format!("Mounted the personalized Developer Disk Image on {name}."),
        (false, false) => format!("Mounted the Developer Disk Image on {name}."),
    }
}

/// A titled, described utility; the caller adds its control.
fn utility_row(title: &str, description: impl Into<SharedString>, cx: &App) -> Div {
    div().flex().items_center().gap_4().child(
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().child(title.to_owned()))
            .child(muted(description, cx)),
    )
}

impl Sideport {
    /// Why the selected device offers no utilities: this Mac is not a lockdown device. (The demo
    /// engine simulates them.)
    pub(super) fn utilities_unavailable(&self, device: &DeviceInfo) -> Option<&'static str> {
        is_mac(device).then_some("Device utilities apply to iPhone, iPad and Apple TV.")
    }

    /// Download (or reuse) and mount the Developer Disk Image in the job slot; stage events and
    /// log lines report its progress.
    fn mount_developer_image(&mut self, udid: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() {
            return;
        }

        let name = self.device_name(&udid).unwrap_or_else(|| udid.clone());
        let job = self.engine.mount_developer_image(udid.clone());

        self.devices.tools.notice = None;
        self.devices.tools.running = Some(Utility::MountImage { udid: udid.clone() });

        self.run_job(job, "Preparing the Developer Disk Image…", window, cx, move |view, mount, _, _| {
            let message = mount_message(mount, &name);

            view.status = message.clone();
            view.devices.tools.notice = Some((udid, message));
        });
    }

    /// Launch a developer-signed app with debugserver and detach, so it keeps running with JIT.
    fn enable_jit(&mut self, udid: String, app: &DeviceApp, window: &mut Window, cx: &mut Context<Self>) {
        if self.occupied() || !app.is_developer_app {
            return;
        }

        let name = app.name.clone();
        let bundle_id = app.bundle_id.clone();
        let job = self.engine.enable_jit(udid.clone(), bundle_id.clone(), true);

        self.devices.tools.notice = None;
        self.devices.tools.running = Some(Utility::Jit { udid: udid.clone(), bundle_id });

        self.run_job(job, &format!("Enabling JIT for {name}…"), window, cx, move |view, (), _, _| {
            view.status = format!("JIT enabled for {name}");
            view.devices.tools.notice = Some((udid, format!("{name} is running with JIT enabled.")));
        });
    }

    fn confirm_repair_pairing(&mut self, device: &DeviceInfo, window: &mut Window, cx: &mut Context<Self>) {
        let name = device_title(device);
        let message = format!(
            "Sideport removes this computer's pairing with {name}, then pairs again. Unlock {name}: it asks \
             you to trust this computer again, and Sideport cannot reach it until you tap Trust."
        );
        let confirmation = Confirmation {
            title: format!("Repair pairing with {name}?"),
            message,
            confirm_label: "Repair pairing",
            action: PendingAction::RepairPairing { udid: device.udid.clone() },
        };

        self.confirm(confirmation, window, cx);
    }

    /// Unpair, then pair again. Like Pair, this waits for "Trust This Computer?" outside the job
    /// slot: the engine does not cancel that wait, so closing the window does not wait for it.
    pub(super) fn repair_pairing(&mut self, udid: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.devices.pairing.is_some() {
            return;
        }

        let name = self.device_name(&udid).unwrap_or_else(|| udid.clone());
        let job = self.engine.repair_pairing(udid.clone());
        let events = job.events();

        self.devices.pairing = Some(udid.clone());
        self.devices.tools.notice = None;
        self.status = "Repairing pairing…".into();
        self.error = None;

        self.devices.pair_task = Some(cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = events.recv().await {
                let JobEvent::Log { level, message } = event else {
                    continue;
                };

                let shown = view.update_in(cx, |view, _, cx| {
                    view.status = message.clone();
                    view.add_log(level, message);
                    cx.notify();
                });

                if shown.is_err() {
                    return;
                }
            }

            let repaired = job.result().await;

            let _ = view.update_in(cx, |view, window, cx| {
                view.devices.pairing = None;

                match repaired {
                    Ok(()) => {
                        view.status = "Pairing repaired".into();
                        view.devices.tools.notice = Some((udid, format!("{name} trusts this computer again.")));
                    }
                    Err(error) => {
                        view.error = Some(format!("Pairing repair failed: {error}"));
                        view.status = "Pairing repair failed".into();
                    }
                }

                view.refresh_devices(window, cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// One heartbeat round trip, proving the device (over Wi-Fi too) answers.
    fn check_connection(&mut self, udid: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.devices.tools.checking.is_some() {
            return;
        }

        let heartbeat = self.engine.heartbeat(udid.clone());

        self.devices.tools.checking = Some(udid.clone());
        self.devices.tools.reachable = None;

        self.devices.tools.heartbeat = Some(cx.spawn_in(window, async move |view, cx| {
            let answered = heartbeat.await;

            let _ = view.update_in(cx, |view, _, cx| {
                view.devices.tools.checking = None;

                match answered {
                    Ok(interval) => {
                        view.devices.tools.reachable = Some((udid, interval));
                        view.error = None;
                    }
                    Err(error) => view.error = Some(format!("Connection check failed: {error}")),
                }

                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn stop_following_app_changes(&mut self) {
        self.devices.tools.follow = None;
        self.devices.tools.following = None;
    }

    /// Reload the device's app list whenever it reports an app installed or removed.
    fn follow_app_changes(&mut self, udid: String, follow: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_following_app_changes();

        if !follow {
            cx.notify();
            return;
        }

        let name = self.device_name(&udid).unwrap_or_else(|| udid.clone());
        let names = APP_CHANGES.map(String::from).to_vec();
        let job = self.engine.notifications(udid.clone(), names);
        let events = job.events();

        self.devices.tools.following = Some(udid.clone());

        self.devices.tools.follow = Some(cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = events.recv().await {
                let JobEvent::Log { message, .. } = event else {
                    continue;
                };

                let reloaded = view.update_in(cx, |view, window, cx| {
                    view.add_log(LogLevel::Info, format!("{name}: {message}"));

                    if view.devices.selected.as_deref() == Some(udid.as_str()) {
                        view.load_device_contents(window, cx);
                    }

                    cx.notify();
                });

                if reloaded.is_err() {
                    return;
                }
            }

            let ended = job.result().await;

            let _ = view.update_in(cx, |view, _, cx| {
                view.devices.tools.following = None;

                if let Err(error) = ended
                    && error != EngineError::Cancelled
                {
                    view.error = Some(format!("Stopped following app changes: {error}"));
                }

                cx.notify();
            });
        }));
        cx.notify();
    }

    /// "Enable JIT" beside a developer-signed app.
    pub(super) fn render_jit_button(
        &self,
        index: usize,
        udid: &str,
        app: &DeviceApp,
        locked: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let running = Utility::Jit { udid: udid.to_owned(), bundle_id: app.bundle_id.clone() };
        let bundle_id = app.bundle_id.clone();
        let summary = app.clone();
        let udid = udid.to_owned();

        Button::new(("jit", index))
            .outline()
            .label("Enable JIT")
            .loading(self.devices.tools.running.as_ref() == Some(&running))
            .disabled(locked)
            .debug_selector(move || format!("jit:{bundle_id}"))
            .on_click(cx.listener(move |view, _, window, cx| view.enable_jit(udid.clone(), &summary, window, cx)))
    }

    pub(super) fn render_device_utilities(
        &self,
        device: &DeviceInfo,
        locked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let udid = device.udid.clone();
        let tools = &self.devices.tools;
        let connection = connection_label(&device.connections);

        let connection_badge = badge(connection.clone(), cx).debug_selector({
            let udid = udid.clone();
            move || format!("connection:{udid}")
        });
        let header =
            div().flex().items_center().gap_2().child(section_title("Device utilities")).child(connection_badge);

        if let Some(reason) = self.utilities_unavailable(device) {
            return card(cx).child(header).child(muted(reason, cx)).into_any_element();
        }

        let reachable = tools.reachable.as_ref().filter(|(owner, _)| owner == &udid);
        let connection_text = match reachable {
            Some((_, interval)) => {
                format!("Connected over {connection}; answered a heartbeat ({interval} s interval).")
            }
            None => format!("Connected over {connection}. Check that the device answers, over Wi-Fi too."),
        };
        let check = Button::new("check-connection")
            .outline()
            .label("Check connection")
            .loading(tools.checking.as_ref() == Some(&udid))
            .disabled(tools.checking.is_some())
            .debug_selector(|| "check-connection".into())
            .on_click(cx.listener({
                let udid = udid.clone();
                move |view, _, window, cx| view.check_connection(udid.clone(), window, cx)
            }));

        let image_text = format!(
            "Mounts the developer image for version {}; JIT needs it. The image is downloaded once and cached.",
            device.os_version
        );
        let mount = Button::new("mount-ddi")
            .outline()
            .label("Mount Developer Disk Image")
            .loading(tools.running.as_ref() == Some(&Utility::MountImage { udid: udid.clone() }))
            .disabled(locked)
            .debug_selector(|| "mount-ddi".into())
            .on_click(cx.listener({
                let udid = udid.clone();
                move |view, _, window, cx| view.mount_developer_image(udid.clone(), window, cx)
            }));

        let jit_text = "Choose Enable JIT beside a sideloaded app below: Sideport launches it with the debugger \
                        and detaches. Only developer-signed apps can be debugged, so App Store apps are not offered.";

        let pairing = self.devices.pairing.as_ref() == Some(&udid);
        let pairing_text = "Removes this computer's pairing and pairs again when the device keeps refusing \
                            Sideport. The device asks to trust this computer again.";
        let repair = Button::new("repair-pairing")
            .danger()
            .label(if pairing { "Waiting for Trust…" } else { "Repair pairing…" })
            .loading(pairing)
            .disabled(locked || self.devices.pairing.is_some())
            .debug_selector(|| "repair-pairing".into())
            .on_click(cx.listener({
                let device = device.clone();
                move |view, _, window, cx| view.confirm_repair_pairing(&device, window, cx)
            }));

        let changes_text = "Listens for apps installed or removed on the device, by Sideport or anything else.";
        let follow = Checkbox::new("follow-apps")
            .label("Keep the app list current")
            .checked(tools.following.as_ref() == Some(&udid))
            .debug_selector(|| "follow-apps".into())
            .on_click(cx.listener({
                let udid = udid.clone();
                move |view, checked, window, cx| view.follow_app_changes(udid.clone(), *checked, window, cx)
            }));

        let running_here = tools.running.as_ref().is_some_and(|utility| utility.udid() == udid);
        let progress = (running_here && self.busy).then(|| {
            let step = self.logs.back().map(|(_, message)| format!(" · {message}")).unwrap_or_default();

            muted(format!("{}{step}", self.status), cx).debug_selector(|| "utility-progress".into())
        });

        let notice = tools
            .notice
            .as_ref()
            .filter(|(owner, _)| owner == &udid)
            .map(|(_, message)| div().debug_selector(|| "utility-notice".into()).text_sm().child(message.clone()));

        card(cx)
            .child(header)
            .child(utility_row("Connection", connection_text, cx).child(check))
            .child(utility_row("Developer Disk Image", image_text, cx).child(mount))
            .child(utility_row("Enable JIT", jit_text, cx))
            .child(utility_row("Pairing", pairing_text, cx).child(repair))
            .child(utility_row("App changes", changes_text, cx).child(follow))
            .children(progress)
            .children(notice)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{connection_label, jit_eligible};
    use sl_engine::{Connection, DeviceApp, DeviceInfo};

    #[test]
    fn connection_kinds_and_jit_eligibility_follow_the_device() {
        let cases = [
            (vec![Connection::Usb], "USB"),
            (vec![Connection::Network], "Wi-Fi"),
            (vec![Connection::Usb, Connection::Network], "USB + Wi-Fi"),
            (Vec::new(), "Not attached"),
        ];

        for (connections, label) in cases {
            assert_eq!(connection_label(&connections), label, "{connections:?}");
        }

        let phone = DeviceInfo {
            udid: "UDID".into(),
            name: "Phone".into(),
            product_type: "iPhone15,2".into(),
            model_name: None,
            os_version: "16.5".into(),
            device_class: "iPhone".into(),
            connections: vec![Connection::Usb],
            paired: true,
        };
        let mac = DeviceInfo { device_class: "Mac".into(), connections: Vec::new(), ..phone.clone() };

        let developer = DeviceApp {
            bundle_id: "com.example.dev".into(),
            name: "Dev".into(),
            version: None,
            is_developer_app: true,
        };
        let store = DeviceApp { is_developer_app: false, ..developer.clone() };

        assert!(jit_eligible(&developer, &phone));
        assert!(!jit_eligible(&store, &phone), "App Store apps cannot be debugged");
        assert!(!jit_eligible(&developer, &mac), "the recovered client disables JIT for this Mac");
    }
}
