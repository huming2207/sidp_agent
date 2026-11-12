//! The main window driven like a user would: clicks and typing on the real
//! components in a headless window, with results checked on the view.
//!
//! These need no device and never open one: the view they build does not
//! watch USB ports. They run against a state file in a temporary folder,
//! never the user's settings.
use gpui_kit::test::{TestAppContextExt, TestEventExt, TestWindowExt};
use gpui_kit::{
    AnyWindowHandle, AppContext, Bounds, Entity, Modifiers, MouseButton, Point, ScrollDelta,
    TestAppContext, Window, WindowBounds, WindowOptions, point, px, size,
};
use sidp_desktop::{
    profile::{DEFAULT_GDB_PORT, DebugMode, Mode, State},
    snippets::Client,
    ui::app::{AppView, Readiness},
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(10);

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/fixtures")
        .join(name)
}

/// Opens the window tall enough that every section is on screen.
fn open(cx: &mut TestAppContext, state_file: &Path) -> (AnyWindowHandle, Entity<AppView>) {
    open_sized(cx, state_file, 2400.)
}

/// Opens the window `height` pixels tall.
fn open_sized(
    cx: &mut TestAppContext,
    state_file: &Path,
    height: f32,
) -> (AnyWindowHandle, Entity<AppView>) {
    let state_file = state_file.to_path_buf();
    cx.update(|cx| {
        gpui_kit::init(cx);
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1200.), px(height)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| AppView::open(Some(state_file), window, cx)),
        )
        .expect("open the window")
    })
}

/// Replaces an input's text the way a user would: click, select all, type.
fn replace(window: &mut Window, id: &str, text: &str, cx: &mut gpui_kit::App) {
    window.click(id.to_owned(), cx);
    window.press(
        if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        },
        cx,
    );
    window.input(text, cx);
}

/// Clicks a mode card, as a user would.
fn choose_mode(window: &mut Window, mode: Mode, cx: &mut gpui_kit::App) {
    let index = Mode::ALL.iter().position(|m| *m == mode).unwrap();
    window.click(("mode", index), cx);
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[gpui_kit::test]
async fn filling_in_the_target_makes_a_ready_job_and_saves_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state_file = dir.path().join("state.json");
    let firmware = dir.path().join("app.bin");
    std::fs::write(&firmware, [0x55; 2048]).unwrap();
    let (window, view) = open(cx, &state_file);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        replace(window, "profile-name", "Sensor board", cx);
        replace(
            window,
            "target",
            &path_text(&fixture("stm32wle5cc.yaml")),
            cx,
        );
        replace(window, "image-firmware.bin", &path_text(&firmware), cx);
    })
    .unwrap();

    cx.wait_for(window, WAIT, |_, cx| {
        matches!(view.read(cx).readiness(), Readiness::Ready(_))
    })
    .await;
    cx.update(|cx| {
        let view = view.read(cx);
        let Readiness::Ready(job) = view.readiness() else {
            unreachable!()
        };
        assert_eq!(job.name, "STM32WLE5CC");
        assert_eq!(job.images, [("firmware.bin".to_owned(), 2048)]);
        assert_eq!(view.state().profile().name, "Sensor board");
    });

    // Edits reach the state file shortly after typing stops.
    cx.wait_for(window, WAIT, |_, _| {
        State::load(&state_file).is_ok_and(|s| s.profile().name == "Sensor board")
    })
    .await;
    let saved = State::load(&state_file).unwrap();
    assert_eq!(saved.profile().target, Some(fixture("stm32wle5cc.yaml")));
    assert_eq!(saved.profile().images["firmware.bin"], firmware);
}

/// Clicks a Browse button and picks `path` in the file chooser.
fn browse(cx: &mut TestAppContext, window: AnyWindowHandle, button: &str, path: &Path) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(button.to_owned(), cx);
    })
    .unwrap();
    cx.run_until_parked();
    let path = path.to_path_buf();
    cx.simulate_path_prompt_response(|_| Some(vec![path]));
    cx.run_until_parked();
}

#[gpui_kit::test]
async fn browsing_fills_in_the_profile_after_a_wrong_choice(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let firmware = dir.path().join("app.bin");
    std::fs::write(&firmware, [0x55; 1024]).unwrap();
    let elf = dir.path().join("app.elf");
    std::fs::write(&elf, b"\x7fELF").unwrap();
    let (window, view) = open(cx, &dir.path().join("state.json"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        choose_mode(window, Mode::Debug, cx);
    })
    .unwrap();

    // A file that is not a target description first, then the right one.
    browse(cx, window, "target-browse", &fixture("pre_prog.yaml"));
    cx.wait_for(window, WAIT, |_, cx| {
        matches!(view.read(cx).readiness(), Readiness::Problem(_))
    })
    .await;
    browse(cx, window, "image-firmware.bin-browse", &firmware);
    browse(cx, window, "target-browse", &fixture("stm32wle5cc.yaml"));
    browse(cx, window, "elf-browse", &elf);

    cx.wait_for(window, WAIT, |_, cx| {
        matches!(view.read(cx).readiness(), Readiness::Ready(_))
    })
    .await;
    cx.update(|cx| {
        let profile = view.read(cx).state().profile();
        assert_eq!(profile.target, Some(fixture("stm32wle5cc.yaml")));
        assert_eq!(profile.images["firmware.bin"], firmware);
        assert_eq!(profile.elf, Some(elf));
    });
}

