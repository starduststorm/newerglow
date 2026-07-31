//! Minimal UF2 container validation.
//!
//! A UF2 file is a sequence of fixed 512-byte blocks; each block begins with
//! two magic words and ends with a third. Before flashing we validate the
//! container shape (nonempty, a whole number of 512-byte blocks) and the
//! first block's magic words, so a truncated download, an HTML error page,
//! or an outright non-UF2 file is rejected with a clear error instead of
//! being copied onto the device.

use crate::error::UpdateError;
use std::io::Read;
use std::path::Path;

/// First magic word at offset 0 of every UF2 block ("UF2\n" little-endian).
const MAGIC_START0: u32 = 0x0A32_4655;
/// Second magic word at offset 4 of every UF2 block.
const MAGIC_START1: u32 = 0x9E5D_5157;
/// Magic word in the final four bytes (offset 508) of every UF2 block.
const MAGIC_END: u32 = 0x0AB1_6F30;

/// Fixed UF2 block size.
const BLOCK_SIZE: u64 = 512;

/// Validate the container shape and the first block's magic words.
/// Returns [`UpdateError::FirmwareInvalid`] otherwise.
pub fn validate_file(path: &Path) -> Result<(), UpdateError> {
    let len = std::fs::metadata(path)?.len();
    if len == 0 || len % BLOCK_SIZE != 0 {
        return Err(UpdateError::FirmwareInvalid);
    }
    let mut file = std::fs::File::open(path)?;
    let mut header = [0u8; BLOCK_SIZE as usize];
    file.read_exact(&mut header)?;
    if !header_is_uf2(&header) {
        return Err(UpdateError::FirmwareInvalid);
    }
    Ok(())
}

/// True if a 512-byte block carries all three UF2 magic words.
fn header_is_uf2(block: &[u8; BLOCK_SIZE as usize]) -> bool {
    let word = |off: usize| {
        u32::from_le_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]])
    };
    word(0) == MAGIC_START0 && word(4) == MAGIC_START1 && word(508) == MAGIC_END
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_block() -> [u8; 512] {
        let mut b = [0u8; 512];
        b[0..4].copy_from_slice(&MAGIC_START0.to_le_bytes());
        b[4..8].copy_from_slice(&MAGIC_START1.to_le_bytes());
        b[508..512].copy_from_slice(&MAGIC_END.to_le_bytes());
        b
    }

    #[test]
    fn accepts_valid_block() {
        assert!(header_is_uf2(&valid_block()));
    }

    #[test]
    fn rejects_bad_start_magic() {
        let mut b = valid_block();
        b[0] ^= 0xFF;
        assert!(!header_is_uf2(&b));
    }

    #[test]
    fn rejects_bad_second_magic() {
        let mut b = valid_block();
        b[4] ^= 0xFF;
        assert!(!header_is_uf2(&b));
    }

    #[test]
    fn rejects_missing_end_magic() {
        let mut b = valid_block();
        b[508] ^= 0xFF;
        assert!(!header_is_uf2(&b));
    }

    #[test]
    fn rejects_all_zero_block() {
        assert!(!header_is_uf2(&[0u8; 512]));
    }
}
