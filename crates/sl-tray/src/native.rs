//! The menu-bar icon and menu: `sl_tray::menu` rendered with `tray-icon` on a `winit` event loop.
//!
//! The engine's scheduler runs in this process. The menu is rebuilt after every refresh
//! notification and once a minute, so the remaining time stays current. Menu actions run as
//! engine jobs on background threads; questions they would ask (sign-in, retries) are declined,
//! as in unattended refresh, and the outcome is shown as the menu's status line.

use chrono::Utc;
use sl_engine::{Engine, JobEvent, JobHandle, PromptReply, RefreshEvent};
use sl_tray::{TrayAction, TrayItem};
use std::collections::HashMap;
use std::time::Duration;
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, MenuItemKind, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::WindowId;

/// How often the menu is rebuilt without a refresh notification.
const REBUILD_INTERVAL: Duration = Duration::from_secs(60);

/// Side length of the generated template icon, in pixels.
const ICON_SIZE: u32 = 36;

#[derive(Debug)]
enum UserEvent {
    Menu(MenuId),
    Rebuild,
    Status(String),
}

struct Tray {
    engine: Engine,
    proxy: EventLoopProxy<UserEvent>,
    icon: Option<TrayIcon>,
    actions: HashMap<MenuId, TrayAction>,
    status: Option<String>,
}

pub fn run(engine: Engine) -> anyhow::Result<()> {
    #[allow(unused_mut)]
    let mut builder = EventLoop::<UserEvent>::with_user_event();

    // A menu-bar item only: no Dock icon and no application menu.
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
    }

    let event_loop = builder.build()?;
    let proxy = event_loop.create_proxy();

    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event.id));
    }));

    forward_refresh_events(&engine, proxy.clone());
    rebuild_periodically(proxy.clone());

    let mut tray = Tray { engine, proxy, icon: None, actions: HashMap::new(), status: None };
    event_loop.run_app(&mut tray)?;

    Ok(())
}

/// Build the native menu for the current state and print its titles, indented by level; `[x]`
/// marks a checked item and `---` a separator. A check that the menu builds without a menu bar.
pub fn print_menu(engine: Engine) -> anyhow::Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    let mut tray = Tray { engine, proxy, icon: None, actions: HashMap::new(), status: None };
    let menu = tray.menu();

    print_items(&menu.items(), 0);

    Ok(())
}

fn print_items(items: &[MenuItemKind], depth: usize) {
    let indent = "  ".repeat(depth);

    for item in items {
        match item {
            MenuItemKind::MenuItem(item) => {
                println!("{indent}{}{}", item.text(), if item.is_enabled() { "" } else { " (disabled)" })
            }
            MenuItemKind::Check(item) => {
                println!("{indent}[{}] {}", if item.is_checked() { "x" } else { " " }, item.text())
            }
            MenuItemKind::Predefined(_) => println!("{indent}---"),
            MenuItemKind::Icon(item) => println!("{indent}{}", item.text()),
            MenuItemKind::Submenu(submenu) => {
                println!("{indent}{} >", submenu.text());
                print_items(&submenu.items(), depth + 1);
            }
        }
    }
}

/// Show each refresh notification as the status line; stop when the event loop is gone.
fn forward_refresh_events(engine: &Engine, proxy: EventLoopProxy<UserEvent>) {
    let events = engine.subscribe_refresh();

    std::thread::spawn(move || {
        while let Ok(event) = futures::executor::block_on(events.recv()) {
            let status = match event {
                RefreshEvent::Started { app_name, .. } => format!("Refreshing {app_name}…"),
                RefreshEvent::Succeeded { app_name, .. } => format!("Refreshed {app_name}"),
                RefreshEvent::Failed { app_name, error, .. } => format!("{app_name}: {error}"),
            };

            if proxy.send_event(UserEvent::Status(status)).is_err() {
                return;
            }
        }
    });
}

fn rebuild_periodically(proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(REBUILD_INTERVAL);

            if proxy.send_event(UserEvent::Rebuild).is_err() {
                return;
            }
        }
    });
}

/// A built menu entry that can be appended to a menu or submenu.
enum Entry {
    Item(MenuItem),
    Check(CheckMenuItem),
    Submenu(Submenu),
    Separator(PredefinedMenuItem),
}

