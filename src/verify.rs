use crate::device::{self, DeviceInfo, UsbId};
use crate::error::UpdateError;
use log::debug;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Wait for the flashed device to reappear on the USB bus after a UF2 write.
///
/// Tries to identify *the device we just flashed* rather than the first
/// one seen — with a second device plugged in we'd otherwise "verify"
/// instantly against the untouched one. Match priority: exact serial
/// (stable across the reboot), then a port not in `before_ports`, then —
/// when no serial is known — the sole device present.
pub fn wait_for_device(
    filter: &[UsbId],
    expected_serial: Option<&str>,
    before_ports: &HashSet<String>,
    timeout: Duration,
) -> Result<DeviceInfo, UpdateError> {
    let start = Instant::now();

    // Give the device time to reboot — the mass storage disconnects, firmware boots,
    // then USB CDC re-enumerates. Typically takes 2–5 seconds.
    std::thread::sleep(Duration::from_secs(2));

    while start.elapsed() < timeout {
        let devices = device::enumerate(filter);

        if let Some(serial) = expected_serial {
            if let Some(dev) = devices
                .iter()
                .find(|d| d.serial_number.as_deref() == Some(serial))
            {
                debug!("device reappeared (serial match): {}", dev);
                return Ok(dev.clone());
            }
        }

        if let Some(dev) = devices.iter().find(|d| !before_ports.contains(&d.port_name)) {
            debug!("device reappeared (new port): {}", dev);
            return Ok(dev.clone());
        }

        if expected_serial.is_none() && devices.len() == 1 {
            let dev = devices.into_iter().next().unwrap();
            debug!("device reappeared (sole device): {}", dev);
            return Ok(dev);
        }

        std::thread::sleep(Duration::from_secs(1));
    }

    Err(UpdateError::VerifyTimeout(timeout.as_secs()))
}
