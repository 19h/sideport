//! Small layout helpers shared by the window's sections.

use chrono::{DateTime, Local, Utc};
use gpui::{App, Div, Entity, FontWeight, IntoElement, SharedString, div, prelude::*, px};
use gpui_component::{
    ActiveTheme,
    input::{Input, InputState},
};

pub(super) fn section_title(title: &str) -> Div {
    div().font_weight(FontWeight::SEMIBOLD).child(title.to_owned())
}

pub(super) fn page_title(title: &str) -> Div {
    div().text_2xl().font_weight(FontWeight::SEMIBOLD).child(title.to_owned())
}

pub(super) fn card(cx: &App) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_4()
        .rounded_xl()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().popover)
        .p_6()
}

pub(super) fn muted(text: impl Into<SharedString>, cx: &App) -> Div {
    div().text_sm().text_color(cx.theme().muted_foreground).child(text.into())
}

pub(super) fn badge(text: impl Into<SharedString>, cx: &App) -> Div {
    div().px_2().py_0p5().rounded_md().bg(cx.theme().muted).text_xs().child(text.into())
}

pub(super) fn danger_text(text: impl Into<SharedString>, cx: &App) -> Div {
    div().text_sm().text_color(cx.theme().danger).child(text.into())
}

pub(super) fn field(label: &str, state: &Entity<InputState>, disabled: bool) -> Div {
    labelled(label, Input::new(state).disabled(disabled))
}

pub(super) fn labelled(label: &str, control: impl IntoElement) -> Div {
    div().flex_1().min_w_0().flex().flex_col().gap_2().child(div().text_sm().child(label.to_owned())).child(control)
}

/// A local date and time, e.g. "Oct 3, 14:05".
pub(super) fn format_time(time: DateTime<Utc>) -> String {
    time.with_timezone(&Local).format("%b %-d, %H:%M").to_string()
}

pub(super) fn format_bytes(bytes: u64) -> String {
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

/// A fixed-size square showing the first letter when an app has no icon.
pub(super) fn icon_placeholder(name: &str, size: f32, cx: &App) -> Div {
    let initial = name.chars().next().map(|character| character.to_uppercase().to_string()).unwrap_or_default();

    div()
        .size(px(size))
        .flex_shrink_0()
        .rounded_xl()
        .bg(cx.theme().muted)
        .flex()
        .items_center()
        .justify_center()
        .text_xl()
        .child(initial)
}
