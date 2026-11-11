//! Small building blocks shared by the views.
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    v_flex,
};
use gpui_kit::{
    AnyElement, App, ClickEvent, Div, ElementId, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};

use crate::worker::Level;

/// Width of the label column in forms.
const LABEL_WIDTH: f32 = 168.;

/// A card with an optional step number, a title and a one-line explanation.
pub fn section(
    number: impl Into<Option<usize>>,
    title: &'static str,
    subtitle: impl Into<SharedString>,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let number = number.into();
    v_flex()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .gap_4()
        .p_5()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .child(
            h_flex()
                .gap_3()
                .items_start()
                .when_some(number, |this, number| {
                    this.child(
                        div()
                            .flex_none()
                            .mt(px(1.))
                            .size(px(22.))
                            .rounded_full()
                            .bg(theme.secondary)
                            .text_color(theme.secondary_foreground)
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(number.to_string()),
                    )
                })
                .child(
                    v_flex()
                        .gap_0p5()
                        .child(
                            div()
                                .text_base()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(subtitle.into()),
                        ),
                ),
        )
}

/// A form row: label on the left, control and optional help underneath.
pub fn field(
    label: impl Into<SharedString>,
    help: Option<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> Div {
    h_flex()
        .w_full()
        .gap_4()
        .items_start()
        .child(
            div()
                .flex_none()
                .w(px(LABEL_WIDTH))
                .pt(px(6.))
                .text_sm()
                .child(label.into()),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(control)
                .when_some(help, |this, help| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(help),
                    )
                }),
        )
}

/// A path input with a Browse button. The input gets `id` and the button
/// `<id>-browse`.
pub fn path_input(
    id: &str,
    input: &gpui_kit::Entity<InputState>,
    disabled: bool,
    on_browse: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    h_flex()
        .w_full()
        .gap_2()
        .child(
            div().flex_1().min_w_0().overflow_hidden().child(
                Input::new(input)
                    .id(SharedString::from(id.to_owned()))
                    .w_full()
                    .disabled(disabled),
            ),
        )
        .child(
            Button::new(SharedString::from(format!("{id}-browse")))
                .outline()
                .label("Browse…")
                .disabled(disabled)
                .on_click(on_browse),
        )
}

/// Monospace text in a muted box, scrolling sideways when lines are long.
pub fn code_block(id: impl Into<ElementId>, text: impl Into<SharedString>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    div()
        .id(id)
        .w_full()
        .overflow_x_scroll()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.muted)
        .px_3()
        .py_2()
        .font_family(theme.mono_font_family.clone())
        .text_size(theme.mono_font_size)
        .whitespace_nowrap()
        .child(text.into())
        .into_any_element()
}

/// A line of text with an icon, colored by importance.
pub fn notice(level: Level, text: impl Into<SharedString>, cx: &App) -> Div {
    let color = level_color(level, cx);
    let icon = match level {
        Level::Info => IconName::Info,
        Level::Success => IconName::CircleCheck,
        Level::Warning => IconName::TriangleAlert,
        Level::Error => IconName::CircleX,
    };
    h_flex()
        .gap_2()
        .items_start()
        .text_sm()
        .child(
            div()
                .flex_none()
                .pt(px(2.))
                .child(Icon::new(icon).small().text_color(color)),
        )
        .child(div().flex_1().min_w_0().child(text.into()))
}

/// Color for a level.
pub fn level_color(level: Level, cx: &App) -> Hsla {
    let theme = cx.theme();
    match level {
        Level::Info => theme.muted_foreground,
        Level::Success => theme.success,
        Level::Warning => theme.warning,
        Level::Error => theme.danger,
    }
}

/// A small colored dot.
pub fn dot(color: Hsla) -> Div {
    div().flex_none().size(px(8.)).rounded_full().bg(color)
}

/// A muted label-value pair for compact details.
pub fn detail(label: &'static str, value: impl Into<SharedString>, cx: &App) -> Div {
    h_flex()
        .w_full()
        .gap_2()
        .items_start()
        .text_xs()
        .child(
            div()
                .flex_none()
                .w(px(64.))
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(div().flex_1().min_w_0().child(value.into()))
}

/// Uppercase heading for sidebar groups, with optional trailing controls.
pub fn group_heading(title: &'static str, trailing: Option<AnyElement>, cx: &App) -> Div {
    h_flex()
        .w_full()
        .h(px(28.))
        .justify_between()
        .px_3()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(cx.theme().muted_foreground)
                .child(title),
        )
        .when_some(trailing, |this, trailing| this.child(trailing))
}

/// A ghost icon button with a tooltip.
pub fn icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    tooltip: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(icon)
        .tooltip(tooltip)
        .on_click(on_click)
}
