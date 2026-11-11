//! The left column: the Soul Injector and the saved profiles.
use super::{
    app::{Activity, AppView},
    widgets::{detail, dot, group_heading, icon_button, level_color},
};
use crate::{describe, worker::Level};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement as _, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use sidp_agent::manage::proto::Trigger;

/// Width of the sidebar.
pub const SIDEBAR_WIDTH: f32 = 272.;

impl AppView {
    pub(super) fn render_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let device = self.render_device(window, cx);
        let profiles = self.render_profiles(window, cx);
        // macOS has these in the menu bar; GPUI has no native menu bar on Linux.
        let footer = (!cfg!(target_os = "macos")).then(|| self.render_sidebar_footer(cx));
        let theme = cx.theme();
        v_flex()
            .flex_none()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .border_r_1()
            .border_color(theme.sidebar_border)
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .child(device)
            .child(div().mx_3().my_2().h(px(1.)).bg(theme.sidebar_border))
            .child(profiles)
            .children(footer)
            .into_any_element()
    }

    fn render_sidebar_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let dark = cx.theme().is_dark();
        h_flex()
            .flex_none()
            .w_full()
            .px_2()
            .py_2()
            .gap_1()
            .border_t_1()
            .border_color(cx.theme().sidebar_border)
            .child(
                Button::new("settings")
                    .ghost()
                    .small()
                    .icon(IconName::Settings)
                    .label("Settings")
                    .tooltip("Connection and log settings (Ctrl+,)")
                    .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
            )
            .child(div().flex_1())
            .child(icon_button(
                "theme",
                if dark { IconName::Sun } else { IconName::Moon },
                "Switch between light and dark",
                cx.listener(|this, _, window, cx| this.toggle_theme(window, cx)),
            ))
            .into_any_element()
    }

    fn render_device(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.is_busy();
        let selected = self.state.port.clone();
        let inspecting = self
            .running
            .as_ref()
            .is_some_and(|r| r.activity == Activity::Inspect);

        let refresh = icon_button(
            "refresh-device",
            IconName::Redo2,
            "Read the Soul Injector's status again",
            cx.listener(|this, _, window, cx| this.inspect(window, cx)),
        )
        .loading(inspecting)
        .disabled(busy || self.selected_port().is_none());

        let mut column = v_flex().pt_3().gap_1().child(group_heading(
            "SOUL INJECTOR",
            Some(refresh.into_any_element()),
            cx,
        ));

        if self.ports.is_empty() {
            column = column.child(
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("No USB device found. Connect the Soul Injector with a USB cable."),
            );
        }

        let device_ok = self.device_snapshot().is_some();
        for (index, port) in self.ports.iter().enumerate() {
            let is_selected = selected.as_deref() == Some(port.path.as_str());
            let theme = cx.theme();
            let color = if !port.soul_injector {
                theme.muted_foreground.opacity(0.5)
            } else if is_selected && device_ok {
                theme.success
            } else if is_selected && self.device_error.is_some() {
                theme.danger
            } else {
                theme.muted_foreground
            };
            let path = port.path.clone();
            column = column.child(
                h_flex()
                    .id(("port", index))
                    .mx_2()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .rounded(theme.radius)
                    .when(is_selected, |this| {
                        this.bg(theme.sidebar_accent)
                            .text_color(theme.sidebar_accent_foreground)
                    })
                    .when(!busy, |this| {
                        this.cursor_pointer()
                            .hover(|this| this.bg(theme.sidebar_accent))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_port(path.clone(), window, cx)
                            }))
                    })
                    .child(dot(color))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().truncate().child(port.product.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(port.path.clone()),
                            ),
                    ),
            );
        }

        if self.selected_port().is_some() {
            column = column.child(self.render_device_details(cx));
        }
        column.into_any_element()
    }

    fn render_device_details(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.is_busy();
        let theme = cx.theme();
        let mut details = v_flex().mx_3().mt_2().gap_1();

        if let Some(error) = &self.device_error {
            details = details.child(
                div()
                    .text_xs()
                    .text_color(level_color(Level::Error, cx))
                    .child(error.clone()),
            );
        }

        let Some(device) = self.device_snapshot() else {
            if self.device_error.is_none() {
                details = details.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Reading status…"),
                );
            }
            return details.into_any_element();
        };

        let info = &device.info;
        let job = &device.job;
        details = details
            .child(detail("Serial", info.serial.clone(), cx))
            .child(detail("Firmware", info.firmware_version.clone(), cx))
            .child(detail(
                "Storage",
                format!(
                    "{:.1} of {:.1} MiB free",
                    f64::from(info.storage_free_kb) / 1024.0,
                    f64::from(info.storage_total_kb) / 1024.0
                ),
                cx,
            ));

        let job_text: SharedString = if job.has_job {
            let trigger = if job.trigger == Trigger::AutoOnDetect as i32 {
                " · programs on plug-in"
            } else {
                ""
            };
            format!("{}{trigger}", job.name).into()
        } else {
            "None".into()
        };
        details = details.child(detail("Job", job_text, cx));
        if job.current_run_id != 0 {
            details = details.child(detail("Run", "Programming a target", cx));
        } else if let Some(result) = job.last_result.as_ref().filter(|r| r.run_id != 0) {
            details = details.child(detail("Last run", describe::run_result(result), cx));
        }

        let waiting = info.log_newest_id > info.log_acked_id;
        let (log_text, log_level) = if info.log_full {
            (
                "Full: programming is refused until it is pulled",
                Some(Level::Error),
            )
        } else if waiting {
            ("Has entries to pull", None)
        } else {
            ("All entries pulled", None)
        };
        details = details.child(
            h_flex()
                .w_full()
                .gap_2()
                .items_start()
                .text_xs()
                .child(
                    div()
                        .flex_none()
                        .w(px(64.))
                        .text_color(theme.muted_foreground)
                        .child("Log"),
                )
                .child(
                    div()
                        .flex_1()
                        .when_some(log_level, |this, level| {
                            this.text_color(level_color(level, cx))
                                .font_weight(FontWeight::MEDIUM)
                        })
                        .child(log_text),
                ),
        );

        let pulling = self
            .running
            .as_ref()
            .is_some_and(|r| r.activity == Activity::PullLog);
        let pull = Button::new("pull-log")
            .small()
            .label("Pull production log")
            .loading(pulling)
            .disabled(busy)
            .tooltip(SharedString::from(format!(
                "Append new entries to {}",
                self.state.settings.log_file().display()
            )))
            .on_click(cx.listener(|this, _, window, cx| this.pull_log(window, cx)));
        let pull = if info.log_full {
            pull.primary()
        } else {
            pull.outline()
        };
        details.child(div().pt_2().child(pull)).into_any_element()
    }

    fn render_profiles(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.is_busy();
        let add = icon_button(
            "add-profile",
            IconName::Plus,
            "New profile",
            cx.listener(|this, _, window, cx| this.new_profile(window, cx)),
        )
        .disabled(busy);

        let mut list = v_flex().gap_0p5();
        for (index, profile) in self.state.profiles.iter().enumerate() {
            let selected = index == self.state.selected;
            let theme = cx.theme();
            let subtitle = profile
                .target
                .as_deref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "No target yet".into());
            let subtitle = match profile
                .target
                .as_deref()
                .and_then(|p| p.parent())
                .and_then(|p| p.file_name())
            {
                Some(folder) => format!("{}/{subtitle}", folder.to_string_lossy()),
                None => subtitle,
            };
            list = list.child(
                h_flex()
                    .id(("profile", index))
                    .mx_2()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .rounded(theme.radius)
                    .when(selected, |this| {
                        this.bg(theme.sidebar_accent)
                            .text_color(theme.sidebar_accent_foreground)
                    })
                    .when(!busy && !selected, |this| {
                        this.cursor_pointer()
                            .hover(|this| this.bg(theme.sidebar_accent))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_profile(index, window, cx)
                            }))
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .when(selected, |this| this.font_weight(FontWeight::MEDIUM))
                                    .child(profile.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(subtitle),
                            ),
                    )
                    .when(selected && !busy, |this| {
                        this.child(
                            h_flex()
                                .flex_none()
                                .child(icon_button(
                                    "duplicate-profile",
                                    IconName::Copy,
                                    "Duplicate profile",
                                    cx.listener(|this, _, window, cx| {
                                        this.duplicate_profile(window, cx)
                                    }),
                                ))
                                .child(icon_button(
                                    "delete-profile",
                                    IconName::Close,
                                    "Delete profile",
                                    cx.listener(|this, _, window, cx| {
                                        this.confirm_delete_profile(window, cx)
                                    }),
                                )),
                        )
                    }),
            );
        }

        v_flex()
            .flex_1()
            .min_h_0()
            .gap_1()
            .child(group_heading("PROFILES", Some(add.into_any_element()), cx))
            .child(
                div()
                    .id("profile-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pb_3()
                    .child(list),
            )
            .into_any_element()
    }
}
