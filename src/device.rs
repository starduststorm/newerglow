use crate::error::UpdateError;
use crate::identify;
use log::debug;
use serialport::SerialPortType;
use std::fmt;

/// Default Raspberry Pi vendor ID.
pub const RP2040_VID: u16 = 0x2E8A;

/// Default Raspberry Pi Pico product ID (application mode).
pub const PICO_PID: u16 = 0x000A;

/// One USB id filter entry. `pid: None` accepts any product id under `vid`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UsbId {
    pub vid: u16,
    pub pid: Option<u16>,
}

#[derive(Debug, Clone)]
pub enum Generation {
    /// No serial command support — older firmware or unknown device.
    Older,
    /// Responds to IDENTIFY/BLINK serial commands.
    Newer { identity: String },
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub port_name: String,
    pub vid: u16,
    pub pid: u16,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial_number: Option<String>,
    pub generation: Generation,
}

impl fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.generation {
            Generation::Newer { identity } => write!(f, "{} [{}]", self.port_name, identity),
            Generation::Older => {
                let desc = self.product.as_deref().unwrap_or("USB device");
                write!(f, "{} [{}]", self.port_name, desc)
            }
        }
    }
}

/// Enumerate USB serial ports matching any entry in `filter`.
///
/// An empty filter matches nothing — with no registered boards there is
/// nothing to update.
pub fn enumerate(filter: &[UsbId]) -> Vec<DeviceInfo> {
    let ports = match serialport::available_ports() {
        Ok(ports) => ports,
        Err(e) => {
            debug!("failed to enumerate serial ports: {}", e);
            return Vec::new();
        }
    };

    let mut devices = Vec::new();

    for port in ports {
        let usb_info = match &port.port_type {
            SerialPortType::UsbPort(info) => info,
            _ => continue,
        };

        let matches = filter
            .iter()
            .any(|f| f.vid == usb_info.vid && f.pid.is_none_or(|p| p == usb_info.pid));
        if !matches {
            continue;
        }

        // On macOS, USB CDC devices appear as both /dev/cu.* (call-up, outgoing — what we want)
        // and /dev/tty.* (incoming, blocks waiting for DCD). Skip the tty.* duplicates.
        #[cfg(target_os = "macos")]
        if port.port_name.starts_with("/dev/tty.") {
            debug!("skipping macOS tty duplicate: {}", port.port_name);
            continue;
        }

        debug!(
            "found USB serial device: {} (VID:{:04X} PID:{:04X})",
            port.port_name, usb_info.vid, usb_info.pid
        );

        devices.push(DeviceInfo {
            port_name: port.port_name,
            vid: usb_info.vid,
            pid: usb_info.pid,
            manufacturer: usb_info.manufacturer.clone(),
            product: usb_info.product.clone(),
            serial_number: usb_info.serial_number.clone(),
            generation: Generation::Older, // will be updated by classify
        });
    }

    devices
}

/// Try to classify each device by sending an IDENTIFY challenge.
/// Devices that respond are marked as Newer; others remain Older.
pub fn classify(devices: &mut [DeviceInfo]) {
    for dev in devices.iter_mut() {
        match identify::try_identify(&dev.port_name) {
            Ok(Some(identity)) => {
                debug!("{}: identified as newer device: {}", dev.port_name, identity);
                dev.generation = Generation::Newer { identity };
            }
            Ok(None) => {
                debug!("{}: no IDENTIFY response (older device)", dev.port_name);
            }
            Err(e) => {
                debug!("{}: IDENTIFY failed: {} (treating as older)", dev.port_name, e);
            }
        }
    }
}

/// Select a device, prompting the user if there are multiple.
pub fn select(devices: &[DeviceInfo]) -> Result<DeviceInfo, UpdateError> {
    match devices.len() {
        0 => Err(UpdateError::NoDeviceFound),
        1 => Ok(devices[0].clone()),
        _ => prompt_selection(devices),
    }
}

fn prompt_selection(devices: &[DeviceInfo]) -> Result<DeviceInfo, UpdateError> {
    println!("Multiple devices found:\n");

    for (i, dev) in devices.iter().enumerate() {
        println!("  {}. {}", i + 1, dev);
    }

    let any_blinkable = devices
        .iter()
        .any(|d| matches!(d.generation, Generation::Newer { .. }));
    if any_blinkable {
        println!("\nTip: enter 'b<number>' to blink a device's LEDs for identification (e.g. 'b1')");
    }

    println!();

    loop {
        let input: String = dialoguer::Input::new()
            .with_prompt("Select device number")
            .interact_text()
            .map_err(|_| UpdateError::Cancelled)?;

        let input = input.trim();

        if let Some(idx) = parse_blink_request(input) {
            if idx >= 1 && idx <= devices.len() {
                blink_device(idx, &devices[idx - 1]);
                continue;
            }
        }

        if let Ok(idx) = input.parse::<usize>() {
            if idx >= 1 && idx <= devices.len() {
                return Ok(devices[idx - 1].clone());
            }
        }

        println!("  Invalid selection. Enter a number from 1 to {}.", devices.len());
    }
}

/// `"b3"` / `"B3"` → `Some(3)`; anything else → `None`.
fn parse_blink_request(input: &str) -> Option<usize> {
    input
        .strip_prefix('b')
        .or_else(|| input.strip_prefix('B'))?
        .parse()
        .ok()
}

fn blink_device(idx: usize, dev: &DeviceInfo) {
    if matches!(dev.generation, Generation::Newer { .. }) {
        match identify::blink(&dev.port_name) {
            Ok(()) => println!("  Sent blink command to {}", dev.port_name),
            Err(e) => println!("  Blink failed: {}", e),
        }
    } else {
        println!("  Device {} does not support blink (older firmware)", idx);
    }
}
