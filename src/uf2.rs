//! Minimal UF2 container validation.
//!
//! A UF2 file is a sequence of fixed 512-byte blocks; each block begins with
//! two magic words and ends with a third. Before flashing we validate the
//! container shape (nonempty, a whole number of 512-byte blocks) and the
//! first block's magic words, so a truncated download, an HTML error page,
//! or an outright non-UF2 file is rejected with a clear error instead of
//! being copied onto the device.

use crate::error::UpdateError;
use crate::volume::Chip;
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

/// Block flag: the `file_size` word at offset 28 holds a family id instead.
const FLAG_FAMILY_ID_PRESENT: u32 = 0x0000_2000;

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

/// Refuse a UF2 whose blocks all name families `chip`'s bootrom ignores.
///
/// note: a bootrom silently skips blocks of a foreign family, so copying an
/// RP2040 image onto an RP2350 "succeeds", nothing is written, and the only
/// symptom is the post-flash verify timing out. A file with no family ids at
/// all can't be judged and is let through.
pub fn check_family(path: &Path, chip: Chip) -> Result<(), UpdateError> {
    let families = family_ids(path)?;
    if families.is_empty() || families.iter().any(|&f| chip.accepts_family(f)) {
        return Ok(());
    }
    let names: Vec<String> = families.iter().map(|&f| family_name(f)).collect();
    Err(UpdateError::WrongChip {
        firmware: names.join(" + "),
        chip: chip.name(),
    })
}

/// Distinct family ids across every block that carries one, in file order.
fn family_ids(path: &Path) -> Result<Vec<u32>, UpdateError> {
    let data = std::fs::read(path)?;
    let mut families = Vec::new();
    for block in data.chunks_exact(BLOCK_SIZE as usize) {
        let block: &[u8; BLOCK_SIZE as usize] = block.try_into().expect("exact chunk");
        if !header_is_uf2(block) {
            return Err(UpdateError::FirmwareInvalid);
        }
        if word(block, 8) & FLAG_FAMILY_ID_PRESENT != 0 {
            let family = word(block, 28);
            if !families.contains(&family) {
                families.push(family);
            }
        }
    }
    Ok(families)
}

fn family_name(id: u32) -> String {
    match id {
        0xE48B_FF56 => "RP2040".to_string(),
        0xE48B_FF57 => "RP2XXX absolute".to_string(),
        0xE48B_FF59 => "RP2350 (ARM)".to_string(),
        0xE48B_FF5A => "RP2350 (RISC-V)".to_string(),
        0xE48B_FF5B => "RP2350 (ARM non-secure)".to_string(),
        other => format!("family 0x{other:08x}"),
    }
}

fn word(block: &[u8; BLOCK_SIZE as usize], off: usize) -> u32 {
    u32::from_le_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]])
}

/// True if a 512-byte block carries all three UF2 magic words.
fn header_is_uf2(block: &[u8; BLOCK_SIZE as usize]) -> bool {
    word(block, 0) == MAGIC_START0 && word(block, 4) == MAGIC_START1 && word(block, 508) == MAGIC_END
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

    fn family_block(family: Option<u32>) -> [u8; 512] {
        let mut b = valid_block();
        if let Some(f) = family {
            b[8..12].copy_from_slice(&FLAG_FAMILY_ID_PRESENT.to_le_bytes());
            b[28..32].copy_from_slice(&f.to_le_bytes());
        }
        b
    }

    fn write_uf2(name: &str, blocks: &[[u8; 512]]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("newerglow-uf2-{}-{name}.uf2", std::process::id()));
        std::fs::write(&path, blocks.concat()).unwrap();
        path
    }

    #[test]
    fn family_check_matches_chip() {
        let rp2040 = write_uf2("rp2040", &[family_block(Some(0xE48B_FF56)); 2]);
        let rp2350 = write_uf2("rp2350", &[family_block(Some(0xE48B_FF59))]);
        let absolute = write_uf2("absolute", &[family_block(Some(0xE48B_FF57))]);
        let bare = write_uf2("bare", &[family_block(None)]);

        assert!(check_family(&rp2040, Chip::Rp2040).is_ok());
        assert!(matches!(
            check_family(&rp2040, Chip::Rp2350),
            Err(UpdateError::WrongChip { chip: "RP2350", .. })
        ));
        assert!(check_family(&rp2350, Chip::Rp2350).is_ok());
        assert!(check_family(&rp2350, Chip::Rp2040).is_err());
        assert!(check_family(&absolute, Chip::Rp2350).is_ok());
        assert!(check_family(&absolute, Chip::Rp2040).is_err());
        assert!(check_family(&bare, Chip::Rp2040).is_ok());

        for p in [rp2040, rp2350, absolute, bare] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn mixed_family_file_passes_if_any_family_fits() {
        let both = write_uf2("both", &[family_block(Some(0xE48B_FF56)), family_block(Some(0xE48B_FF59))]);
        assert!(check_family(&both, Chip::Rp2040).is_ok());
        assert!(check_family(&both, Chip::Rp2350).is_ok());
        let _ = std::fs::remove_file(both);
    }
}
