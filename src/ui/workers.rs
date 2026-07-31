use eframe::egui;
use log::debug;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crate::boards::{Board, Registry};
use crate::device::{self, DeviceInfo, UsbId};
use crate::ui::github::{self, Release};
use crate::{bootsel, flash, identify, verify, volume};

/// URI scheme used for mock devices injected via the NEWERGLOW_MOCK env var.
/// Bypasses the real serial layer in identify/blink/update.
const MOCK_PREFIX: &str = "mock://";

fn is_mock(port: &str) -> bool {
    port.starts_with(MOCK_PREFIX)
}

/// Port-name prefix of a blinkable mock; the remainder is the board id it
/// impersonates.
const MOCK_BLINK_PREFIX: &str = "mock://blinkable-";

/// Read NEWERGLOW_MOCK and return shim DeviceInfos to inject into
/// discovery. Recognized values: "basic" (one unidentified), "blinkable"
/// (one per registered board), "both", or "" (no mocks).
fn mock_devices(registry: &Registry) -> Vec<DeviceInfo> {
    let mode = std::env::var("NEWERGLOW_MOCK").unwrap_or_default();
    if mode.is_empty() {
        return Vec::new();
    }
    let want_basic = matches!(mode.as_str(), "basic" | "both");
    let want_blink = matches!(mode.as_str(), "blinkable" | "both");

    // Mocks borrow the first board's USB ids so they look like something the
    // real enumerator would have produced.
    let (vid, pid) = registry
        .usb_filter()
        .first()
        .map(|u| (u.vid, u.pid.unwrap_or(device::PICO_PID)))
        .unwrap_or((device::RP2040_VID, device::PICO_PID));

    let mut devs = Vec::new();
    if want_basic {
        devs.push(DeviceInfo {
            port_name: "mock://unidentified-device".to_string(),
            vid,
            pid,
            manufacturer: Some("MockCo".to_string()),
            product: Some("Mock device".to_string()),
            serial_number: Some("MOCK-UNID-001".to_string()),
            generation: device::Generation::Older,
        });
    }
    if want_blink {
        for (i, board) in registry.boards().iter().enumerate() {
            devs.push(DeviceInfo {
                port_name: format!("{}{}", MOCK_BLINK_PREFIX, board.id),
                vid,
                pid,
                manufacturer: Some("MockCo".to_string()),
                product: Some(format!("Mock {}", board.manifest.display_name)),
                serial_number: Some(format!("MOCK-BLINK-{:03}", i + 1)),
                generation: device::Generation::Older,
            });
        }
    }
    devs
}

/// Mock IDENTIFY response, synthesized from the board the port impersonates.
/// Returns None for non-blinkable mocks and unknown board ids.
fn mock_identify(port: &str, registry: &Registry) -> Option<String> {
    let id = port.strip_prefix(MOCK_BLINK_PREFIX)?;
    let board = registry.get(id)?;
    Some(format!(
        "{} v0.9.0 hw=v5 sn=MOCKDEADBEEF",
        board.manifest.identity.prefix
    ))
}

/// Events emitted by background workers, consumed by the UI thread.
#[derive(Debug)]
pub enum Event {
    DevicesChanged(Vec<DeviceInfo>),
    IdentityResolved {
        port_name: String,
        identity: Option<String>,
        /// True when the IDENTIFY probe couldn't even open the serial
        /// port because of EACCES. Drives the GUI's "Authorize" banner.
        permission_denied: bool,
    },
    ReleasesLoaded {
        board_id: String,
        releases: Vec<Release>,
    },
    ReleasesFailed {
        board_id: String,
        error: String,
    },
    UpdateProgress(UpdatePhase),
    UpdateComplete(Result<(), String>),
}

/// Phases of the update flow, surfaced one-by-one to the UI.
#[derive(Clone, Debug)]
pub enum UpdatePhase {
    Downloading,
    EnteringBootsel,
    WaitingForVolume,
    Flashing,
    Verifying,
}

/// Description of a firmware source for the update worker.
#[derive(Clone)]
pub enum FirmwareSource {
    /// File already on disk; no download needed.
    Local(PathBuf),
    /// Need to download. The destination cache path is computed from
    /// (owner, repo, tag) by the worker. `size` and `digest` come from the
    /// GitHub asset metadata and are used to validate the download.
    Remote {
        owner: String,
        repo: String,
        tag: String,
        url: String,
        /// Expected byte length from the GitHub asset (0 if unknown).
        size: u64,
        /// Expected content digest, e.g. `"sha256:…"` (None on older releases).
        digest: Option<String>,
    },
}

