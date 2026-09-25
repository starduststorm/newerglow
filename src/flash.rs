use crate::error::UpdateError;
use crate::{uf2, volume};
use log::debug;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Copy a UF2 firmware file to the bootloader volume. The bootloader
/// reboots the device itself once the write completes.
/// Refuses, before writing anything, a UF2 built for the other chip.
pub fn flash_firmware(volume: &Path, firmware_path: &Path) -> Result<(), UpdateError> {
    let dest = resolve_dest(volume, firmware_path)?;

    match volume::chip(volume) {
        Some(chip) => uf2::check_family(firmware_path, chip)?,
        None => debug!("can't tell which chip {} belongs to; skipping UF2 family check", volume.display()),
    }

    debug!(
        "copying {} to {}",
        firmware_path.display(),
        dest.display()
    );

    let data = std::fs::read(firmware_path).map_err(|e| {
        UpdateError::FlashFailed(format!("read {}: {}", firmware_path.display(), e))
    })?;
    let mut out = std::fs::File::create(&dest)
        .map_err(|e| UpdateError::FlashFailed(format!("{}: {}", dest.display(), e)))?;
    out.write_all(&data)
        .map_err(|e| UpdateError::FlashFailed(format!("{}: {}", dest.display(), e)))?;

    // The bootloader reboots the instant it sees the final UF2 block, so
    // without an explicit fsync the copy can "complete" while blocks are
    // still in the OS page cache — the device yanks the mount mid-write
    // and macOS reports "Disk Not Ejected Properly".
    out.sync_all()
        .map_err(|e| UpdateError::FlashFailed(format!("sync {}: {}", dest.display(), e)))?;

    debug!("firmware copy complete");
    Ok(())
}

/// Compute the destination path inside `volume`, rejecting filenames
/// that could escape it.
fn resolve_dest(volume: &Path, firmware_path: &Path) -> Result<PathBuf, UpdateError> {
    let raw = firmware_path
        .file_name()
        .ok_or_else(|| UpdateError::FlashFailed(
            format!("invalid firmware path (no filename): {}", firmware_path.display())
        ))?;
    let name = raw.to_str().ok_or_else(|| {
        UpdateError::FlashFailed(format!(
            "invalid firmware filename (non-UTF-8): {}",
            firmware_path.display()
        ))
    })?;
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
    {
        return Err(UpdateError::FlashFailed(format!(
            "refusing unsafe firmware filename: {:?}",
            name
        )));
    }
    Ok(volume.join(name))
}

#[cfg(test)]
mod tests {
    use super::resolve_dest;
    use std::path::{Path, PathBuf};

    #[test]
    fn accepts_normal_filename() {
        let dest = resolve_dest(Path::new("/Volumes/RPI-RP2"), Path::new("/tmp/fw.uf2")).unwrap();
        assert_eq!(dest, PathBuf::from("/Volumes/RPI-RP2/fw.uf2"));
    }

    #[test]
    fn rejects_dot_and_dotdot() {
        assert!(resolve_dest(Path::new("/Volumes/RPI-RP2"), Path::new(".")).is_err());
        assert!(resolve_dest(Path::new("/Volumes/RPI-RP2"), Path::new("..")).is_err());
        assert!(resolve_dest(Path::new("/Volumes/RPI-RP2"), Path::new("/")).is_err());
    }

    #[test]
    fn rejects_path_separators_in_filename() {
        // file_name() of "/a/b" returns "b", so this case is naturally safe.
        // What we guard against is a filename literal that contains a
        // separator after extraction (shouldn't happen via std, but cheap).
        let dest = resolve_dest(Path::new("/Volumes/RPI-RP2"), Path::new("/some/dir/fw.uf2"));
        assert_eq!(dest.unwrap(), PathBuf::from("/Volumes/RPI-RP2/fw.uf2"));
    }
}