impl Entry {
    fn as_item(&self) -> &dyn IsMenuItem {
        match self {
            Entry::Item(item) => item,
            Entry::Check(item) => item,
            Entry::Submenu(item) => item,
            Entry::Separator(item) => item,
        }
    }
}

impl Tray {
    fn create(&mut self) {
        let menu = self.menu();
        let icon = Icon::from_rgba(template_icon(), ICON_SIZE, ICON_SIZE).expect("generated icon");

        let built = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .with_icon_as_template(true)
            .with_tooltip("Sideport refresh")
            .build();

        match built {
            Ok(icon) => self.icon = Some(icon),
            Err(error) => tracing::error!("cannot show the menu-bar icon: {error}"),
        }
    }

    fn rebuild(&mut self) {
        let menu = self.menu();

        if let Some(icon) = &self.icon {
            icon.set_menu(Some(Box::new(menu)));
        }
    }

    fn menu(&mut self) -> Menu {
        let installations = self.engine.installations().unwrap_or_default();
        let autostart = std::env::current_exe().ok().map(|_| self.engine.autostart());
        let items = sl_tray::menu(&installations, Utc::now(), autostart, self.status.as_deref());

        self.actions.clear();

        let menu = Menu::new();

        for item in &items {
            let entry = self.entry(item);
            let _ = menu.append(entry.as_item());
        }

        menu
    }

    fn entry(&mut self, item: &TrayItem) -> Entry {
        match item {
            TrayItem::Label(text) => Entry::Item(MenuItem::new(text, false, None)),
            TrayItem::Separator => Entry::Separator(PredefinedMenuItem::separator()),

            TrayItem::Action { title, action } => {
                let id = self.register(*action);
                Entry::Item(MenuItem::with_id(id, title, true, None))
            }

            TrayItem::Check { title, checked, action } => {
                let id = self.register(*action);
                Entry::Check(CheckMenuItem::with_id(id, title, true, *checked, None))
            }

            TrayItem::Submenu { title, items } => {
                let submenu = Submenu::new(title, true);

                for child in items {
                    let entry = self.entry(child);
                    let _ = submenu.append(entry.as_item());
                }

                Entry::Submenu(submenu)
            }
        }
    }

    fn register(&mut self, action: TrayAction) -> MenuId {
        let id = MenuId::new(self.actions.len().to_string());
        self.actions.insert(id.clone(), action);

        id
    }

    fn perform(&mut self, action: TrayAction, event_loop: &ActiveEventLoop) {
        let engine = self.engine.clone();
        let proxy = self.proxy.clone();

        match action {
            TrayAction::Refresh(id) => unattended(engine.refresh(id), proxy, "Refreshed"),

            TrayAction::RefreshAll => {
                std::thread::spawn(move || {
                    for installation in engine.installations().unwrap_or_default() {
                        let _ = run_declining(engine.refresh(installation.id));
                    }

                    let _ = proxy.send_event(UserEvent::Rebuild);
                });
            }

            TrayAction::EnableJit(id) => {
                let installation = engine.installations().unwrap_or_default().into_iter().find(|entry| entry.id == id);

                if let Some(installation) = installation {
                    let job = engine.enable_jit(installation.device_udid, installation.bundle_id, true);
                    unattended(job, proxy, "Enabled JIT for");
                }
            }

            TrayAction::Forget(id) => {
                if confirm("Forget this installation?", "Sideport stops refreshing it. The app stays on the device.") {
                    self.report(engine.forget_installation(id), "Forgot the installation");
                }
            }

            TrayAction::ResetDatabase => {
                let message = "Sideport forgets every tracked installation. Apps stay on their devices.";

                if confirm("Reset the installation database?", message) {
                    let forgotten = engine
                        .installations()
                        .and_then(|all| all.iter().try_for_each(|entry| engine.forget_installation(entry.id)));
                    self.report(forgotten, "Forgot every installation");
                }
            }

            TrayAction::OpenApp => open_desktop_app(&engine),

            TrayAction::ToggleAutostart => {
                let enabled = !engine.autostart();
                let program = std::env::current_exe().map_err(|error| sl_engine::EngineError::Other(error.to_string()));
                let toggled = program.and_then(|program| engine.set_autostart(enabled, &program));
                let done = if enabled { "Starts at login" } else { "No longer starts at login" };

                self.report(toggled, done);
            }

            TrayAction::Quit => event_loop.exit(),
        }
    }

