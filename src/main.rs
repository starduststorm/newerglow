use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use newerglow::error::UpdateError;
use newerglow::boards::Registry;
use newerglow::device::UsbId;
use newerglow::{bootsel, device, flash, uf2, verify, volume};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "newerglow-cli", about = "Firmware updater for starduststorm USB devices")]
struct Cli {
    /// Path to the .uf2 firmware file
    firmware: Option<PathBuf>,

    /// USB Vendor ID to filter (hex, e.g. 2E8A). Default: every registered board's
    #[arg(long, value_parser = parse_hex_u16)]
    vid: Option<u16>,

    /// USB Product ID to filter (hex, e.g. 000A)
    #[arg(long, value_parser = parse_hex_u16)]
    pid: Option<u16>,

    /// Serial port to use directly (skip enumeration)
    #[arg(long)]
    port: Option<String>,

    /// Enable verbose debug logging
    #[arg(short, long)]
    verbose: bool,

    /// Install the udev rule that grants the active console user access
    /// to the USB devices, prompting once for the admin password via
    /// PolicyKit. One-time setup; not needed on subsequent runs.
    #[cfg(target_os = "linux")]
    #[arg(long, conflicts_with = "firmware")]
    install_udev: bool,

    /// USB vendor ids (hex) the udev rule should cover. Set by the app when
    /// it re-execs itself as root, where the user's board directory isn't
    /// visible; falls back to the registered boards when absent.
    #[cfg(target_os = "linux")]
    #[arg(long = "udev-vid", value_parser = parse_hex_u16, hide = true, requires = "install_udev")]
    udev_vid: Vec<u16>,
}

fn parse_hex_u16(s: &str) -> Result<u16, String> {
    u16::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16)
        .map_err(|e| format!("invalid hex value '{}': {}", s, e))
}

fn main() {
    let cli = Cli::parse();

    let default_level = if cli.verbose { "debug" } else { "warn" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level)).init();

    #[cfg(target_os = "linux")]
    if cli.install_udev {
        let vids = if cli.udev_vid.is_empty() {
            Registry::load().usb_vids()
        } else {
            cli.udev_vid.clone()
        };
        if let Err(e) = newerglow::install_udev::run(&vids) {
            eprintln!("\nerror: {}", e);
            std::process::exit(1);
        }
        println!("USB device access configured. Unplug and replug your device.");
        return;
    }

    if let Err(e) = run(cli) {
        eprintln!("\nerror: {}", e);
        print_troubleshooting(&e);
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), UpdateError> {
    let firmware_path = cli.firmware.as_ref().ok_or_else(|| {
        UpdateError::FirmwareNotFound("(no firmware path provided)".to_string())
    })?;
    if !firmware_path.exists() {
        return Err(UpdateError::FirmwareNotFound(
            firmware_path.display().to_string(),
        ));
    }
    uf2::validate_file(firmware_path)?;

    let fw_name = firmware_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();

    println!("newerglow-cli — firmware updater for USB devices\n");

    let registry = Registry::load();
    // --vid replaces the board union outright; --pid alone narrows within it,
    // preserving the old "filter under the default VID" behavior.
    let usb: Vec<UsbId> = match (cli.vid, cli.pid) {
        (Some(vid), pid) => vec![UsbId { vid, pid }],
        (None, Some(pid)) => registry
            .usb_filter()
            .into_iter()
            .map(|u| UsbId { vid: u.vid, pid: Some(pid) })
            .collect(),
        (None, None) => registry.usb_filter(),
    };

    if let Some(vol) = volume::check_existing_volume() {
        println!("Device already in BOOTSEL mode ({})\n", vol.display());
        // No app-mode device to identify, so no serial to match on; snapshot
        // whatever serial ports exist now so post-flash verify can spot the
        // newly-appeared one.
        let before_ports = snapshot_ports(&usb);
        return flash_and_verify(firmware_path, &vol, &usb, None, &before_ports);
    }

    let spinner = make_spinner("Scanning for devices...");

    let devices = if let Some(ref port) = cli.port {
        spinner.finish_and_clear();
        vec![device::DeviceInfo {
            port_name: port.clone(),
            vid: cli.vid.unwrap_or(device::RP2040_VID),
            pid: cli.pid.unwrap_or(device::PICO_PID),
            manufacturer: None,
            product: None,
            serial_number: None,
            generation: device::Generation::Older,
        }]
    } else {
        let mut devs = device::enumerate(&usb);
        spinner.finish_and_clear();

        if devs.is_empty() {
            return Err(UpdateError::NoDeviceFound);
        }

        device::classify(&mut devs);
        devs
    };

    let selected = device::select(&devices)?;

    println!("\n  Device:   {}", selected);
    println!("  Firmware: {}\n", fw_name);

    let confirm = dialoguer::Confirm::new()
        .with_prompt("Proceed with firmware update?")
        .default(true)
        .interact()
        .map_err(|_| UpdateError::Cancelled)?;

    if !confirm {
        return Err(UpdateError::Cancelled);
    }

    // Snapshot the ports present now (device still in app mode) so post-flash
    // verify can tell the flashed device apart from any others on the bus.
    let before_ports: HashSet<String> =
        devices.iter().map(|d| d.port_name.clone()).collect();
    let expected_serial = selected.serial_number.clone();

    let spinner = make_spinner("Entering bootloader mode...");
    match bootsel::enter_bootsel(&selected.port_name) {
        Ok(()) => spinner.finish_with_message("Device entered BOOTSEL mode"),
        Err(UpdateError::BootselTimeout(_)) => {
            spinner.finish_with_message("Device may not have reset (retrying...)");
            bootsel::enter_bootsel(&selected.port_name)?;
        }
        Err(e) => {
            spinner.finish_and_clear();
            return Err(e);
        }
    }

    let spinner = make_spinner("Waiting for bootloader volume...");
    let vol = match volume::wait_for_volume(Duration::from_secs(15)) {
        Ok(v) => {
            spinner.finish_with_message(format!("Volume found: {}", v.display()));
            v
        }
        Err(e) => {
            spinner.finish_and_clear();
            return Err(e);
        }
    };

    flash_and_verify(
        firmware_path,
        &vol,
        &usb,
        expected_serial.as_deref(),
        &before_ports,
    )
}

