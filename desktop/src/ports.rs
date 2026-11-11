//! Finds Soul Injectors among the computer's USB serial ports.
use serialport::{SerialPortInfo, SerialPortType, UsbPortInfo};

/// USB product string of the Soul Injector firmware.
pub const PRODUCT: &str = "Soul Injector";

/// One USB serial port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Port {
    /// Path or name to open, e.g. `/dev/ttyACM0`, `/dev/cu.usbmodem1101` or `COM3`.
    pub path: String,
    /// USB product, or a generic description.
    pub product: String,
    /// USB serial number, when reported.
    pub serial: Option<String>,
    /// Whether the port belongs to a Soul Injector.
    pub soul_injector: bool,
}

/// USB serial ports, Soul Injectors first.
pub fn list() -> Result<Vec<Port>, String> {
    let ports = serialport::available_ports().map_err(|e| e.to_string())?;
    Ok(from_infos(ports))
}

fn from_infos(infos: Vec<SerialPortInfo>) -> Vec<Port> {
    let names: Vec<String> = infos.iter().map(|p| p.port_name.clone()).collect();
    let mut ports: Vec<Port> = infos
        .into_iter()
        .filter_map(|info| match info.port_type {
            SerialPortType::UsbPort(usb) => Some(port(info.port_name, usb)),
            _ => None,
        })
        // macOS lists each device twice; /dev/cu.* is the one to open.
        .filter(|port| {
            port.path
                .strip_prefix("/dev/tty.")
                .is_none_or(|rest| !names.contains(&format!("/dev/cu.{rest}")))
        })
        .collect();
    ports.sort_by(|a, b| {
        b.soul_injector
            .cmp(&a.soul_injector)
            .then_with(|| a.path.cmp(&b.path))
    });
    ports.dedup_by(|a, b| a.path == b.path);
    ports
}

fn port(path: String, usb: UsbPortInfo) -> Port {
    let product = usb.product.clone().unwrap_or_default();
    let soul_injector = product.contains(PRODUCT);
    let product = if product.is_empty() {
        format!("USB serial {:04x}:{:04x}", usb.vid, usb.pid)
    } else {
        product
    };
    Port {
        path,
        product,
        serial: usb.serial_number.filter(|s| !s.is_empty()),
        soul_injector,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb(name: &str, product: Option<&str>) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.into(),
            port_type: SerialPortType::UsbPort(UsbPortInfo {
                vid: 0x303a,
                pid: 0x80ce,
                serial_number: Some("ABC".into()),
                manufacturer: None,
                product: product.map(Into::into),
            }),
        }
    }

    #[test]
    fn soul_injectors_come_first_and_others_stay() {
        let ports = from_infos(vec![
            usb("/dev/ttyACM0", Some("CMSIS-DAP")),
            SerialPortInfo {
                port_name: "/dev/ttyS0".into(),
                port_type: SerialPortType::Unknown,
            },
            usb("/dev/ttyACM1", Some("Soul Injector")),
            usb("/dev/ttyUSB0", None),
        ]);
        let paths: Vec<_> = ports.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, ["/dev/ttyACM1", "/dev/ttyACM0", "/dev/ttyUSB0"]);
        assert!(ports[0].soul_injector);
        assert_eq!(ports[2].product, "USB serial 303a:80ce");
    }

    #[test]
    fn prefers_macos_callout_devices() {
        let ports = from_infos(vec![
            usb("/dev/tty.usbmodem1101", Some("Soul Injector")),
            usb("/dev/cu.usbmodem1101", Some("Soul Injector")),
        ]);
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].path, "/dev/cu.usbmodem1101");
    }
}
