//! The GPUI interface.
//!
//! [`app`] holds the state and decides what happens; the other modules
//! only draw it. The window uses the system's own title bar and, on macOS,
//! the native menu bar.
mod activity;
pub mod app;
mod assets;
mod main_panel;
mod modes;
pub mod power;
mod settings;
mod sidebar;
mod widgets;

use crate::platform;
use app::AppView;
use gpui_kit::component::{ActiveTheme as _, TitleBar, h_flex, scroll::ScrollableElement, v_flex};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Decorations, Entity, FontWeight,
    InteractiveElement as _, IntoElement, KeyBinding, Menu, MenuItem, ParentElement, Render,
    StatefulInteractiveElement as _, Styled, TitlebarOptions, Window, WindowBounds,
    WindowDecorations, WindowKind, WindowOptions, div, prelude::FluentBuilder as _, px, size,
};

mod menu_actions {
    #![allow(missing_docs)]
    gpui_kit::actions!(soul_agent, [Quit, OpenSettings, ToggleTheme]);
}
use menu_actions::{OpenSettings, Quit, ToggleTheme};

/// Opens the main window and runs until it is closed.
pub fn run() {
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1200.), px(860.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // The system draws the title bar.
                titlebar: Some(TitlebarOptions {
                    title: Some(platform::APP_NAME.into()),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                window_decorations: Some(WindowDecorations::Server),
                window_min_size: Some(size(px(960.), px(620.))),
                kind: WindowKind::Normal,
                app_id: Some("soul-agent".into()),
                ..Default::default()
            };
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                window.set_window_title(platform::APP_NAME);
                cx.new(|cx| AppView::new(window, cx))
            });
            match opened {
                Ok((window, view)) => install_menus(window, view, cx),
                Err(error) => {
                    eprintln!("cannot open the window: {error}");
                    cx.quit();
                    return;
                }
            }
            cx.activate(true);
        });
}

/// Keyboard shortcuts and the macOS menu bar. Linux has no native menu bar
/// in GPUI, so the same commands are also in the sidebar there.
fn install_menus(window: AnyWindowHandle, view: Entity<AppView>, cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    let settings = view.clone();
    cx.on_action(move |_: &OpenSettings, cx| {
        let view = settings.clone();
        let _ = window.update(cx, |_, window, cx| {
            view.update(cx, |this, cx| this.open_settings(window, cx))
        });
    });
    cx.on_action(move |_: &ToggleTheme, cx| {
        let view = view.clone();
        let _ = window.update(cx, |_, window, cx| {
            view.update(cx, |this, cx| this.toggle_theme(window, cx))
        });
    });
    // GPUI resolves "secondary" only with an explicit OS, so spell it out.
    let (quit, settings) = if cfg!(target_os = "macos") {
        ("cmd-q", "cmd-,")
    } else {
        ("ctrl-q", "ctrl-,")
    };
    cx.bind_keys([
        KeyBinding::new(quit, Quit, None),
        KeyBinding::new(settings, OpenSettings, None),
    ]);
    cx.set_menus(vec![
        Menu::new(platform::APP_NAME).items([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::action(format!("Quit {}", platform::APP_NAME), Quit),
        ]),
        Menu::new("View").items([MenuItem::action("Toggle Dark Mode", ToggleTheme)]),
    ]);
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Where the system cannot draw a title bar (GNOME on Wayland), draw
        // a plain one so the window can still be moved and closed.
        let own_title_bar = matches!(window.window_decorations(), Decorations::Client { .. });
        let mode_bar = self.render_mode_bar(cx);
        let sidebar = self.render_sidebar(window, cx);
        let main = self.render_main(window, cx);
        let activity = self.render_activity(window, cx);
        let theme = cx.theme();
        let background = theme.background;
        let canvas = theme.muted.opacity(0.35);
        v_flex()
            .size_full()
            .bg(background)
            .when(own_title_bar, |this| {
                this.child(
                    TitleBar::new().child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(platform::APP_NAME),
                    ),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .items_start()
                    .child(sidebar)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(mode_bar)
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .w_full()
                                    .bg(canvas)
                                    // A plain scroll area: wheel events reach
                                    // the innermost element first, so the
                                    // power chart can zoom instead of the
                                    // page scrolling. (GPUI Kit's Scrollable
                                    // takes the wheel before its children.)
                                    .relative()
                                    .child(
                                        div()
                                            .id("page")
                                            .size_full()
                                            .overflow_y_scroll()
                                            .track_scroll(&self.page_scroll)
                                            .child(main),
                                    )
                                    .vertical_scrollbar(&self.page_scroll),
                            )
                            .child(activity),
                    ),
            )
    }
}
