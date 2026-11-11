//! The mode bar: Program, Debug or Power, chosen by what the user wants to
//! do rather than by how the Soul Injector does it.
use super::{
    app::{Activity, AppView},
    assets::Lucide,
};
use crate::profile::Mode;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement,
    StatefulInteractiveElement as _, Styled, TestSupportExt as _, div, prelude::FluentBuilder as _,
    px,
};

impl Mode {
    /// Icon, title and one line on what the mode is for.
    fn describe(self) -> (Lucide, &'static str, &'static str) {
        match self {
            Mode::Program => (
                Lucide::Microchip,
                "Program",
                "Put firmware on targets. The Soul Injector can also do it on its own.",
            ),
            Mode::Debug => (
                Lucide::Bug,
                "Debug",
                "Step through firmware with GDB, VS Code or CLion.",
            ),
            Mode::Power => (
                Lucide::Activity,
                "Power",
                "Measure how much current the target draws over time.",
            ),
        }
    }
}

impl AppView {
    /// Whether something is running in `mode`, shown on its card.
    fn is_active(&self, mode: Mode, cx: &gpui_kit::App) -> bool {
        let activity = self.running.as_ref().map(|r| r.activity);
        match mode {
            Mode::Program => matches!(activity, Some(Activity::Program | Activity::Push)),
            Mode::Debug => activity == Some(Activity::Debug),
            Mode::Power => self.power.read(cx).is_capturing(),
        }
    }

    pub(super) fn render_mode_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let cards: Vec<AnyElement> = Mode::ALL
            .into_iter()
            .enumerate()
            .map(|(index, mode)| {
                let theme = cx.theme();
                let (icon, title, text) = mode.describe();
                let selected = self.state.mode == mode;
                let active = self.is_active(mode, cx);
                h_flex()
                    .id(("mode", index))
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .items_start()
                    .p_3()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(if selected {
                        theme.foreground.opacity(0.7)
                    } else {
                        theme.border
                    })
                    .when(selected, |this| this.bg(theme.secondary))
                    .when(!selected, |this| {
                        this.cursor_pointer()
                            .hover(|style| style.bg(theme.secondary.opacity(0.5)))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.set_mode(mode, cx)))
                    .child(
                        div()
                            .flex_none()
                            .size(px(32.))
                            .rounded(theme.radius)
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(if selected { theme.primary } else { theme.muted })
                            .child(Icon::new(icon).small().text_color(if selected {
                                theme.primary_foreground
                            } else {
                                theme.muted_foreground
                            })),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(title),
                                    )
                                    .when(active, |this| {
                                        this.child(
                                            div().size(px(7.)).rounded_full().bg(theme.success),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(text),
                            ),
                    )
                    .test_support()
                    .into_any_element()
            })
            .collect();
        let theme = cx.theme();
        h_flex()
            .w_full()
            .flex_none()
            .gap_3()
            .px_6()
            .py_3()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.background)
            .children(cards)
            .into_any_element()
    }
}
