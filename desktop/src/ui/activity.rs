//! The bottom of the main column: what is happening now, and the log.
use super::{app::AppView, widgets::level_color};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    progress::Progress,
    spinner::Spinner,
    v_flex,
};
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement,
    StatefulInteractiveElement as _, Styled, Window, div, prelude::FluentBuilder as _, px,
};

/// Height of the expanded log.
const LOG_HEIGHT: f32 = 140.;

impl AppView {
    pub(super) fn render_activity(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let status: AnyElement = match &self.running {
            Some(running) => h_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .child(Spinner::new().small())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .child(running.step.clone()),
                )
                .when_some(running.progress, |this, progress| {
                    this.child(
                        div()
                            .flex_none()
                            .w(px(160.))
                            .child(Progress::new("progress").value(progress * 100.0)),
                    )
                })
                .when(!running.preparing, |this| {
                    this.child(
                        Button::new("stop")
                            .small()
                            .outline()
                            .label(if running.stopping {
                                "Stopping…"
                            } else {
                                "Stop"
                            })
                            .disabled(running.stopping)
                            .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                    )
                })
                .into_any_element(),
            None => div()
                .flex_1()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Idle")
                .into_any_element(),
        };

        let toggle = Button::new("toggle-log")
            .ghost()
            .small()
            .icon(if self.show_log {
                IconName::ChevronDown
            } else {
                IconName::ChevronUp
            })
            .label("Activity")
            .on_click(cx.listener(|this, _, _, cx| {
                this.show_log = !this.show_log;
                cx.notify();
            }));

        let bar = h_flex()
            .w_full()
            .h(px(40.))
            .flex_none()
            .px_4()
            .gap_3()
            .border_t_1()
            .border_color(theme.border)
            .child(status)
            .child(toggle)
            .when(self.show_log, |this| {
                this.child(
                    Button::new("clear-log")
                        .ghost()
                        .small()
                        .label("Clear")
                        .disabled(self.log.is_empty())
                        .on_click(cx.listener(|this, _, _, cx| this.clear_log(cx))),
                )
            });

        let mut column = v_flex().w_full().flex_none().child(bar);
        if self.show_log {
            let mut lines = v_flex().w_full().px_4().py_2().gap_0p5();
            for line in &self.log {
                lines = lines.child(
                    h_flex()
                        .w_full()
                        .gap_3()
                        .items_start()
                        .text_xs()
                        .child(
                            div()
                                .flex_none()
                                .font_family(theme.mono_font_family.clone())
                                .text_color(theme.muted_foreground)
                                .child(line.time.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(match line.level {
                                    crate::worker::Level::Info => theme.foreground,
                                    level => level_color(level, cx),
                                })
                                .child(line.text.clone()),
                        ),
                );
            }
            column = column.child(
                div()
                    .id("log")
                    .w_full()
                    .h(px(LOG_HEIGHT))
                    .overflow_y_scroll()
                    .track_scroll(&self.log_scroll)
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.muted.opacity(0.4))
                    .child(lines),
            );
        }
        column.into_any_element()
    }
}