#[gpui_kit::test]
async fn a_missing_image_is_explained(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open(cx, &dir.path().join("state.json"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        replace(
            window,
            "target",
            &path_text(&fixture("stm32wle5cc.yaml")),
            cx,
        );
    })
    .unwrap();
    cx.wait_for(window, WAIT, |_, cx| match view.read(cx).readiness() {
        Readiness::Problem(problem) => problem.as_ref() == "Choose the firmware to program.",
        _ => false,
    })
    .await;
    cx.update(|cx| {
        // Program-then-debug needs the job, so debugging cannot start.
        assert!(view.read(cx).debug_blocker(cx).is_some());
    });
}

#[gpui_kit::test]
async fn profiles_are_added_and_deleted_after_confirmation(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open(cx, &dir.path().join("state.json"));

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("add-profile", cx);
    })
    .unwrap();
    cx.update(|cx| {
        let state = view.read(cx).state();
        assert_eq!(state.profiles.len(), 2);
        assert_eq!(state.selected, 1);
        assert_eq!(state.profile().name, "New target");
    });

    // The new profile's fields are empty, and typing edits that profile only.
    cx.update_window(window, |_, window, cx| {
        replace(window, "profile-name", "Gateway", cx);
        window.click("delete-profile", cx);
    })
    .unwrap();
    cx.update(|cx| assert_eq!(view.read(cx).state().profiles.len(), 2));
    cx.wait_for(window, WAIT, |window, _| window.try_find("ok").is_some())
        .await;
    cx.update_window(window, |_, window, cx| window.click("ok", cx))
        .unwrap();
    cx.update(|cx| {
        let state = view.read(cx).state();
        assert_eq!(state.profiles.len(), 1);
        assert_eq!(state.profile().name, "My target");
    });
}

#[gpui_kit::test]
async fn an_invalid_gdb_port_is_not_used(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open(cx, &dir.path().join("state.json"));

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        choose_mode(window, Mode::Debug, cx);
        replace(window, "gdb-port", "70000", cx);
    })
    .unwrap();
    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.typed_port(cx), None);
        assert_eq!(view.state().profile().debug.port, DEFAULT_GDB_PORT);
        let blocker = view.debug_blocker(cx).unwrap_or_default();
        assert!(blocker.contains("GDB port"), "{blocker}");
    });

    cx.update_window(window, |_, window, cx| {
        replace(window, "gdb-port", "4444", cx);
    })
    .unwrap();
    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.state().profile().debug.port, 4444);
        assert!(view.snippet(Client::Gdb).contains("127.0.0.1:4444"));
    });
}

#[gpui_kit::test]
async fn the_debugger_tabs_switch_settings(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open(cx, &dir.path().join("state.json"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        choose_mode(window, Mode::Debug, cx);
    })
    .unwrap();
    for (index, client) in Client::ALL.into_iter().enumerate().rev() {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.within("clients").click(index, cx);
        })
        .unwrap();
        cx.update(|cx| assert_eq!(view.read(cx).client(), client));
    }
}

