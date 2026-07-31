//! Per-board model and icon bytes.
//!
//! Embedded boards get theirs from build.rs codegen; on-disk boards read
//! theirs beside board.toml.
//!
//! note: a disk board falls back per-file to the embedded board of the same
//! id. Manifests replace wholesale, but overriding a board just to point it
//! at a different repo shouldn't silently lose its bundled 3D model.

use crate::boards::{Board, BoardOrigin};
use std::borrow::Cow;
use std::path::Path;

#[derive(Default)]
pub struct BoardAssets {
    pub model_obj: Option<Cow<'static, [u8]>>,
    pub model_mtl: Option<Cow<'static, [u8]>>,
    pub model_png: Option<Cow<'static, [u8]>>,
    pub icon_png: Option<Cow<'static, [u8]>>,
}

include!(concat!(env!("OUT_DIR"), "/board_assets_generated.rs"));

pub fn load(board: &Board) -> BoardAssets {
    match &board.origin {
        BoardOrigin::Embedded => embedded_assets(&board.id),
        BoardOrigin::Disk(dir) => {
            let embedded = embedded_assets(&board.id);
            BoardAssets {
                model_obj: read(dir, "model.obj").or(embedded.model_obj),
                model_mtl: read(dir, "model.mtl").or(embedded.model_mtl),
                model_png: read(dir, "model.png").or(embedded.model_png),
                icon_png: read(dir, "icon.png").or(embedded.icon_png),
            }
        }
    }
}

fn read(dir: &Path, name: &str) -> Option<Cow<'static, [u8]>> {
    std::fs::read(dir.join(name)).ok().map(Cow::Owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boards::{BoardManifest, Firmware, Identity};
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("newerglow-assets-test-{tag}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn board(id: &str, origin: BoardOrigin) -> Board {
        Board {
            id: id.to_string(),
            manifest: BoardManifest {
                display_name: id.to_string(),
                identity: Identity {
                    prefix: id.to_string(),
                    usb_vid: vec![0x2E8A],
                    usb_pid: Vec::new(),
                },
                firmware: Firmware {
                    github_owner: "o".into(),
                    github_repo: "r".into(),
                    asset_pattern: "*.uf2".into(),
                    tag_prefix: String::new(),
                },
            },
            origin,
        }
    }

    #[test]
    fn embedded_board_ships_its_bundled_model() {
        let assets = load(&board("motionhexa", BoardOrigin::Embedded));
        assert!(assets.model_obj.is_some(), "boards/motionhexa ships a model.obj");
        assert!(assets.model_png.is_some(), "and a baked model.png");
    }

    #[test]
    fn unknown_board_has_no_assets() {
        let assets = load(&board("nonexistent", BoardOrigin::Embedded));
        assert!(assets.model_obj.is_none());
        assert!(assets.icon_png.is_none());
    }

    #[test]
    fn disk_board_reads_its_own_files() {
        let dir = TempDir::new("own");
        std::fs::write(dir.0.join("icon.png"), b"icon-bytes").unwrap();

        let assets = load(&board("dummy", BoardOrigin::Disk(dir.0.clone())));
        assert_eq!(assets.icon_png.as_deref(), Some(&b"icon-bytes"[..]));
        assert!(assets.model_obj.is_none(), "no model.obj on disk, none embedded");
    }

    #[test]
    fn disk_board_falls_back_per_file_to_the_embedded_board() {
        let dir = TempDir::new("fallback");
        // Overrides only the icon; the model must still come from the
        // embedded board of the same id.
        std::fs::write(dir.0.join("icon.png"), b"icon-bytes").unwrap();

        let assets = load(&board("motionhexa", BoardOrigin::Disk(dir.0.clone())));
        assert_eq!(assets.icon_png.as_deref(), Some(&b"icon-bytes"[..]));
        assert!(
            assets.model_obj.is_some(),
            "overriding a board must not lose its bundled model"
        );
    }

    #[test]
    fn disk_files_win_over_embedded_ones() {
        let dir = TempDir::new("win");
        std::fs::write(dir.0.join("model.obj"), b"local-obj").unwrap();

        let assets = load(&board("motionhexa", BoardOrigin::Disk(dir.0.clone())));
        assert_eq!(assets.model_obj.as_deref(), Some(&b"local-obj"[..]));
    }
}
