use log::debug;
use serialport::SerialPort;
use std::io::{BufRead, BufReader, Read, Write};
use std::time::{Duration, Instant};

/// Default baud rate for the IDENTIFY challenge.
/// Most RP2040 CDC ACM devices ignore baud rate, but we use a sensible default.
const IDENTIFY_BAUD: u32 = 115200;

/// How long to wait for an IDENTIFY response. We may receive several lines
/// of unrelated log output before (or instead of) the ID: response.
const IDENTIFY_TIMEOUT: Duration = Duration::from_millis(800);

/// Prefix the device must use to respond to IDENTIFY. This lets us filter out
/// interleaved log output from the device's normal serial logging.
const ID_PREFIX: &str = "ID:";

/// Read cap for one IDENTIFY exchange — bounds a device that streams
/// bytes without ever sending '\n'.
const MAX_IDENT_BYTES: u64 = 4096;

/// Cap on the post-`ID:` payload length we keep.
const MAX_IDENTITY_LEN: usize = 256;

/// Try to identify a device by sending "IDENTIFY\n" over serial.
///
/// Returns `Ok(Some(identity_string))` if the device responds with a line
/// starting with "ID:", `Ok(None)` if no such line arrives within the
/// timeout, or `Err` if the port can't be opened. The returned string is
/// always sanitized to printable ASCII and length-bounded.
pub fn try_identify(port_name: &str) -> Result<Option<String>, serialport::Error> {
    let mut port = serialport::new(port_name, IDENTIFY_BAUD)
        .timeout(Duration::from_millis(100))
        .open()?;

    flush_input(&mut *port);

    port.write_all(b"IDENTIFY\n")?;
    port.flush()?;

    // The device is untrusted: a malfunctioning or hostile firmware could
    // stream bytes without newline. Cap the underlying reader so the
    // BufReader's read_line cannot grow past MAX_IDENT_BYTES across the
    // whole exchange.
    let mut reader = BufReader::new(port.take(MAX_IDENT_BYTES));
    let deadline = Instant::now() + IDENTIFY_TIMEOUT;
    let mut line = String::new();

    while Instant::now() < deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix(ID_PREFIX) {
                    let identity = sanitize_identity(rest.trim());
                    debug!("IDENTIFY response: {:?}", identity);
                    return Ok(Some(identity));
                } else if !trimmed.is_empty() {
                    debug!("ignoring non-ID line: {:?}", trimmed);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(serialport::Error::from(e)),
        }
    }

    Ok(None)
}

/// Keep only printable ASCII (graphic characters and space) and truncate
/// to MAX_IDENTITY_LEN — the device is untrusted input.
fn sanitize_identity(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_IDENTITY_LEN));
    for c in s.chars() {
        if c.is_ascii_graphic() || c == ' ' {
            out.push(c);
            if out.len() >= MAX_IDENTITY_LEN {
                break;
            }
        }
    }
    out
}

/// Send a BLINK command to a device for visual identification.
pub fn blink(port_name: &str) -> Result<(), serialport::Error> {
    let mut port = serialport::new(port_name, IDENTIFY_BAUD)
        .timeout(Duration::from_millis(100))
        .open()?;

    port.write_all(b"BLINK\n")?;
    port.flush()?;

    Ok(())
}

/// Drain any pending input from the serial port.
fn flush_input(port: &mut dyn SerialPort) {
    let mut buf = [0u8; 256];
    while let Ok(n) = port.read(&mut buf) {
        if n == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize_identity, MAX_IDENTITY_LEN};

    #[test]
    fn passes_through_normal_identity() {
        let s = sanitize_identity("motionhexa v0.9.0 hw=5 sn=DEADBEEF");
        assert_eq!(s, "motionhexa v0.9.0 hw=5 sn=DEADBEEF");
    }

    #[test]
    fn strips_ansi_and_control_bytes() {
        // The ESC byte (\x1b), CR/LF, and NUL are stripped. The bracket /
        // letters / digits that *would* have formed an ANSI escape are
        // preserved as inert text — without ESC they have no special
        // meaning to a terminal or to egui, which is the property we
        // care about for safe rendering.
        let s = sanitize_identity("\x1b[31mevil\x1b[0m\r\nproduct\x00v1.0");
        assert_eq!(s, "[31mevil[0mproductv1.0");
        assert!(!s.contains('\x1b'));
        assert!(!s.contains('\x00'));
        assert!(!s.contains('\r'));
        assert!(!s.contains('\n'));
    }

    #[test]
    fn strips_non_ascii_including_bidi_and_combining() {
        // Right-to-left override + combining chars + emoji must be dropped.
        let s = sanitize_identity("a\u{202E}b\u{0301}c\u{1F4A9}");
        assert_eq!(s, "abc");
    }

    #[test]
    fn keeps_space_but_strips_other_whitespace() {
        let s = sanitize_identity("a b\tc\nd\re");
        assert_eq!(s, "a bcde");
    }

    #[test]
    fn truncates_oversized_input() {
        let huge: String = "x".repeat(MAX_IDENTITY_LEN * 4);
        let s = sanitize_identity(&huge);
        assert_eq!(s.len(), MAX_IDENTITY_LEN);
        assert!(s.chars().all(|c| c == 'x'));
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert_eq!(sanitize_identity(""), "");
    }
}
