//! Injection sources and the custom icon: local files and `.deb` packages from the picker, typed
//! `http(s)://` URLs, the recovered specials, and a replacement PNG icon. The engine resolves
//! URLs and specials when the job runs (docs/ACQUIRE.md).

use super::{
    Sideport,
    widgets::{badge, muted, section_title},
};
use crate::{ExportMode, InjectionKind, SPECIAL_INJECTIONS, injection_url, picker, validate_icon};
use gpui::{AnyElement, Context, IntoElement, Window, div, img, prelude::*, px};
use gpui_component::{
    Disableable, Selectable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent},
};
use sl_engine::LibraryInjection;
use std::path::{Path, PathBuf};

/// "Substrate" for a special, the URL or path otherwise.
fn injection_label(source: &Path) -> String {
    let special = SPECIAL_INJECTIONS.iter().find(|(path, _)| source == Path::new(path));

    match special {
        Some((_, label)) => format!("{label}, resolved when the job runs"),
        None => source.display().to_string(),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned()
}

impl Sideport {
    /// Whether the editor accepts edits now: nothing else owns the window and the mode re-signs.
    fn editable(&self) -> bool {
        !self.occupied() && self.mode != ExportMode::Original
    }

    /// Choose local injections: dylibs, frameworks, bundles, resources and `.deb` packages.
    pub(super) fn add_injection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }

        let paths = picker::paths(window, cx, "Choose dylibs, frameworks, bundles, .deb packages or resources", true);
        self.picking = true;

        self.picker = Some(cx.spawn_in(window, async move |view, cx| {
            let result = paths.await;

            let _ = view.update_in(cx, |view, window, cx| {
                view.picking = false;
                view.focus.focus(window);
                window.activate_window();

                match result {
                    Ok(Some(paths)) => view.add_injection_sources(paths, cx),
                    Err(error) => view.error = Some(error.to_string()),
                    _ => {}
                }

                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Add each source once, keeping the order in which they were chosen.
    pub(super) fn add_injection_sources(&mut self, sources: Vec<PathBuf>, cx: &mut Context<Self>) {
        let injections = &mut self.draft.options.injections;

        for source in sources {
            if !injections.iter().any(|item| item.source == source) {
                injections.push(LibraryInjection { source, name: None });
            }
        }

        self.error = None;
        cx.notify();
    }

    /// Add the typed `http(s)://` URL; the engine downloads it when the job runs.
    pub(super) fn add_injection_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }

        let text = self.fields.injection_url.read(cx).value().to_string();

        match injection_url(&text) {
            Ok(url) => {
                self.fields.injection_url.update(cx, |input, cx| input.set_value("", window, cx));
                self.add_injection_sources(vec![url], cx);
            }
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    pub(super) fn add_injection_url_on_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.subscribe_in(&self.fields.injection_url, window, |view, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                view.add_injection_url(window, cx);
            }
        })
        .detach();
    }

    fn toggle_special(&mut self, source: &'static str, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }

        let injections = &mut self.draft.options.injections;
        let special = Path::new(source);

        if injections.iter().any(|item| item.source == special) {
            injections.retain(|item| item.source != special);
        } else {
            injections.push(LibraryInjection { source: special.into(), name: None });
        }

        cx.notify();
    }

    fn choose_icon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }

        let paths = picker::paths(window, cx, "Choose a PNG app icon", false);
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
                            view.set_custom_icon(path, cx);
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

    /// Use a PNG as the app icon (`AppOptions::icon`); Original mode leaves it out of the job.
    pub(super) fn set_custom_icon(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match validate_icon(&path) {
            Ok(()) => {
                self.draft.options.icon = Some(path);
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }

        cx.notify();
    }

    pub(super) fn render_icon_choice(&self, disabled: bool, cx: &mut Context<Self>) -> AnyElement {
        let original = self.mode == ExportMode::Original;
        let icon = self.draft.options.icon.as_ref().filter(|_| !original);

        let description = match (original, icon) {
            (true, _) => "Custom icons need a re-signing mode; Original keeps the app unchanged.".to_string(),
            (false, Some(path)) => {
                format!("{} replaces the app's PNG icons, resized to each size the app declares.", file_name(path))
            }
            (false, None) => "The app keeps its own icon. Choose a PNG to replace its icon files.".to_string(),
        };
        let details = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().child("App icon"))
            .child(muted(description, cx));

        let preview = icon.map(|path| img(path.clone()).size(px(40.)).flex_shrink_0().rounded_lg());

        let choose = Button::new("choose-icon")
            .outline()
            .label("Choose PNG…")
            .disabled(disabled)
            .debug_selector(|| "choose-icon".into())
            .on_click(cx.listener(|view, _, window, cx| view.choose_icon(window, cx)));
        let clear = self.draft.options.icon.is_some().then(|| {
            Button::new("clear-icon")
                .ghost()
                .label("Use original")
                .disabled(disabled)
                .debug_selector(|| "clear-icon".into())
                .on_click(cx.listener(|view, _, _, cx| {
                    view.draft.options.icon = None;
                    cx.notify();
                }))
        });

        div()
            .flex()
            .items_center()
            .gap_3()
            .when(icon.is_some(), |this| this.debug_selector(|| "custom-icon".into()))
            .children(preview)
            .child(details)
            .child(choose)
            .children(clear)
            .into_any_element()
    }

    pub(super) fn render_injections(&self, disabled: bool, cx: &mut Context<Self>) -> AnyElement {
        let injections = &self.draft.options.injections;

        let add_files = Button::new("add-injection")
            .outline()
            .label("Add files…")
            .disabled(disabled)
            .debug_selector(|| "add-injection".into())
            .on_click(cx.listener(|view, _, window, cx| view.add_injection(window, cx)));
        let header =
            div().flex().justify_between().items_center().child(section_title("Libraries & tweaks")).child(add_files);

        let add_url = Button::new("add-injection-url")
            .outline()
            .label("Add URL")
            .disabled(disabled)
            .debug_selector(|| "add-injection-url".into())
            .on_click(cx.listener(|view, _, window, cx| view.add_injection_url(window, cx)));
        let url_field = Input::new(&self.fields.injection_url).disabled(disabled);
        let url = div().flex().gap_2().child(div().flex_1().child(url_field)).child(add_url);

        let specials = SPECIAL_INJECTIONS.into_iter().enumerate().map(|(index, (source, label))| {
            let name = source.trim_start_matches("///special/");
            let selected = injections.iter().any(|item| item.source == Path::new(source));

            Button::new(("special", index))
                .outline()
                .label(label)
                .selected(selected)
                .disabled(disabled)
                .debug_selector(move || format!("special:{name}"))
                .on_click(cx.listener(move |view, _, _, cx| view.toggle_special(source, cx)))
        });
        let specials = div().flex().items_center().gap_2().child(div().text_sm().child("Specials")).children(specials);

        let rows = injections.iter().enumerate().map(|(index, injection)| {
            let source = injection.source.clone();
            let selector = source.to_string_lossy().into_owned();
            let kind = InjectionKind::of(&source);

            let remove = Button::new(("remove-injection", index))
                .ghost()
                .label("Remove")
                .disabled(disabled)
                .debug_selector(move || format!("remove-injection:{selector}"))
                .on_click(cx.listener({
                    let source = source.clone();
                    move |view, _, _, cx| {
                        view.draft.options.injections.retain(|injection| injection.source != source);
                        cx.notify();
                    }
                }));

            div()
                .flex()
                .items_center()
                .gap_3()
                .child(div().flex_1().min_w_0().text_sm().child(injection_label(&source)))
                .children(kind.label().map(|label| badge(label, cx)))
                .child(remove)
        });

        let explanation = "Add dylibs, frameworks, bundles, resources or .deb packages, whose tweaks are unpacked \
                           when the job runs. URLs and specials are downloaded then too.";

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(header)
            .child(muted(explanation, cx))
            .child(url)
            .child(specials)
            .when(injections.is_empty(), |this| this.child(muted("Nothing is injected.", cx)))
            .children(rows)
            .into_any_element()
    }
}
