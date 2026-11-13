//! The settings sheet: values shared by every profile, rarely changed.
use super::{app::AppView, widgets::code_block};
use crate::{platform, profile::State};
use gpui_kit::component::{
    ActiveTheme as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    v_flex,
};
use gpui_kit::{Context, IntoElement, ParentElement, SharedString, Styled, Window, div, px};

impl AppView {
    /// Opens the settings sheet.
    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity();
        window.open_sheet(cx, move |sheet, _, cx| {
            let this = view.read(cx);
            let theme = cx.theme();
            let log_file = this.state.settings.log_file();
            let state_file = State::default_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "not saved: no settings folder".into());

            let row = |label: &'static str, help: &'static str, control: gpui_kit::AnyElement| {
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child(label))
                    .child(control)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(help),
                    )
            };

            let choose = view.clone();
            let rescan = view.clone();
            let reveal_dir = log_file.parent().map(|p| p.to_path_buf());
            sheet
                .title("Settings")
                .size(px(420.))
                .child(
                    v_flex()
                        .gap_5()
                        .child(row(
                            "Time server",
                            "The Soul Injector gets its clock from this NTP server through the PC, so production log entries carry UTC times. Leave empty to skip.",
                            Input::new(&this.ntp_input).into_any_element(),
                        ))
                        .child(row(
                            "Response timeout (ms)",
                            "How long to wait for each answer from the Soul Injector: 100 to 60000.",
                            div().w(px(140.)).child(Input::new(&this.timeout_input)).into_any_element(),
                        ))
                        .child(row(
                            "Connection settle time (ms)",
                            "Wait after opening the USB port before the first request: 0 to 10000.",
                            div().w(px(140.)).child(Input::new(&this.settle_input)).into_any_element(),
                        ))
                        .child(row(
                            "WiFi port",
                            "Port Soul Injectors on WiFi connect to; it must match their server address. Ports below 1024 need permission on Linux. Used from the next start.",
                            div().w(px(140.)).child(Input::new(&this.listen_port_input)).into_any_element(),
                        ))
                        .child(row(
                            "Production log file",
                            "Pulled entries are appended here, one JSON object per line.",
                            v_flex()
                                .gap_2()
                                .child(code_block(
                                    "log-file",
                                    SharedString::from(log_file.display().to_string()),
                                    cx,
                                ))
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Button::new("choose-log")
                                                .outline()
                                                .label("Change…")
                                                .on_click(move |_, window, cx| {
                                                    choose.update(cx, |this, cx| {
                                                        this.choose_log_file(window, cx)
                                                    })
                                                }),
                                        )
                                        .child(
                                            Button::new("reveal-log")
                                                .ghost()
                                                .label("Open folder")
                                                .on_click(move |_, _, _| {
                                                    if let Some(dir) = &reveal_dir {
                                                        let _ = std::fs::create_dir_all(dir);
                                                        let _ = platform::reveal(dir);
                                                    }
                                                }),
                                        ),
                                )
                                .into_any_element(),
                        ))
                        .child(row(
                            "GDB",
                            "Search PATH and the usual toolchain folders again, for example after installing a toolchain.",
                            h_flex()
                                .child(
                                    Button::new("rescan-gdb")
                                        .outline()
                                        .label("Search again")
                                        .on_click(move |_, window, cx| {
                                            rescan.update(cx, |this, cx| this.scan_gdbs(window, cx))
                                        }),
                                )
                                .into_any_element(),
                        ))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("Settings and profiles are saved in {state_file}")),
                        ),
                )
        });
    }
}