/// Spawn a one-shot thread that fetches the GitHub releases for the given
/// board and emits the result tagged with the board id.
pub fn spawn_fetch_releases(board: Board, tx: mpsc::Sender<Event>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let board_id = board.id.clone();
        let event = match github::fetch_releases(&board) {
            Ok(rs) => Event::ReleasesLoaded {
                board_id,
                releases: rs,
            },
            Err(e) => Event::ReleasesFailed {
                board_id,
                error: e.to_string(),
            },
        };
        let _ = tx.send(event);
        ctx.request_repaint();
    });
}

/// Spawn the discovery worker thread. It polls every `poll_interval` and
/// emits an `Event::DevicesChanged` whenever the set of ports changes.
/// For newly-seen ports it also kicks off a one-shot sub-thread that runs
/// the IDENTIFY challenge and emits `Event::IdentityResolved`.
pub fn spawn_discovery(
    registry: Arc<Registry>,
    tx: mpsc::Sender<Event>,
    ctx: egui::Context,
    poll_interval: Duration,
) {
    std::thread::spawn(move || {
        let mut last_ports: HashSet<String> = HashSet::new();
        let usb = registry.usb_filter();

        loop {
            let mut devices = device::enumerate(&usb);
            devices.extend(mock_devices(&registry));
            let current_ports: HashSet<String> =
                devices.iter().map(|d| d.port_name.clone()).collect();

            let newly_seen: Vec<String> = current_ports
                .difference(&last_ports)
                .cloned()
                .collect();

            if current_ports != last_ports {
                debug!(
                    "discovery: ports changed (was {} -> now {})",
                    last_ports.len(),
                    current_ports.len()
                );
                if tx.send(Event::DevicesChanged(devices.clone())).is_err() {
                    return; // UI gone
                }
                ctx.request_repaint();
                last_ports = current_ports;
            }

            for port in newly_seen {
                spawn_identify(port, registry.clone(), tx.clone(), ctx.clone());
            }

            std::thread::sleep(poll_interval);
        }
    });
}

/// Send a BLINK command to a device on a one-shot thread (so the UI stays
/// responsive). Errors are logged but not surfaced to the UI — blink is
/// purely advisory.
pub fn spawn_blink(port_name: String) {
    std::thread::spawn(move || {
        if is_mock(&port_name) {
            debug!("mock blink: {}", port_name);
            std::thread::sleep(Duration::from_millis(300));
            return;
        }
        if let Err(e) = identify::blink(&port_name) {
            debug!("blink {} failed: {}", port_name, e);
        }
    });
}

/// Run the full update flow on a one-shot worker thread. `serial` is the
/// flashed device's serial number (when known), used to re-identify it
/// after the reboot rather than grabbing the first device on the bus.
pub fn spawn_update(
    port_name: String,
    serial: Option<String>,
    source: FirmwareSource,
    registry: Arc<Registry>,
    tx: mpsc::Sender<Event>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let result = run_update(port_name, serial, source, &registry, &tx, &ctx);
        let _ = tx.send(Event::UpdateComplete(result.map_err(|e| e.to_string())));
        ctx.request_repaint();
    });
}

