//! The main column: one page per mode. Program has the target and the
//! programming controls; Debug has the target, the debug server and the
//! debugger settings; Power has the power measurement.
use super::{
    app::{Activity, AppView, PathField, Readiness},
    widgets::{code_block, dot, field, notice, path_input, section},
};
use crate::{
    profile::{DebugMode, Mode},
    snippets::Client,
    target::Family,
    worker::{GdbState, Level},
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    select::Select,
    switch::Switch,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{
    AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};

/// Where the Arm GNU Toolchain can be downloaded.
const TOOLCHAIN_URL: &str = "https://developer.arm.com/downloads/-/arm-gnu-toolchain-downloads";

impl AppView {
    pub(super) fn render_main(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sections: Vec<AnyElement> = match self.state.mode {
            Mode::Program => vec![
                self.render_target(Mode::Program, window, cx),
                self.render_programming(window, cx),
            ],
            Mode::Debug if self.family() == Some(Family::Esp32) => vec![
                self.render_target(Mode::Debug, window, cx),
                self.render_esp32_cannot_debug(cx),
            ],
            Mode::Debug => vec![
                self.render_target(Mode::Debug, window, cx),
                self.render_debug(window, cx),
                self.render_debugger(window, cx),
            ],
            Mode::Power => vec![self.render_power_page(cx)],
        };
        h_flex()
            .w_full()
            .justify_center()
            .items_start()
            .px_6()
            .py_6()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    // The power charts use whatever width the window gives them.
                    .when(self.state.mode != Mode::Power, |this| this.max_w(px(860.)))
                    .gap_5()
                    .children(sections),
            )
            .into_any_element()
    }

    fn family(&self) -> Option<Family> {
        match &self.readiness {
            Readiness::Ready(job) => Some(job.family),
            _ => self
                .summary
                .as_ref()
                .and_then(|s| s.as_ref().ok())
                .map(|s| s.family),
        }
    }

    fn render_target(&mut self, mode: Mode, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.is_busy();
        let summary = self.summary.as_ref().and_then(|s| s.as_ref().ok());
        let needs_variant = summary.is_some_and(|s| s.needs_variant_choice());
        let is_esp32 = summary.is_some_and(|s| s.family == Family::Esp32);

        let mut body = v_flex()
            .gap_3()
            .child(field(
                "Name",
                None,
                Input::new(&self.name_input)
                    .id("profile-name")
                    .disabled(busy),
                cx,
            ))
            .child(field(
                "Target description",
                Some(
                    "The target.yaml that describes the microcontroller and its flash algorithm."
                        .into(),
                ),
                path_input(
                    "target",
                    &self.target_input,
                    busy,
                    cx.listener(|this, _, window, cx| this.browse(PathField::Target, window, cx)),
                ),
                cx,
            ));

        if needs_variant {
            body = body.child(field(
                "Variant",
                Some("target.yaml describes several chips; choose the one on your board.".into()),
                Select::new(&self.variant_select)
                    .id("variant")
                    .placeholder("Choose a variant")
                    .disabled(busy),
                cx,
            ));
        }

        for image in &self.image_inputs {
            let name = image.name.clone();
            let (label, help): (SharedString, Option<SharedString>) = if is_esp32 {
                (format!("Image {name}").into(), None)
            } else {
                (
                    "Firmware image".into(),
                    Some("The raw binary to program (.bin).".into()),
                )
            };
            body = body.child(field(
                label,
                help,
                path_input(
                    &format!("image-{name}"),
                    &image.input,
                    busy,
                    cx.listener(move |this, _, window, cx| {
                        this.browse(PathField::Image(name.clone()), window, cx)
                    }),
                ),
                cx,
            ));
        }

        if mode == Mode::Debug && !is_esp32 {
            body = body.child(field(
                "Debug symbols",
                Some(
                    "The ELF built with the image. GDB reads source lines and variables from it."
                        .into(),
                ),
                path_input(
                    "elf",
                    &self.elf_input,
                    busy,
                    cx.listener(|this, _, window, cx| this.browse(PathField::Elf, window, cx)),
                ),
                cx,
            ));
        }

        body = body.child(
            h_flex().child(
                Button::new("toggle-more")
                    .ghost()
                    .small()
                    .icon(if self.show_more {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .label("Production options")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_more = !this.show_more;
                        cx.notify();
                    })),
            ),
        );
        if self.show_more {
            body = body
                .child(field(
                    "Before programming",
                    Some(
                        "Optional pre_prog.yaml with steps run before the target is detected."
                            .into(),
                    ),
                    path_input(
                        "pre",
                        &self.pre_input,
                        busy,
                        cx.listener(|this, _, window, cx| {
                            this.browse(PathField::PreProgram, window, cx)
                        }),
                    ),
                    cx,
                ))
                .child(field(
                    "After programming",
                    Some(
                        "Optional post_prog.yaml with steps run after verify and self tests."
                            .into(),
                    ),
                    path_input(
                        "post",
                        &self.post_input,
                        busy,
                        cx.listener(|this, _, window, cx| {
                            this.browse(PathField::PostProgram, window, cx)
                        }),
                    ),
                    cx,
                ))
                .child(field(
                    "Job name",
                    Some("Shown on the Soul Injector and in its production log.".into()),
                    Input::new(&self.job_name_input)
                        .id("job-name")
                        .disabled(busy),
                    cx,
                ));
        }

        body = body.child(div().pt_1().child(self.render_readiness(cx)));

        let subtitle = if mode == Mode::Debug {
            "The firmware you are debugging, and the chip it runs on."
        } else {
            "The firmware to program, and the chip it goes on."
        };
        section(1, "Target", subtitle, cx)
            .child(body)
            .into_any_element()
    }

    fn render_readiness(&self, cx: &Context<Self>) -> AnyElement {
        match &self.readiness {
            Readiness::Checking => notice(Level::Info, "Checking files…", cx).into_any_element(),
            Readiness::Problem(problem) => {
                notice(Level::Warning, problem.clone(), cx).into_any_element()
            }
            Readiness::Ready(job) => {
                let images = job
                    .images
                    .iter()
                    .map(|(name, size)| format!("{name} {}", kib(*size)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut column = v_flex().gap_1().child(notice(
                    Level::Success,
                    format!("Ready: {} · {} · {images}", job.name, job.family.label()),
                    cx,
                ));
                for warning in &job.warnings {
                    column = column.child(notice(Level::Warning, warning.clone(), cx));
                }
                column.into_any_element()
            }
        }
    }

    fn render_debug(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let options = self.state.profile().debug.clone();
        let debugging = self
            .running
            .as_ref()
            .is_some_and(|r| r.activity == Activity::Debug);
        let busy = self.is_busy();
        let locked = busy;

        let mode_index = match options.mode {
            DebugMode::ProgramThenDebug => 0,
            DebugMode::Attach => 1,
        };
        let mode = TabBar::new("debug-mode")
            .segmented()
            .small()
            .selected_index(mode_index)
            .child(Tab::new().label("Program, then debug"))
            .child(Tab::new().label("Debug only"))
            .when(!locked, |this| {
                this.on_click(cx.listener(|this, index: &usize, _, cx| {
                    this.set_debug_option(cx, |o| {
                        o.mode = if *index == 0 {
                            DebugMode::ProgramThenDebug
                        } else {
                            DebugMode::Attach
                        }
                    });
                }))
            });
        let mode_help: SharedString = match options.mode {
            DebugMode::ProgramThenDebug => "Programs the target with the image above, then stops it at its first instruction.".into(),
            DebugMode::Attach => "Stops the target wherever it is, without programming it. The Soul Injector must already hold this target's job.".into(),
        };

        let switches = v_flex()
            .gap_2()
            .child(
                Switch::new("under-reset")
                    .small()
                    .checked(options.connect_under_reset)
                    .disabled(locked)
                    .label("Connect under reset")
                    .tooltip("Hold the reset pin while connecting. Use it when the firmware sleeps or reuses the SWD pins soon after boot. The reset pin must be wired.")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        let checked = *checked;
                        this.set_debug_option(cx, |o| o.connect_under_reset = checked);
                    })),
            )
            .child(
                Switch::new("resume")
                    .small()
                    .checked(options.resume_on_detach)
                    .disabled(locked)
                    .label("Let the target run when GDB detaches")
                    .tooltip("Otherwise the target stays halted after a detach.")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        let checked = *checked;
                        this.set_debug_option(cx, |o| o.resume_on_detach = checked);
                    })),
            )
            .child(
                Switch::new("keep-serving")
                    .small()
                    .checked(options.keep_serving)
                    .disabled(locked)
                    .label("Keep serving after GDB disconnects")
                    .tooltip("Wait for the next GDB session, for example when your IDE restarts debugging. In \"Program, then debug\" mode each new session starts from reset.")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        let checked = *checked;
                        this.set_debug_option(cx, |o| o.keep_serving = checked);
                    })),
            );

        let port_valid = self.typed_port(cx).is_some();
        let blocker = self.debug_blocker(cx);
        let button = if debugging {
            Button::new("debug")
                .danger()
                .icon(Icon::new(IconName::Close))
                .label("Stop debugging")
                .disabled(self.running.as_ref().is_some_and(|r| r.stopping))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_debug(window, cx)))
        } else {
            Button::new("debug")
                .primary()
                .icon(Icon::new(IconName::ArrowRight))
                .label("Start debugging")
                .disabled(busy || blocker.is_some())
                .on_click(cx.listener(|this, _, window, cx| this.toggle_debug(window, cx)))
        };

        let status: AnyElement = if debugging {
            self.render_debug_status(cx)
        } else if let Some(blocker) = blocker {
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(blocker)
                .into_any_element()
        } else {
            div().into_any_element()
        };

        section(
            2,
            "Debug server",
            "The Soul Injector becomes a GDB server on this computer.",
            cx,
        )
        .child(
            v_flex()
                .gap_3()
                .child(field("Start", Some(mode_help), h_flex().child(mode), cx))
                .child(field(
                    "Power",
                    None,
                    Switch::new("measure-power")
                        .small()
                        .checked(options.measure_power)
                        .disabled(locked)
                        .label("Measure power during the session")
                        .tooltip("Records the target's current while you debug. Starts and stops with the debug server.")
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            let checked = *checked;
                            this.set_debug_option(cx, |o| o.measure_power = checked);
                        })),
                    cx,
                ))
                .child(field(
                    "GDB port",
                    None,
                    v_flex()
                        .gap_1()
                        .child(
                            div().w(px(120.)).child(
                                Input::new(&self.port_input).id("gdb-port").disabled(locked),
                            ),
                        )
                        .child(if port_valid {
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("GDB connects to 127.0.0.1 on this port.")
                                .into_any_element()
                        } else {
                            notice(Level::Warning, "Enter a port from 1 to 65535.", cx)
                                .text_xs()
                                .into_any_element()
                        }),
                    cx,
                ))
                .child(field("Options", None, switches, cx))
                .child(
                    h_flex()
                        .gap_4()
                        .pt_1()
                        .child(button)
                        .child(div().flex_1().min_w_0().child(status)),
                )
                .when(options.measure_power, |this| {
                    this.child(
                        v_flex()
                            .gap_3()
                            .pt_3()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child("Power during the session"),
                            )
                            .child(self.power.clone()),
                    )
                }),
        )
        .into_any_element()
    }

    /// Shown in Debug mode for targets that cannot be debugged.
    fn render_esp32_cannot_debug(&mut self, cx: &mut Context<Self>) -> AnyElement {
        section(
            2,
            "Debugging is not available",
            "ESP32 targets are programmed over UART. Debugging through the Soul Injector needs an ARM Cortex-M target on SWD.",
            cx,
        )
        .child(
            h_flex().child(
                Button::new("switch-to-program")
                    .outline()
                    .icon(IconName::ArrowRight)
                    .label("Program this target instead")
                    .on_click(cx.listener(|this, _, _, cx| this.set_mode(Mode::Program, cx))),
            ),
        )
        .into_any_element()
    }

    /// The Power page: wiring, capture and the chart.
    fn render_power_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        section(
            None,
            "Power",
            "The Soul Injector measures the current your target draws from its supply, from nanoamps to amps.",
            cx,
        )
        .child(notice(
            Level::Info,
            "Wiring: feed VPP input (connector pin 11) from 3.3 V (pin 7) or a 1.8 to 3.6 V bench supply, and power the target from VPP output (pin 9). The target is powered whenever VPP input is.",
            cx,
        ))
        .child(self.power.clone())
        .into_any_element()
    }

    /// Why debugging cannot start, if it cannot. Lasting reasons come first.
    pub fn debug_blocker(&self, cx: &gpui_kit::App) -> Option<SharedString> {
        if self.family() == Some(Family::Esp32) {
            return Some(
                "ESP32 targets can be programmed but not debugged through the Soul Injector."
                    .into(),
            );
        }
        if self.typed_port(cx).is_none() {
            return Some("Enter a GDB port from 1 to 65535.".into());
        }
        if let Some(blocker) = self.device_blocker() {
            return Some(blocker.into());
        }
        if self.state.profile().debug.mode == DebugMode::ProgramThenDebug {
            match &self.readiness {
                Readiness::Ready(_) => {}
                Readiness::Checking => return Some("Checking files…".into()),
                Readiness::Problem(problem) => return Some(problem.clone()),
            }
        }
        None
    }

    fn render_debug_status(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (color, text): (gpui_kit::Hsla, SharedString) = match &self.gdb_state {
            GdbState::Connected(_) => (theme.success, "GDB is connected".into()),
            GdbState::Listening(address) => (
                theme.success,
                format!("Waiting for GDB on {address}").into(),
            ),
            GdbState::Idle => (
                theme.warning,
                self.running
                    .as_ref()
                    .map(|r| r.step.clone())
                    .unwrap_or_default(),
            ),
        };
        h_flex()
            .gap_2()
            .text_sm()
            .child(dot(color))
            .child(div().flex_1().min_w_0().child(text))
            .into_any_element()
    }

    fn render_debugger(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let client = self.client;
        let tab_index = Client::ALL.iter().position(|c| *c == client).unwrap_or(0);
        let serving = matches!(
            self.gdb_state,
            GdbState::Listening(_) | GdbState::Connected(_)
        );

        let gdb_row = if self.gdbs.is_empty() && self.state.settings.gdb.is_none() {
            let text: SharedString = if self.gdbs_scanned {
                "No ARM GDB found. Install the Arm GNU Toolchain (arm-none-eabi-gdb) or gdb-multiarch, or choose one."
                    .into()
            } else {
                "Looking for GDB…".into()
            };
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(notice(Level::Warning, text, cx)),
                )
                .when(self.gdbs_scanned, |this| {
                    this.child(
                        Button::new("download-toolchain")
                            .ghost()
                            .small()
                            .icon(IconName::ExternalLink)
                            .label("Download")
                            .on_click(|_, _, cx| cx.open_url(TOOLCHAIN_URL)),
                    )
                })
                .child(
                    Button::new("browse-gdb")
                        .outline()
                        .label("Browse…")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.browse(PathField::Gdb, window, cx)
                        })),
                )
                .into_any_element()
        } else {
            h_flex()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Select::new(&self.gdb_select)),
                )
                .child(
                    Button::new("browse-gdb")
                        .outline()
                        .label("Browse…")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.browse(PathField::Gdb, window, cx)
                        })),
                )
                .into_any_element()
        };
        let gdb_help = self.gdb_version().map(|v| SharedString::from(v.to_owned()));

        let tabs = TabBar::new("clients")
            .underline()
            .small()
            .selected_index(tab_index)
            .children(Client::ALL.iter().map(|c| Tab::new().label(c.label())))
            .on_click(cx.listener(|this, index: &usize, _, cx| {
                this.client = Client::ALL[*index];
                cx.notify();
            }));

        let actions = h_flex()
            .gap_2()
            .child(
                Button::new("copy-snippet")
                    .outline()
                    .small()
                    .icon(IconName::Copy)
                    .label("Copy")
                    .on_click(cx.listener(|this, _, window, cx| this.copy_snippet(window, cx))),
            )
            .when(client == Client::Gdb && !cfg!(windows), |this| {
                this.child(
                    Button::new("open-terminal")
                        .outline()
                        .small()
                        .icon(IconName::SquareTerminal)
                        .label("Open in terminal")
                        .disabled(!serving)
                        .tooltip(if serving {
                            "Open GDB connected to the debug server"
                        } else {
                            "Start the debug server first"
                        })
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_gdb_terminal(window, cx)),
                        ),
                )
            });

        let needs_elf = self.state.profile().elf.is_none() && self.family() != Some(Family::Esp32);

        section(
            3,
            "Connect your debugger",
            "Use GDB directly or from your IDE while the debug server runs.",
            cx,
        )
        .child(
            v_flex()
                .gap_3()
                .child(field("GDB", gdb_help, gdb_row, cx))
                .child(
                    v_flex()
                        .gap_2()
                        .child(tabs)
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(client.hint()),
                        )
                        .child(code_block("snippet", self.snippet(client), cx))
                        .when(needs_elf, |this| {
                            this.child(notice(
                                Level::Info,
                                "Choose the debug symbols (ELF) above to see source code in the debugger.",
                                cx,
                            ))
                        })
                        .child(actions),
                ),
        )
        .into_any_element()
    }

    fn render_programming(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.is_busy();
        let ready = matches!(self.readiness, Readiness::Ready(_));
        let blocked = busy || !ready || self.device_blocker().is_some();
        let auto = self.state.profile().auto_run;

        let hint: Option<SharedString> = if let Some(blocker) = self.device_blocker() {
            Some(blocker.into())
        } else if !ready {
            Some("Complete the target above first.".into())
        } else {
            None
        };

        section(
            2,
            "Program",
            "The Soul Injector stores the job, so it can also program targets on its own, without this computer.",
            cx,
        )
        .child(
            v_flex()
                .gap_3()
                .child(
                    Switch::new("auto-run")
                        .small()
                        .checked(auto)
                        .disabled(busy)
                        .label("Program every target plugged into the Soul Injector")
                        .tooltip("Applies when the job is sent. Debug sessions always turn this off, so programming does not restart under the debugger.")
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.set_auto_run(*checked, cx)
                        })),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("program")
                                .outline()
                                .icon(IconName::ArrowRight)
                                .label("Program now")
                                .disabled(blocked)
                                .on_click(cx.listener(|this, _, window, cx| this.program(window, cx))),
                        )
                        .child(
                            Button::new("push")
                                .ghost()
                                .label("Send job only")
                                .tooltip("Store the job and images on the Soul Injector without programming.")
                                .disabled(blocked)
                                .on_click(cx.listener(|this, _, window, cx| this.push(window, cx))),
                        )
                        .when_some(hint, |this, hint| {
                            this.child(
                                div()
                                    .pl_2()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(hint),
                            )
                        }),
                ),
        )
        .into_any_element()
    }
}

fn kib(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}
