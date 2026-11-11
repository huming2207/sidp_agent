//! The main window driven like a user would: clicks and typing on the real
//! components in a headless window, with results checked on the view.
//!
//! These need no device. They run against a state file in a temporary
//! folder, never the user's settings.
use gpui_kit::test::{TestAppContextExt, TestWindowExt};
use gpui_kit::{
    AnyWindowHandle, AppContext, Bounds, Entity, Point, TestAppContext, Window, WindowBounds,
    WindowOptions, px, size,
};
use sidp_desktop::{
    profile::{DEFAULT_GDB_PORT, State},
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
    let state_file = state_file.to_path_buf();
    cx.update(|cx| {
        gpui_kit::init(cx);
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1200.), px(2400.)),
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
        Readiness::Problem(problem) => problem.contains("firmware.bin"),
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