/// Enumerate the serial ports currently visible under `usb` as a set of port
/// names — the "before" snapshot for post-flash verify's new-port heuristic.
fn snapshot_ports(usb: &[UsbId]) -> HashSet<String> {
    device::enumerate(usb)
        .into_iter()
        .map(|d| d.port_name)
        .collect()
}

fn flash_and_verify(
    firmware_path: &Path,
    vol: &Path,
    usb: &[UsbId],
    expected_serial: Option<&str>,
    before_ports: &HashSet<String>,
) -> Result<(), UpdateError> {
    let fw_name = firmware_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();

    let spinner = make_spinner(format!("Copying {}...", fw_name));
    flash::flash_firmware(vol, firmware_path)?;
    spinner.finish_with_message("Firmware copied — device is rebooting");

    let spinner = make_spinner("Waiting for device to reappear...");
    match verify::wait_for_device(
        usb,
        expected_serial,
        before_ports,
        Duration::from_secs(20),
    ) {
        Ok(dev) => {
            spinner.finish_with_message(format!("Device online: {}", dev));
            println!("\nFirmware update complete!");
            Ok(())
        }
        Err(UpdateError::VerifyTimeout(secs)) => {
            spinner.finish_with_message("Device did not reappear");
            // This isn't necessarily a failure — the firmware might have changed
            // the VID/PID, or the device might just be slow to enumerate.
            println!(
                "\nWarning: device did not reappear within {}s.",
                secs
            );
            println!("The firmware was copied successfully. If the device is working, the update likely succeeded.");
            println!("Try unplugging and replugging the USB cable.");
            Ok(())
        }
        Err(e) => {
            spinner.finish_and_clear();
            Err(e)
        }
    }
}

fn make_spinner(msg: impl Into<std::borrow::Cow<'static, str>>) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_message(msg);
    pb
}

fn print_troubleshooting(err: &UpdateError) {
    let hint = match err {
        UpdateError::NoDeviceFound => Some(
            "Is the device plugged in via USB? Check the cable and try a different port.\n\
             If the device is unresponsive, try entering BOOTSEL mode manually:\n\
             hold the BOOT button while plugging in the USB cable."
        ),
        UpdateError::PortBusy => Some(
            "Close any programs using the serial port (PlatformIO monitor, screen, minicom, etc.) and try again."
        ),
        UpdateError::PortPermissionDenied => Some(
            "Linux restricts serial port access by default. Run:\n\
             \n    newerglow-cli --install-udev\n\n\
             once to grant access (asks for your password via PolicyKit), then unplug and replug the device."
        ),
        UpdateError::BootselTimeout(_) => Some(
            "The device did not reset into bootloader mode.\n\
             Try entering BOOTSEL mode manually: hold the BOOT button while plugging in the USB cable,\n\
             then run this tool again."
        ),
        UpdateError::VolumeTimeout(_) => Some(
            "The bootloader volume did not appear.\n\
             Try entering BOOTSEL mode manually: hold the BOOT button while plugging in the USB cable.\n\
             On Linux, you may need to mount the device manually or check that your file manager auto-mounts USB drives."
        ),
        UpdateError::MultipleVolumes => Some(
            "Disconnect all but one device in BOOTSEL mode and try again."
        ),
        UpdateError::WrongChip { .. } => Some(
            "Nothing was written. The device is still in bootloader mode; unplug and replug it,\n\
             then run this tool again with the .uf2 built for its hardware revision."
        ),
        UpdateError::FlashFailed(_) => Some(
            "The firmware file could not be written to the device.\n\
             Try unplugging the device, re-entering BOOTSEL mode, and running this tool again."
        ),
        _ => None,
    };

    if let Some(hint) = hint {
        eprintln!("\n{}", hint);
    }
}
