//! Network setup commands: how the device connects over WiFi.
use crate::commands::{Connection, read_line};
use sidp_agent::{
    login,
    manage::proto::{NetConfig, NetState},
    transport::{self, WEBSOCKET_PORT},
};
use std::error::Error;

type CommandResult = Result<(), Box<dyn Error>>;

/// `network show`.
pub fn show(connection: &Connection) -> CommandResult {
    let mut session = connection.open()?;
    let config = session.net_config_get()?;
    let status = session.net_status()?;
    println!("Networking: {}", if config.enabled { "on" } else { "off" });
    if !status.device_id.is_empty() {
        println!("Device ID:  {}", status.device_id);
    }
    println!("WiFi:       {}", empty_as_none(&config.wifi_ssid));
    println!(
        "Password:   {}",
        if config.has_wifi_password {
            "stored"
        } else {
            "none"
        }
    );
    println!("Server:     {}", empty_as_none(&config.server_url));
    let state = match status.state() {
        NetState::Off if config.enabled => "off until the Soul Injector restarts",
        NetState::Off => "off",
        NetState::WifiConnecting => "joining WiFi",
        NetState::ServerConnecting => "on WiFi, connecting to the server",
        NetState::Connected => "connected to the server",
    };
    println!("State:      {state}");
    if !status.ip.is_empty() {
        println!("Address:    {} ({} dBm)", status.ip, status.rssi);
    }
    if status.wifi_reason != 0 {
        println!("Last WiFi disconnect reason: {}", status.wifi_reason);
    }
    Ok(())
}

/// A password given on the command line, kept on the device, or asked for.
pub enum Secret {
    /// Use this one.
    Given(String),
    /// Keep the one on the device.
    Keep,
    /// Ask on standard input.
    Ask,
}

impl Secret {
    /// From a command-line value and a keep flag.
    pub fn new(value: Option<String>, keep: bool) -> Self {
        match value {
            _ if keep => Secret::Keep,
            Some(value) => Secret::Given(value),
            None => Secret::Ask,
        }
    }

    /// The password, asking with `prompt` if needed; `None` to keep the stored one.
    fn resolve(self, prompt: &str) -> std::io::Result<Option<String>> {
        match self {
            Secret::Given(value) => Ok(Some(value)),
            Secret::Keep => Ok(None),
            Secret::Ask => read_line(prompt).map(Some),
        }
    }
}

/// `network set`.
pub fn set(
    connection: &Connection,
    ssid: &str,
    server: Option<String>,
    wifi_password: Secret,
    password: Secret,
) -> CommandResult {
    if !connection.is_usb() {
        return Err("network settings can only be changed over USB".into());
    }
    let server_url = match server {
        Some(server) => server,
        None => transport::default_server_url(),
    };
    if !server_url.starts_with("ws://") {
        return Err("--server must start with ws://".into());
    }
    let wifi_password =
        wifi_password.resolve("WiFi password (shown as typed; empty for an open network): ")?;
    let password = password.resolve("Device password for the network (shown as typed): ")?;
    if password.as_deref() == Some("") {
        return Err("the device password must not be empty".into());
    }
    let verifier = password.as_deref().map(login::make_verifier).transpose()?;
    let config = NetConfig {
        enabled: true,
        wifi_ssid: ssid.to_owned(),
        keep_wifi_password: wifi_password.is_none(),
        wifi_password: wifi_password.unwrap_or_default(),
        server_url: server_url.clone(),
        keep_password: verifier.is_none(),
        password_salt: verifier
            .as_ref()
            .map(|v| v.salt.clone())
            .unwrap_or_default(),
        password_verifier: verifier.map(|v| v.verifier).unwrap_or_default(),
        ..Default::default()
    };
    let mut session = connection.open()?;
    session.net_config_set(&config)?;
    let id = session.net_status()?.device_id;
    println!("Stored: WiFi '{ssid}', server {server_url}.");
    println!(
        "Restart the Soul Injector to connect, and run `sidp-agent serve --listen 0.0.0.0:{}` \
         (or the desktop app) here. Then use --port ws://127.0.0.1:{}/{id}.",
        url_port(&server_url),
        url_port(&server_url)
    );
    Ok(())
}

/// `network off`.
pub fn off(connection: &Connection) -> CommandResult {
    if !connection.is_usb() {
        return Err("network settings can only be changed over USB".into());
    }
    let mut session = connection.open()?;
    let mut config = session.net_config_get()?;
    config.enabled = false;
    config.keep_wifi_password = true;
    config.keep_password = true;
    session.net_config_set(&config)?;
    println!("Networking is off from the next restart.");
    Ok(())
}

fn empty_as_none(text: &str) -> &str {
    if text.is_empty() { "not set" } else { text }
}

/// The port in a `ws://HOST[:PORT][/PATH]` URL.
fn url_port(url: &str) -> u16 {
    let authority = url
        .trim_start_matches("ws://")
        .split('/')
        .next()
        .unwrap_or("");
    authority
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(WEBSOCKET_PORT)
}
