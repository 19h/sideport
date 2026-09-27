#![forbid(unsafe_code)]

use clap::Parser;
use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, Menu, MenuItem, OsAction, TitlebarOptions, WindowBounds,
    WindowOptions, actions, px, size,
};
use gpui_component::Root;
use sl_app::{Assets, CancelJob, ExportApp, OpenApp, Sideport, apply_theme};
use sl_engine::{Engine, EngineConfig};
use std::{cell::RefCell, path::PathBuf, rc::Rc};

actions!(sideport, [Quit]);

#[derive(Parser)]
#[command(name = "Sideport", about = "Prepare and export apps")]
struct Arguments {
    /// IPA, app ZIP, or .app to open at startup.
    source: Option<PathBuf>,
    /// Override the directory containing desktop settings.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let arguments = Arguments::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let engine = Engine::new(EngineConfig { data_dir: arguments.data_dir, ..EngineConfig::default() })?;
    let startup_error = Rc::new(RefCell::new(None));
    let error_slot = startup_error.clone();

    Application::new().with_assets(Assets).run(move |cx: &mut App| {
        gpui_component::init(cx);
        bind_keys(cx);
        set_menus(cx);

        let bounds = Bounds::centered(None, size(px(1040.), px(820.)), cx);
        let mut sideport = None;
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(780.), px(600.))),
                titlebar: Some(TitlebarOptions { title: Some("Sideport".into()), ..TitlebarOptions::default() }),
                app_id: Some("com.sideport.desktop".into()),
                ..WindowOptions::default()
            },
            |window, cx| {
                apply_theme(engine.settings().theme, window, cx);
                let view = cx.new(|cx| Sideport::new(engine, window, cx));

                if let Some(source) = arguments.source {
                    view.update(cx, |view, cx| view.load_path(source, window, cx));
                }

                sideport = Some(view.downgrade());
                cx.new(|cx| Root::new(view, window, cx))
            },
        );

        let window = match opened {
            Ok(window) => window,
            Err(error) => {
                *error_slot.borrow_mut() = Some(error);
                cx.quit();
                return;
            }
        };

        // Quit follows the same cancellation-and-join path as the window close button.
        cx.on_action(move |_: &Quit, cx| {
            if let Some(view) = &sideport {
                let _ = window.update(cx, |_, window, cx| {
                    let _ = view.update(cx, |view, cx| view.request_close(window, cx));
                });
            }
        });
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        cx.activate(true);
    });

    if let Some(error) = startup_error.borrow_mut().take() {
        return Err(error);
    }

    Ok(())
}

fn bind_keys(cx: &mut App) {
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-o", OpenApp, Some("Sideport")),
        KeyBinding::new("cmd-e", ExportApp, Some("Sideport")),
        KeyBinding::new("cmd-.", CancelJob, Some("Sideport")),
        KeyBinding::new("cmd-q", Quit, None),
    ]);

    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([
        KeyBinding::new("ctrl-o", OpenApp, Some("Sideport")),
        KeyBinding::new("ctrl-e", ExportApp, Some("Sideport")),
        KeyBinding::new("ctrl-.", CancelJob, Some("Sideport")),
        KeyBinding::new("ctrl-q", Quit, None),
    ]);
}

fn set_menus(cx: &mut App) {
    use gpui_component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};

    cx.set_menus(vec![
        Menu { name: "Sideport".into(), items: vec![MenuItem::action("Quit Sideport", Quit)] },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("Open app…", OpenApp),
                MenuItem::action("Export IPA…", ExportApp),
                MenuItem::separator(),
                MenuItem::action("Cancel operation", CancelJob),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::os_action("Undo", Undo, OsAction::Undo),
                MenuItem::os_action("Redo", Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", Cut, OsAction::Cut),
                MenuItem::os_action("Copy", Copy, OsAction::Copy),
                MenuItem::os_action("Paste", Paste, OsAction::Paste),
                MenuItem::os_action("Select all", SelectAll, OsAction::SelectAll),
            ],
        },
    ]);
}