#[gpui_kit::test]
async fn esp32_targets_ask_for_each_image_and_cannot_be_debugged(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("esp32.yaml");
    std::fs::write(
        &target,
        "family: esp32\n\
         variants:\n\
         \x20 - name: s3\n\
         \x20   chip: esp32s3\n\
         \x20   images:\n\
         \x20     - { path: /data/bootloader.bin, offset: 0x0 }\n\
         \x20     - { path: /data/app.bin, offset: 0x10000 }\n\
         \x20 - name: c3\n\
         \x20   chip: esp32c3\n\
         \x20   images:\n\
         \x20     - { path: /data/app.bin, offset: 0x10000 }\n",
    )
    .unwrap();
    let (window, view) = open(cx, &dir.path().join("state.json"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        replace(window, "target", &path_text(&target), cx);
    })
    .unwrap();

    // Two variants: the user has to choose, and no image is asked for yet.
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("variant").is_some()
    })
    .await;
    cx.update(|cx| assert!(view.read(cx).image_names().is_empty()));

    // Choose a variant with the keyboard, as a user could.
    cx.update_window(window, |_, window, cx| {
        window.within("variant").click("input", cx);
        window.press("down", cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.wait_for(window, WAIT, |_, cx| {
        view.read(cx).state().profile().variant.is_some()
    })
    .await;
    let expected: &[&str] =
        cx.update(
            |cx| match view.read(cx).state().profile().variant.as_deref() {
                Some("s3") => &["bootloader.bin", "app.bin"][..],
                Some("c3") => &["app.bin"],
                other => panic!("unexpected variant {other:?}"),
            },
        );
    // The image fields follow the chosen variant.
    cx.wait_for(window, WAIT, |_, cx| {
        view.read(cx).image_names() == expected
    })
    .await;
    cx.update(|cx| {
        let view = view.read(cx);
        // ESP32 targets cannot be debugged, whatever else is missing.
        let blocker = view.debug_blocker(cx).unwrap_or_default();
        assert!(blocker.contains("ESP32"), "{blocker}");
    });
}

#[gpui_kit::test]
async fn each_mode_shows_its_own_page(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state_file = dir.path().join("state.json");
    let (window, view) = open(cx, &state_file);
    // (mode, a control only its page has)
    let pages = [
        (Mode::Program, "program"),
        (Mode::Debug, "debug"),
        (Mode::Power, "power-start"),
    ];
    for (mode, _) in [Mode::Debug, Mode::Power, Mode::Program]
        .into_iter()
        .map(|m| pages.into_iter().find(|(p, _)| *p == m).unwrap())
    {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            choose_mode(window, mode, cx);
            for (other, other_control) in pages {
                assert_eq!(
                    window.try_find(other_control).is_some(),
                    other == mode,
                    "{other_control} on the {mode:?} page"
                );
            }
        })
        .unwrap();
        cx.update(|cx| assert_eq!(view.read(cx).mode(), mode));
    }
    // The page is remembered.
    cx.update_window(window, |_, window, cx| choose_mode(window, Mode::Power, cx))
        .unwrap();
    cx.wait_for(window, WAIT, |_, _| {
        State::load(&state_file).is_ok_and(|s| s.mode == Mode::Power)
    })
    .await;
}

/// Runs the UI until `done`, in real time: a capture produces samples on
/// its own thread.
fn run_until(
    cx: &mut TestAppContext,
    view: &Entity<AppView>,
    mut done: impl FnMut(&sidp_desktop::ui::power::PowerView) -> bool,
) {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        // Let the capture thread work, then let the view's timer collect.
        std::thread::sleep(Duration::from_millis(20));
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        if cx.update(|cx| done(view.read(cx).power().read(cx))) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "timed out");
    }
}

#[gpui_kit::test]
async fn the_simulated_preview_records_until_stopped(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open(cx, &dir.path().join("state.json"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        choose_mode(window, Mode::Power, cx);
        window.within("power-source").click(1usize, cx);
        window.click("power-start", cx);
    })
    .unwrap();
    run_until(cx, &view, |power| power.recording().len() >= 300);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("power-start", cx);
    })
    .unwrap();
    run_until(cx, &view, |power| !power.is_capturing());
    cx.update(|cx| {
        let power = view.read(cx).power().read(cx);
        assert_eq!(power.error(), None);
        let recording = power.recording();
        let stats = recording.stats(0.0, recording.duration()).unwrap();
        assert!(stats.average > 0.0 && stats.max >= stats.min);
    });
}

#[gpui_kit::test]
async fn recording_without_a_soul_injector_says_what_is_missing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open(cx, &dir.path().join("state.json"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        choose_mode(window, Mode::Power, cx);
        window.click("power-start", cx);
    })
    .unwrap();
    run_until(cx, &view, |power| !power.is_capturing());
    cx.update(|cx| {
        let error = view
            .read(cx)
            .power()
            .read(cx)
            .error()
            .unwrap_or_default()
            .to_owned();
        // The tests never open a device, so none is connected.
        assert!(error.contains("Connect a Soul Injector"), "{error}");
    });
}

#[gpui_kit::test]
async fn power_can_be_measured_while_debugging(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open(cx, &dir.path().join("state.json"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        choose_mode(window, Mode::Debug, cx);
        assert!(window.try_find("power-start").is_none());
        window.click("measure-power", cx);
        window.render_frame(cx);
        // The compact power view appears on the Debug page.
        assert!(window.try_find("power-start").is_some());
        assert!(window.try_find("power-rate").is_none());
    })
    .unwrap();
    cx.update(|cx| assert!(view.read(cx).state().profile().debug.measure_power));
}