    fn report(&mut self, result: sl_engine::Result<()>, done: &str) {
        self.status = Some(match result {
            Ok(()) => done.to_owned(),
            Err(error) => error.to_string(),
        });

        self.rebuild();
    }
}

impl ApplicationHandler<UserEvent> for Tray {
    fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) {
        // The status item must be created once the application has finished launching.
        if cause == StartCause::Init {
            self.create();
        }
    }

    fn resumed(&mut self, _: &ActiveEventLoop) {}

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Rebuild => self.rebuild(),

            UserEvent::Status(status) => {
                self.status = Some(status);
                self.rebuild();
            }

            UserEvent::Menu(id) => {
                if let Some(action) = self.actions.get(&id).copied() {
                    self.perform(action, event_loop);
                }
            }
        }
    }
}

/// Run a job on a background thread and show its outcome as the status line.
fn unattended<T: Send + 'static>(job: JobHandle<T>, proxy: EventLoopProxy<UserEvent>, done: &'static str) {
    std::thread::spawn(move || {
        let status = match run_declining(job) {
            Ok(()) => done.to_owned(),
            Err(error) => error.to_string(),
        };

        let _ = proxy.send_event(UserEvent::Status(status));
        let _ = proxy.send_event(UserEvent::Rebuild);
    });
}

/// Wait for a job, declining its questions: nobody is at the menu to answer them.
fn run_declining<T>(job: JobHandle<T>) -> sl_engine::Result<()> {
    let events = job.events();

    futures::executor::block_on(async {
        while let Ok(event) = events.recv().await {
            if let JobEvent::Prompt(prompt) = event {
                prompt.answer(PromptReply::Cancel);
            }
        }

        job.result().await.map(|_| ())
    })
}

/// Raise the running desktop app through local IPC, or start it (recovered `pingOrRunSideloadly`).
fn open_desktop_app(engine: &Engine) {
    if engine.ipc_client(None).and_then(|client| client.raise(None)).unwrap_or(false) {
        return;
    }

    let sibling = std::env::current_exe().ok().map(|exe| exe.with_file_name("SideportDesktop"));

    let started = match sibling.filter(|path| path.is_file()) {
        Some(path) => std::process::Command::new(path).spawn().map(drop),
        None if cfg!(target_os = "macos") => {
            std::process::Command::new("open").args(["-b", "com.sideport.desktop"]).spawn().map(drop)
        }
        None => Err(std::io::Error::other("the desktop app is not installed beside the tray")),
    };

    if let Err(error) = started {
        tracing::warn!("cannot start the desktop app: {error}");
    }
}

#[cfg(target_os = "macos")]
fn confirm(title: &str, description: &str) -> bool {
    let answer = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title(title)
        .set_description(description)
        .set_buttons(rfd::MessageButtons::OkCancel)
        .show();

    answer == rfd::MessageDialogResult::Ok
}

/// Without a native dialog the destructive actions are refused.
#[cfg(not(target_os = "macos"))]
fn confirm(_: &str, _: &str) -> bool {
    false
}

/// A template image (only alpha matters): a ring around a downward arrow.
fn template_icon() -> Vec<u8> {
    let size = ICON_SIZE as f32;
    let center = size / 2.0;
    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);

    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let (px, py) = (x as f32 + 0.5 - center, y as f32 + 0.5 - center);
            let radius = (px * px + py * py).sqrt();

            // A ring, a vertical stem and a "V" whose tip points down.
            let tip = size * 0.24;
            let ring = (radius - size * 0.42).abs() < size * 0.045;
            let stem = px.abs() < size * 0.045 && py > -size * 0.24 && py < tip;
            let head = px.abs() < size * 0.2 && (py - (tip - px.abs())).abs() < size * 0.05;

            let alpha = if ring || stem || head { 255 } else { 0 };
            rgba.extend_from_slice(&[0, 0, 0, alpha]);
        }
    }

    rgba
}