fn run_update(
    port_name: String,
    serial: Option<String>,
    source: FirmwareSource,
    registry: &Registry,
    tx: &mpsc::Sender<Event>,
    ctx: &egui::Context,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Mock devices walk through the phases with delays for visual testing
    // but never touch real hardware.
    if is_mock(&port_name) {
        for phase in [
            UpdatePhase::Downloading,
            UpdatePhase::EnteringBootsel,
            UpdatePhase::WaitingForVolume,
            UpdatePhase::Flashing,
            UpdatePhase::Verifying,
        ] {
            emit(tx, ctx, Event::UpdateProgress(phase));
            std::thread::sleep(Duration::from_millis(700));
        }
        return Ok(());
    }

    let firmware_path = match source {
        FirmwareSource::Local(p) => p,
        FirmwareSource::Remote {
            owner,
            repo,
            tag,
            url,
            size,
            digest,
        } => {
            emit(tx, ctx, Event::UpdateProgress(UpdatePhase::Downloading));
            download_cached(&owner, &repo, &tag, &url, size, digest.as_deref())?
        }
    };

    // Validate the UF2 container before touching hardware, so a truncated
    // download or a non-UF2 file fails here instead of on the device.
    crate::uf2::validate_file(&firmware_path)?;

    // Snapshot the ports present now (device still in app mode) so post-flash
    // verify can tell the flashed device apart from any others on the bus.
    let usb: Vec<UsbId> = registry.usb_filter();
    let before_ports: HashSet<String> = device::enumerate(&usb)
        .into_iter()
        .map(|d| d.port_name)
        .collect();

    emit(tx, ctx, Event::UpdateProgress(UpdatePhase::EnteringBootsel));
    bootsel::enter_bootsel(&port_name)?;

    emit(tx, ctx, Event::UpdateProgress(UpdatePhase::WaitingForVolume));
    let volume = volume::wait_for_volume(Duration::from_secs(15))?;

    emit(tx, ctx, Event::UpdateProgress(UpdatePhase::Flashing));
    flash::flash_firmware(&volume, &firmware_path)?;

    emit(tx, ctx, Event::UpdateProgress(UpdatePhase::Verifying));
    let _new_device = verify::wait_for_device(
        &usb,
        serial.as_deref(),
        &before_ports,
        Duration::from_secs(20),
    )?;

    Ok(())
}

fn emit(tx: &mpsc::Sender<Event>, ctx: &egui::Context, event: Event) {
    let _ = tx.send(event);
    ctx.request_repaint();
}

/// Download a firmware asset to the user's cache dir and return the local
/// path. A cached copy is reused only while it still matches the asset's
/// size/digest metadata — an asset re-uploaded under the same tag would
/// otherwise be served stale.
fn download_cached(
    owner: &str,
    repo: &str,
    tag: &str,
    url: &str,
    expected_size: u64,
    expected_digest: Option<&str>,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let cache_root = dirs::cache_dir()
        .ok_or("could not determine cache directory")?
        .join("newerglow")
        .join("firmware")
        .join(format!("{}_{}", owner, repo));
    std::fs::create_dir_all(&cache_root)?;

    // Sanitize the tag for filesystem safety
    let safe_tag: String = tag
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    let dest = cache_root.join(format!("{}.uf2", safe_tag));

    if std::fs::metadata(&dest).is_ok_and(|m| m.len() > 0)
        && cached_file_matches(&dest, expected_size, expected_digest)
    {
        debug!("using cached firmware at {}", dest.display());
        return Ok(dest);
    }

    debug!("downloading {} -> {}", url, dest.display());
    github::require_https(url)?;
    let agent = github::build_https_agent();
    let resp = agent
        .get(url)
        .timeout(Duration::from_secs(60))
        .call()?;

    // Cap the download. RP2040's flash is 2 MiB (RP2350 up to 4 MiB); 64 MiB
    // is well above any legitimate UF2 and stops a runaway response (or a
    // hostile redirect on a compromised mirror) from filling the cache disk.
    const MAX_FW_BYTES: u64 = 64 * 1024 * 1024;
    let mut reader = resp.into_reader().take(MAX_FW_BYTES + 1);
    let tmp = dest.with_extension("uf2.partial");
    let copied = {
        let mut out = std::fs::File::create(&tmp)?;
        std::io::copy(&mut reader, &mut out)?
    };
    if copied > MAX_FW_BYTES {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "firmware download exceeded {} bytes — refusing",
            MAX_FW_BYTES
        )
        .into());
    }
    if expected_size > 0 && copied != expected_size {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "firmware download size mismatch: got {} bytes, expected {}",
            copied, expected_size
        )
        .into());
    }
    if let Some(digest) = expected_digest {
        match verify_digest(&tmp, digest) {
            Ok(true) => {}
            Ok(false) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("firmware digest mismatch (expected {})", digest).into());
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("could not verify firmware digest: {}", e).into());
            }
        }
    }
    std::fs::rename(&tmp, &dest)?;

    Ok(dest)
}

/// Whether a cached file still satisfies the expected asset metadata. Prefer
/// the digest; fall back to a size check; if neither is known, trust the
/// cached file (best we can do for pre-digest releases).
fn cached_file_matches(path: &Path, expected_size: u64, expected_digest: Option<&str>) -> bool {
    if let Some(digest) = expected_digest {
        return verify_digest(path, digest).unwrap_or(false);
    }
    if expected_size > 0 {
        return std::fs::metadata(path)
            .map(|m| m.len() == expected_size)
            .unwrap_or(false);
    }
    true
}

