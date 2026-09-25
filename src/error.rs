use thiserror::Error;

#[derive(Error, Debug)]
pub enum UpdateError {
    #[error("no device found on USB")]
    NoDeviceFound,

    #[error("serial port error: {0}")]
    Serial(#[from] serialport::Error),

    #[error("could not open serial port — is another program (serial monitor, PlatformIO) using it?")]
    PortBusy,

    #[error("permission denied opening serial port — run `newerglow-cli --install-udev` once to set up USB device access")]
    PortPermissionDenied,

    #[error("device did not enter BOOTSEL mode within {0}s")]
    BootselTimeout(u64),

    #[error("bootloader volume not found within {0}s after entering BOOTSEL")]
    VolumeTimeout(u64),

    #[error("multiple bootloader volumes found — disconnect all but one device in BOOTSEL mode")]
    MultipleVolumes,

    #[error("firmware copy failed: {0}")]
    FlashFailed(String),

    #[error("device did not reappear on USB within {0}s after flashing")]
    VerifyTimeout(u64),

    #[error("UF2 file not found: {0}")]
    FirmwareNotFound(String),

    #[error("not a valid UF2 firmware file (bad size or missing UF2 signature)")]
    FirmwareInvalid,

    #[error("this firmware is built for {firmware} but the device is an {chip}; pick the build for this hardware")]
    WrongChip { firmware: String, chip: &'static str },

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("user cancelled")]
    Cancelled,
}