#[gpui_kit::test]
async fn a_debug_session_leaves_a_recording_started_by_hand(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open(cx, &dir.path().join("state.json"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        choose_mode(window, Mode::Power, cx);
        window.within("power-source").click(1usize, cx);
        window.click("power-start", cx);
    })
    .unwrap();
    run_until(cx, &view, |power| power.recording().len() >= 50);

    // The test view sees no Soul Injector, so the session ends at once.
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |this, cx| {
            this.set_debug_option(cx, |o| {
                o.mode = DebugMode::Attach;
                o.measure_power = true;
            });
            this.toggle_debug(window, cx);
        })
    })
    .unwrap();
    cx.run_until_parked();
    assert!(cx.update(|cx| !view.read(cx).is_busy()));
    let before = cx.update(|cx| view.read(cx).power().read(cx).recording().len());
    run_until(cx, &view, |power| {
        !power.is_capturing() || power.recording().len() >= before + 100
    });
    cx.update(|cx| assert!(view.read(cx).power().read(cx).is_capturing()));
}

#[gpui_kit::test]
async fn the_chart_zooms_pans_and_selects_like_the_power_profiler(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    // Short enough that the page scrolls, as in a normal window: the chart
    // must still get the wheel.
    let (window, view) = open_sized(cx, &dir.path().join("state.json"), 900.);
    let power = cx.update(|cx| view.read(cx).power().clone());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        choose_mode(window, Mode::Power, cx);
    })
    .unwrap();
    cx.update(|cx| {
        power.update(cx, |power, cx| {
            power.show_recording(sidp_desktop::power::simulated_recording(1000, 30.0), cx)
        })
    });
    let shown = |cx: &mut TestAppContext| cx.update(|cx| power.read(cx).window());
    assert_eq!(shown(cx), (20.0, 30.0));

    // The page does scroll when the wheel is elsewhere...
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let before = window.find("power-chart").bounds();
        window.scroll("power-start", ScrollDelta::Lines(point(0., -2.)), cx);
        window.render_frame(cx);
        let after = window.find("power-chart").bounds();
        assert!(after.origin.y < before.origin.y, "the page should scroll");
        window.scroll("power-start", ScrollDelta::Lines(point(0., 2.)), cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(shown(cx), (20.0, 30.0));

    // ...but over the chart, scrolling up zooms in around the pointer (the
    // centre) and leaves the page where it is.
    cx.update_window(window, |_, window, cx| {
        let before = window.find("power-chart").bounds();
        window.scroll("power-chart", ScrollDelta::Lines(point(0., 3.)), cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("power-chart").bounds(),
            before,
            "the page moved"
        );
    })
    .unwrap();
    let (from, to) = shown(cx);
    assert!(
        (to - from - 10.0 / 1.2f64.powi(3)).abs() < 0.05,
        "{from}..{to}"
    );
    assert!(((from + to) / 2.0 - 25.0).abs() < 0.1, "{from}..{to}");

    // Scrolling down far enough shows the whole recording.
    cx.update_window(window, |_, window, cx| {
        window.scroll("power-chart", ScrollDelta::Lines(point(0., -40.)), cx);
    })
    .unwrap();
    assert_eq!(shown(cx), (0.0, 30.0));

    // Dragging to the right moves back in time and stops following.
    cx.update_window(window, |_, window, cx| {
        window.within("power-span").click(3usize, cx); // 10 s
        window.render_frame(cx);
        let chart = window.find("power-chart").bounds();
        let start = chart.center();
        window.pointer_down(start, MouseButton::Left, cx);
        window.pointer_move(
            start + point(chart.size.width / 4., px(0.)),
            Some(MouseButton::Left),
            cx,
        );
        window.pointer_up(
            start + point(chart.size.width / 4., px(0.)),
            MouseButton::Left,
            cx,
        );
    })
    .unwrap();
    let (from, to) = shown(cx);
    assert!((to - from - 10.0).abs() < 1e-6, "{from}..{to}");
    assert!((from - 17.5).abs() < 0.1, "{from}..{to}");
    cx.update(|cx| assert_eq!(power.read(cx).selection(), None));

    // Shift-drag selects instead.
    cx.update_window(window, |_, window, cx| {
        let chart = window.find("power-chart").bounds();
        let start = chart.center();
        let end = start + point(chart.size.width / 5., px(0.));
        window.change_modifiers(Modifiers::shift(), cx);
        window.pointer_down(start, MouseButton::Left, cx);
        window.pointer_move(end, Some(MouseButton::Left), cx);
        window.pointer_up(end, MouseButton::Left, cx);
        window.change_modifiers(Modifiers::default(), cx);
    })
    .unwrap();
    let (window_from, window_to) = shown(cx);
    assert_eq!(
        (window_from, window_to),
        (from, to),
        "selecting does not pan"
    );
    let selection = cx
        .update(|cx| power.read(cx).selection())
        .expect("a selection");
    assert!(
        (selection.1 - selection.0 - 2.0).abs() < 0.1,
        "{selection:?}"
    );
}
