//! First-run permission setup for Linux.
//!
//! Linux denies non-root users access to `/dev/ttyACM*` by default
//! (mode 0660, group `dialout` on Debian-family / `uucp` on Arch-family).
//! Rather than telling users to add themselves to a group and re-login,
//! we install a udev rule that tags every registered board's USB vendor
//! id with `uaccess`. With that
//! tag, systemd-logind hands the active console seat user an ACL on the
//! device — no group membership, no relog, works on every modern
//! systemd-based desktop distro.
//!
//! `/etc/udev/rules.d/` is root-only, so the installer re-execs itself
//! via `pkexec`. The PolicyKit action file gives the prompt a friendly
//! message; on immutable distros where `/usr/share/` is read-only, the
//! policy write degrades to a generic prompt but the udev rule still
//! installs (the udev rule is what actually grants access).
//!
//! On macOS and Windows the module's `run` and `rule_file_installed` are
//! stubs so callers can stay `cfg`-free at the call site.

#[cfg(not(target_os = "linux"))]
pub fn run(_vids: &[u16]) -> Result<(), crate::error::UpdateError> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn rule_file_installed(_vids: &[u16]) -> bool {
    true
}

#[cfg(target_os = "linux")]
pub use linux::{rule_file_installed, run};

#[cfg(target_os = "linux")]
mod linux {
    use crate::error::UpdateError;
    use log::{info, warn};
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const UDEV_RULE_PATH: &str = "/etc/udev/rules.d/99-newerglow.rules";
    const POLKIT_POLICY_PATH: &str =
        "/usr/share/polkit-1/actions/art.starduststorm.newerglow.policy";

    const POLKIT_POLICY: &str =
        include_str!("../assets/linux/polkit/art.starduststorm.newerglow.policy");

    /// The udev rule text covering `vids`.
    ///
    /// Generated rather than shipped as a static file: a board dropped in by
    /// the user can declare any vendor id, and a stale rule set would leave
    /// its device permanently EACCES with no visible cause.
    fn udev_rules(vids: &[u16]) -> String {
        let mut out = String::from(
            "# Installed by Newer Glow. Grants the active local console user ACL\n\
             # access to updatable devices (CDC ACM application mode + BOOTSEL\n\
             # mass storage). Regenerated from the registered boards' vendor ids.\n",
        );
        for vid in vids {
            out.push_str(&format!(
                "SUBSYSTEM==\"tty\", ATTRS{{idVendor}}==\"{vid:04x}\", TAG+=\"uaccess\"\n\
                 SUBSYSTEM==\"usb\", ATTRS{{idVendor}}==\"{vid:04x}\", TAG+=\"uaccess\"\n"
            ));
        }
        out
    }

    /// True only when the installed rule matches what we'd write now.
    ///
    /// note: content comparison, not existence. Adding a board with a new
    /// vendor id after a prior install must re-show the authorize banner —
    /// otherwise the new device is denied with the banner suppressed.
    pub fn rule_file_installed(vids: &[u16]) -> bool {
        fs::read_to_string(UDEV_RULE_PATH).is_ok_and(|s| s == udev_rules(vids))
    }

    pub fn run(vids: &[u16]) -> Result<(), UpdateError> {
        if unsafe { libc::geteuid() } != 0 {
            return reexec_via_pkexec(vids);
        }
        install_as_root(vids)
    }

    /// AppImage exposes `$APPIMAGE` as the path of the outer `.AppImage`
    /// file. Using it (when set) skips the squashfs interior path that
    /// `current_exe()` would otherwise return — pkexec needs a stable,
    /// re-runnable absolute path.
    fn self_exe() -> io::Result<PathBuf> {
        if let Ok(p) = std::env::var("APPIMAGE") {
            return Ok(PathBuf::from(p));
        }
        std::env::current_exe()
    }

    /// note: the vendor ids are passed on the command line because the child
    /// runs as root, where `dirs::config_dir()` points at /root — the user's
    /// board directory is invisible, so a registry rebuilt there would miss
    /// exactly the boards that motivate regenerating the rules.
    fn reexec_via_pkexec(vids: &[u16]) -> Result<(), UpdateError> {
        let exe = self_exe()?;
        info!("re-execing via pkexec: {} --install-udev", exe.display());
        let status = Command::new("pkexec")
            .arg(&exe)
            .arg("--install-udev")
            .args(vids.iter().map(|v| format!("--udev-vid={v:04x}")))
            .status()
            .map_err(|e| {
                UpdateError::Io(io::Error::new(
                    e.kind(),
                    format!("failed to launch pkexec: {} (is PolicyKit installed?)", e),
                ))
            })?;
        if !status.success() {
            return Err(UpdateError::Io(io::Error::other(format!(
                "pkexec exited with status {} — authentication cancelled or failed",
                status
            ))));
        }
        Ok(())
    }

    fn install_as_root(vids: &[u16]) -> Result<(), UpdateError> {
        // The udev rule is the mandatory artifact — without it, the ACL
        // grant never happens. Surface this failure to the caller.
        write_file(Path::new(UDEV_RULE_PATH), &udev_rules(vids))?;
        info!("installed udev rule at {}", UDEV_RULE_PATH);

        // The polkit policy only customizes the auth-dialog message.
        // On immutable distros (Silverblue, SteamOS) /usr is read-only;
        // the udev rule alone is enough to make the app work, so we
        // warn and continue rather than aborting.
        match write_file(Path::new(POLKIT_POLICY_PATH), POLKIT_POLICY) {
            Ok(()) => info!("installed polkit policy at {}", POLKIT_POLICY_PATH),
            Err(e) => warn!(
                "could not install polkit policy at {} ({}); future prompts will use the generic pkexec message",
                POLKIT_POLICY_PATH, e
            ),
        }

        reload_udev(vids)?;
        Ok(())
    }

    fn write_file(path: &Path, contents: &str) -> Result<(), UpdateError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
        Ok(())
    }

    fn reload_udev(vids: &[u16]) -> Result<(), UpdateError> {
        let status = Command::new("udevadm")
            .args(["control", "--reload-rules"])
            .status()?;
        if !status.success() {
            warn!("udevadm control --reload-rules exited with {}", status);
        }

        for vid in vids {
            let status = Command::new("udevadm")
                .args([
                    "trigger",
                    "--subsystem-match=tty",
                    "--subsystem-match=usb",
                    &format!("--attr-match=idVendor={vid:04x}"),
                ])
                .status()?;
            if !status.success() {
                warn!("udevadm trigger for {:04x} exited with {}", vid, status);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::{udev_rules, POLKIT_POLICY};
        use crate::boards::Registry;
        use crate::device::RP2040_VID;

        #[test]
        fn udev_rules_cover_every_board_vid() {
            let vids = Registry::load().usb_vids();
            let rules = udev_rules(&vids);
            for v in &vids {
                let vid = format!("{v:04x}");
                for subsystem in ["tty", "usb"] {
                    let line = format!(
                        "SUBSYSTEM==\"{subsystem}\", ATTRS{{idVendor}}==\"{vid}\", TAG+=\"uaccess\""
                    );
                    assert!(rules.contains(&line), "missing rule: {line}\n{rules}");
                }
            }
            // The bundled board declares the RP2040 vendor id, so this also
            // catches RP2040_VID drifting away from boards/*/board.toml.
            assert!(rules.contains(&format!("{:04x}", RP2040_VID)));
        }

        #[test]
        fn polkit_policy_has_action_id() {
            assert!(POLKIT_POLICY.contains("art.starduststorm.newerglow.install-udev"));
        }
    }
}
