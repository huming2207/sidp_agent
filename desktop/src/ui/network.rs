//! The network sheet: how the Soul Injector connects over WiFi, set up over USB.
use super::app::{Activity, AppView};
use crate::{
    ports,
    worker::{NetworkSnapshot, Operation},
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    input::Input,
    notification::Notification,
    switch::Switch,
    v_flex,
};
use gpui_kit::{
    Context, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use sidp_agent::{
    login,
    manage::proto::{NetConfig, NetState},
    transport::{self, WEBSOCKET_PORT},
};

impl AppView {
    /// Opens the network sheet and reads the device's settings.
    pub fn open_network(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_busy() {
            self.network = None;
            self.start(Activity::Network, Operation::ReadNetwork, cx);
        }
        let view = cx.entity();
        window.open_sheet(cx, move |sheet, _, cx| {
            let this = view.read(cx);
            let theme = cx.theme();
            let busy = this.is_busy();
            // The device takes new settings only over USB.
            let on_usb = this
                .selected_port()
                .is_some_and(|p| !ports::is_network(&p.path));
            let help = |text: SharedString| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(text)
            };
            let row = |label: &'static str, control: gpui_kit::AnyElement, text: SharedString| {
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child(label))
                    .child(control)
                    .child(help(text))
            };

            let config = this.network.as_ref().map(|n| &n.config);
            let password_help = if config.is_some_and(|c| c.has_wifi_password) {
                "A password is stored. Leave this empty to keep it."
            } else {
                "Leave empty for an open network."
            };
            let device_password_help = if config.is_some_and(|c| c.has_password) {
                "Needed to use the Soul Injector over the network. One is set; leave this empty to keep it."
            } else {
                "Needed to use the Soul Injector over the network. Only a check value derived from it is stored on the device."
            };
            let toggle = view.clone();
            let save = view.clone();
            sheet
                .title("Network")
                .size(px(420.))
                .child(
                    v_flex()
                        .gap_5()
                        .child(help(
                            "The Soul Injector joins this WiFi network and connects to this computer. \
                             Settings are stored over USB and used after the Soul Injector restarts."
                                .into(),
                        ))
                        .child(
                            Switch::new("network-enabled")
                                .checked(this.network_enabled)
                                .label("Connect over WiFi")
                                .on_click(move |checked: &bool, _, cx| {
                                    toggle.update(cx, |this, cx| {
                                        this.network_enabled = *checked;
                                        cx.notify();
                                    })
                                }),
                        )
                        .child(row(
                            "WiFi network",
                            Input::new(&this.ssid_input).into_any_element(),
                            "The network name (SSID).".into(),
                        ))
                        .child(row(
                            "WiFi password",
                            Input::new(&this.wifi_password_input).mask_toggle().into_any_element(),
                            password_help.into(),
                        ))
                        .child(row(
                            "Device password",
                            Input::new(&this.device_password_input).mask_toggle().into_any_element(),
                            device_password_help.into(),
                        ))
                        .child(row(
                            "Server",
                            Input::new(&this.server_input).into_any_element(),
                            format!(
                                "Where the Soul Injector connects: this computer by its name on the \
                                 LAN (HOSTNAME.local), port {WEBSOCKET_PORT} unless the address gives \
                                 one. It must match the port in Settings, and the firewall must allow \
                                 incoming connections on it."
                            )
                            .into(),
                        ))
                        .child(help(network_state(this.network.as_ref())))
                        .when(!on_usb && this.selected_port().is_some(), |this| {
                            this.child(help("Settings can only be changed over USB.".into()))
                        })
                        .child(
                            Button::new("save-network")
                                .primary()
                                .label("Save to the Soul Injector")
                                .loading(busy && this.running_activity() == Some(Activity::Network))
                                .disabled(busy || !on_usb)
                                .on_click(move |_, window, cx| {
                                    save.update(cx, |this, cx| this.save_network(window, cx))
                                }),
                        ),
                )
        });
    }

    /// Fills the sheet from what the device reported.
    pub(super) fn show_network(
        &mut self,
        snapshot: NetworkSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let config = &snapshot.config;
        let server = if config.server_url.is_empty() {
            default_server(self.state.settings.listen_port)
        } else {
            config.server_url.clone()
        };
        let ssid = config.wifi_ssid.clone();
        self.ssid_input
            .update(cx, |input, cx| input.set_value(ssid, window, cx));
        self.server_input
            .update(cx, |input, cx| input.set_value(server, window, cx));
        self.wifi_password_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.device_password_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        // Nothing stored yet: offer to turn it on.
        self.network_enabled = config.enabled || config.wifi_ssid.is_empty();
        self.network = Some(snapshot);
    }

    fn save_network(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ssid = self.ssid_input.read(cx).value().trim().to_owned();
        let server = self.server_input.read(cx).value().trim().to_owned();
        let password = self.wifi_password_input.read(cx).value().to_string();
        let device_password = self.device_password_input.read(cx).value().to_string();
        let stored = self.network.as_ref().map(|n| &n.config);
        let has_password = stored.is_some_and(|c| c.has_wifi_password);
        let has_device_password = stored.is_some_and(|c| c.has_password);
        let problem = if !self.network_enabled {
            None
        } else if ssid.is_empty() {
            Some("Enter the WiFi network name.")
        } else if !server.starts_with("ws://") {
            Some("The server address must start with ws://")
        } else if device_password.is_empty() && !has_device_password {
            Some("Choose a device password.")
        } else {
            None
        };
        if let Some(problem) = problem {
            window.push_notification(Notification::error(problem), cx);
            return;
        }
        let verifier = if device_password.is_empty() {
            None
        } else {
            match login::make_verifier(&device_password) {
                Ok(verifier) => Some(verifier),
                Err(error) => {
                    window.push_notification(Notification::error(error.to_string()), cx);
                    return;
                }
            }
        };
        let config = NetConfig {
            enabled: self.network_enabled,
            wifi_ssid: ssid,
            keep_wifi_password: password.is_empty() && has_password,
            wifi_password: password,
            server_url: server,
            keep_password: verifier.is_none(),
            password_salt: verifier
                .as_ref()
                .map(|v| v.salt.clone())
                .unwrap_or_default(),
            password_verifier: verifier.map(|v| v.verifier).unwrap_or_default(),
            ..Default::default()
        };
        self.start(Activity::Network, Operation::SetNetwork(config), cx);
    }
}

/// This computer's mDNS name and the app's port, for a Soul Injector not set up yet.
fn default_server(port: u16) -> String {
    let url = transport::default_server_url();
    if port == WEBSOCKET_PORT {
        url
    } else {
        format!("{}:{port}/", url.trim_end_matches('/'))
    }
}

fn network_state(network: Option<&NetworkSnapshot>) -> SharedString {
    let Some(network) = network else {
        return "Reading the Soul Injector's settings…".into();
    };
    let status = &network.status;
    let id = if status.device_id.is_empty() {
        String::new()
    } else {
        format!(" Device ID {}.", status.device_id)
    };
    let state = match status.state() {
        NetState::Off if network.config.enabled => {
            "Not connected yet: restart the Soul Injector to use these settings."
        }
        NetState::Off => "WiFi is off.",
        NetState::WifiConnecting => "Joining the WiFi network…",
        NetState::ServerConnecting => "On WiFi, trying to reach the server.",
        NetState::Connected => "Connected to the server.",
    };
    if status.ip.is_empty() {
        format!("{state}{id}").into()
    } else {
        format!(
            "{state} Address {}, signal {} dBm.{id}",
            status.ip, status.rssi
        )
        .into()
    }
}
