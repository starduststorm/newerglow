use crate::error::UpdateError;
use log::debug;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Bootloader mass-storage volume labels we accept. The RP2040 UF2
/// bootloader mounts as `RPI-RP2`; the RP2350 bootloader mounts as
/// `RP2350`. Both take the same UF2 flash flow.
const VOLUME_LABELS: &[&str] = &["RPI-RP2", "RP2350"];

/// Accept `path` only if it is a real directory (not a symlink that could
/// redirect the flash) whose basename is a known bootloader label.
fn accept_volume(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_dir() {
        return false;
    }
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(name) if VOLUME_LABELS.contains(&name)
    )
}

/// Find mounted bootloader mass-storage volumes (any label in
/// `VOLUME_LABELS`). Returns all matching mount points (usually 0 or 1).
pub fn find_bootloader_volumes() -> Vec<PathBuf> {
    let mut found = Vec::new();

    #[cfg(target_os = "macos")]
    {
        for label in VOLUME_LABELS {
            let path = PathBuf::from("/Volumes").join(label);
            if accept_volume(&path) {
                debug!("found bootloader volume at {}", path.display());
                found.push(path);
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(user) = std::env::var("USER") {
            for prefix in &["/media", "/run/media"] {
                for label in VOLUME_LABELS {
                    let path = PathBuf::from(prefix).join(&user).join(label);
                    if accept_volume(&path) {
                        debug!("found bootloader volume at {}", path.display());
                        found.push(path);
                    }
                }
            }
        }

        // /proc/mounts catches mounts outside the standard automount prefixes.
        if found.is_empty() {
            if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
                for line in mounts.lines() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        let path = PathBuf::from(parts[1]);
                        if accept_volume(&path) {
                            debug!("found bootloader volume via /proc/mounts at {}", path.display());
                            found.push(path);
                        }
                    }
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        found.extend(find_bootloader_volumes_windows());
    }

    found
}

#[cfg(target_os = "windows")]
fn find_bootloader_volumes_windows() -> Vec<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    let mut found = Vec::new();

    for letter in b'A'..=b'Z' {
        let root = format!("{}:\\", letter as char);
        let root_wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
        let mut label_buf = [0u16; 64];

        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW(
                root_wide.as_ptr(),
                label_buf.as_mut_ptr(),
                label_buf.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };

        if ok != 0 {
            let len = label_buf.iter().position(|&c| c == 0).unwrap_or(label_buf.len());
            let label = OsString::from_wide(&label_buf[..len]);
            let label = label.to_string_lossy();
            if VOLUME_LABELS.iter().any(|v| *v == label) {
                debug!("found bootloader volume at {}", root);
                found.push(PathBuf::from(root));
            }
        }
    }

    found
}

/// Return the mounted bootloader volume, but only when exactly one is present.
pub fn check_existing_volume() -> Option<PathBuf> {
    let mut volumes = find_bootloader_volumes();
    if volumes.len() == 1 {
        volumes.pop()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::VOLUME_LABELS;

    #[test]
    fn accepts_both_bootloader_labels() {
        // RP2040 mounts as RPI-RP2, RP2350 as RP2350; both must be accepted.
        assert!(VOLUME_LABELS.contains(&"RPI-RP2"));
        assert!(VOLUME_LABELS.contains(&"RP2350"));
    }

    #[test]
    fn rejects_suffix_spoofed_labels() {
        assert!(!VOLUME_LABELS.contains(&"fake-RPI-RP2"));
        assert!(!VOLUME_LABELS.contains(&"RPI-RP2-x"));
    }
}

/// Poll for the bootloader volume to appear, with a timeout.
pub fn wait_for_volume(timeout: Duration) -> Result<PathBuf, UpdateError> {
    let start = Instant::now();

    // Give the OS a moment to mount the device
    std::thread::sleep(Duration::from_secs(1));

    while start.elapsed() < timeout {
        let mut volumes = find_bootloader_volumes();

        match volumes.len() {
            0 => {}
            1 => {
                let path = volumes.pop().unwrap();
                debug!("bootloader volume appeared at {}", path.display());
                // Give it a moment to fully mount
                std::thread::sleep(Duration::from_millis(500));
                return Ok(path);
            }
            _ => return Err(UpdateError::MultipleVolumes),
        }

        std::thread::sleep(Duration::from_millis(500));
    }

    Err(UpdateError::VolumeTimeout(timeout.as_secs()))
}