/// Verify a file's SHA-256 against a GitHub `digest` string of the form
/// `"sha256:<hex>"`. An unrecognized algorithm prefix is treated as
/// unverifiable (returns Ok(true)) rather than a hard failure, so a future
/// digest format doesn't block updates.
fn verify_digest(
    path: &Path,
    digest: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        debug!("unrecognized digest format {:?}; skipping verification", digest);
        return Ok(true);
    };
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    let actual: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
    Ok(actual.eq_ignore_ascii_case(hex))
}

/// Run the IDENTIFY challenge on a port in a one-shot thread.
fn spawn_identify(
    port_name: String,
    registry: Arc<Registry>,
    tx: mpsc::Sender<Event>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let (identity, permission_denied) = if is_mock(&port_name) {
            (mock_identify(&port_name, &registry), false)
        } else {
            match identify::try_identify(&port_name) {
                Ok(Some(id)) => (Some(id), false),
                Ok(None) => {
                    // A device that just rebooted after a flash re-enumerates
                    // before its firmware's serial handler is ready, so the
                    // first IDENTIFY often returns nothing. Retry a couple of
                    // times with a short backoff before giving up, so a
                    // freshly-updated card doesn't linger as "Unidentified".
                    let mut resolved = None;
                    for delay in [Duration::from_millis(300), Duration::from_millis(600)] {
                        std::thread::sleep(delay);
                        match identify::try_identify(&port_name) {
                            Ok(Some(id)) => {
                                resolved = Some(id);
                                break;
                            }
                            Ok(None) => continue,
                            Err(_) => break,
                        }
                    }
                    (resolved, false)
                }
                Err(e) => {
                    let denied = matches!(
                        e.kind(),
                        serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied)
                    );
                    if denied {
                        debug!("identify {} denied: {}", port_name, e);
                    } else {
                        debug!("identify {} failed: {}", port_name, e);
                    }
                    (None, denied)
                }
            }
        };
        let _ = tx.send(Event::IdentityResolved {
            port_name,
            identity,
            permission_denied,
        });
        ctx.request_repaint();
    });
}

/// Run `pkexec <current_exe> --install-udev` on a one-shot thread, then
/// trigger a re-IDENTIFY of every currently-known port so the GUI's
/// authorize banner self-hides on success. On non-zero exit (user
/// cancelled the polkit dialog, etc.) we warn and leave the UI alone —
/// banner stays so the user can retry.
#[cfg(target_os = "linux")]
pub fn spawn_install_udev(registry: Arc<Registry>, tx: mpsc::Sender<Event>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let exe = std::env::var("APPIMAGE")
            .map(std::path::PathBuf::from)
            .or_else(|_| std::env::current_exe())
            .ok();
        let Some(exe) = exe else {
            log::warn!("install_udev: could not resolve current_exe()");
            return;
        };
        let status = std::process::Command::new("pkexec")
            .arg(&exe)
            .arg("--install-udev")
            .args(
                registry
                    .usb_vids()
                    .iter()
                    .map(|v| format!("--udev-vid={v:04x}")),
            )
            .status();
        match status {
            Ok(s) if s.success() => {
                log::info!("install_udev: pkexec succeeded; re-probing devices");
                let mut devices = device::enumerate(&registry.usb_filter());
                devices.extend(mock_devices(&registry));
                let port_names: Vec<String> =
                    devices.iter().map(|d| d.port_name.clone()).collect();
                if tx.send(Event::DevicesChanged(devices)).is_ok() {
                    for port in port_names {
                        spawn_identify(port, registry.clone(), tx.clone(), ctx.clone());
                    }
                    ctx.request_repaint();
                }
            }
            Ok(s) => log::warn!("install_udev: pkexec exited with {}", s),
            Err(e) => log::warn!("install_udev: failed to spawn pkexec: {}", e),
        }
    });
}

/// No-op on macOS/Windows so call sites stay cfg-free.
#[cfg(not(target_os = "linux"))]
pub fn spawn_install_udev(
    _registry: Arc<Registry>,
    _tx: mpsc::Sender<Event>,
    _ctx: egui::Context,
) {
}
