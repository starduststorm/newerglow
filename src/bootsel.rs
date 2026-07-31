use crate::error::UpdateError;
use log::debug;
use std::time::{Duration, Instant};

/// Trigger BOOTSEL mode via the 1200-baud touch mechanism.
///
/// The arduino-pico framework's SerialUSB detects when a host opens the CDC
/// serial port at 1200 baud and drops DTR, then calls reset_usb_boot(0, 0).
/// This is framework-level behavior that works on all arduino-pico devices.
pub fn enter_bootsel(port_name: &str) -> Result<(), UpdateError> {
    debug!("opening {} at 1200 baud for BOOTSEL trigger", port_name);

    let mut port = serialport::new(port_name, 1200)
        .timeout(Duration::from_millis(100))
        .open()
        .map_err(|e| {
            if matches!(e.kind(), serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied)) {
                UpdateError::PortPermissionDenied
            } else if matches!(e.kind(), serialport::ErrorKind::Io(std::io::ErrorKind::ResourceBusy))
                || e.to_string().to_lowercase().contains("busy")
            {
                // Prefer the typed ResourceBusy kind (stable since Rust 1.83);
                // keep the string match as a fallback for platforms/versions
                // that still surface EBUSY as a generic Io error.
                UpdateError::PortBusy
            } else {
                UpdateError::Serial(e)
            }
        })?;

    port.write_data_terminal_ready(true).map_err(UpdateError::Serial)?;
    std::thread::sleep(Duration::from_millis(50));
    port.write_data_terminal_ready(false).map_err(UpdateError::Serial)?;

    // Close the port — the device will now reset into BOOTSEL
    drop(port);

    debug!("1200-baud touch sent, waiting for serial port to disappear");

    wait_for_port_disappearance(port_name, Duration::from_secs(5))
}

/// Poll until the given serial port is no longer listed.
fn wait_for_port_disappearance(port_name: &str, timeout: Duration) -> Result<(), UpdateError> {
    let start = Instant::now();

    // Give the device a moment to start resetting
    std::thread::sleep(Duration::from_millis(500));

    while start.elapsed() < timeout {
        let ports = serialport::available_ports().unwrap_or_default();
        let still_present = ports.iter().any(|p| p.port_name == port_name);

        if !still_present {
            debug!("port {} disappeared — device is resetting", port_name);
            return Ok(());
        }

        std::thread::sleep(Duration::from_millis(250));
    }

    Err(UpdateError::BootselTimeout(timeout.as_secs()))
}
