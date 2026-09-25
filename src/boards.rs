//! Board registry: everything that identifies one update target — USB ids,
//! IDENTIFY prefix, firmware source — discovered from `boards/<id>/` at build
//! time and from user board directories at startup. Asset *bytes* live in
//! `crate::ui::board_assets` so this module carries no UI dependencies.

use crate::device::UsbId;
use log::warn;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Placeholder in `Firmware::asset_pattern` for the device's hardware revision.
pub const HW_PLACEHOLDER: &str = "{hw}";

mod generated {
    include!(concat!(env!("OUT_DIR"), "/boards_generated.rs"));
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardManifest {
    pub display_name: String,
    pub identity: Identity,
    pub firmware: Firmware,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// Prefix of the device's IDENTIFY response (post `ID:`) that selects
    /// this board. Empty matches anything, at length 0.
    #[serde(default)]
    pub prefix: String,
    /// USB vendor ids this board enumerates under. Required and non-empty:
    /// enumeration filters on the union across every registered board, so a
    /// board that declares none would silently never appear.
    pub usb_vid: Vec<u16>,
    /// Optional product-id narrowing. Empty accepts any pid under `usb_vid`.
    #[serde(default)]
    pub usb_pid: Vec<u16>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Firmware {
    pub github_owner: String,
    pub github_repo: String,
    /// Glob matched against release asset names. May contain `{hw}` once,
    /// standing for the device's IDENTIFY `hw=` revision (see
    /// `doc/firmware-release-contract.md`).
    pub asset_pattern: String,
    /// Release tags must start with this; the remainder (minus one optional
    /// leading `v`) parses as the version. Empty accepts `v1.2.3` and `1.2.3`.
    #[serde(default)]
    pub tag_prefix: String,
}

/// Where a board came from. `Disk` carries the directory so the UI can read
/// the board's model and icon files beside its board.toml.
#[derive(Clone, Debug)]
pub enum BoardOrigin {
    Embedded,
    Disk(PathBuf),
}

#[derive(Clone, Debug)]
pub struct Board {
    /// Directory name. Unique within the registry.
    pub id: String,
    pub manifest: BoardManifest,
    pub origin: BoardOrigin,
}

#[derive(Clone, Debug, Default)]
pub struct Registry {
    boards: Vec<Board>,
}

impl Board {
    pub fn releases_page_url(&self) -> String {
        format!(
            "https://github.com/{}/{}/releases",
            self.manifest.firmware.github_owner, self.manifest.firmware.github_repo
        )
    }

    /// Length of the matching identity prefix, or None when it doesn't match.
    pub fn identity_match_len(&self, identity: &str) -> Option<usize> {
        let prefix = &self.manifest.identity.prefix;
        identity.starts_with(prefix.as_str()).then(|| prefix.len())
    }

    /// Every (vid, pid) pair this board enumerates under.
    pub fn usb_ids(&self) -> Vec<UsbId> {
        let id = &self.manifest.identity;
        if id.usb_pid.is_empty() {
            return id.usb_vid.iter().map(|&vid| UsbId { vid, pid: None }).collect();
        }
        id.usb_vid
            .iter()
            .flat_map(|&vid| {
                id.usb_pid
                    .iter()
                    .map(move |&pid| UsbId { vid, pid: Some(pid) })
            })
            .collect()
    }
}

impl Registry {
    /// Embedded boards first, then each user board root in ascending
    /// precedence. A later root replaces an earlier board of the same id.
    pub fn load() -> Self {
        let mut boards: Vec<Board> = Vec::new();

        for (id, src) in generated::EMBEDDED_MANIFESTS {
            match parse_manifest(src) {
                // note: build.rs checks board.toml syntax and `cargo test`
                // checks the schema, so this can't normally fail — but warn
                // rather than panic so one bad board can never brick the app.
                Ok(manifest) => push(
                    &mut boards,
                    Board {
                        id: (*id).to_string(),
                        manifest,
                        origin: BoardOrigin::Embedded,
                    },
                ),
                Err(e) => warn!("ignoring bundled board '{id}': {e}"),
            }
        }

        for root in user_board_roots() {
            load_root(&root, &mut boards);
        }

        boards.sort_by(|a, b| a.id.cmp(&b.id));
        if boards.is_empty() {
            warn!("no boards registered — no devices will be enumerated");
        }
        Registry { boards }
    }

    pub fn boards(&self) -> &[Board] {
        &self.boards
    }

    pub fn get(&self, id: &str) -> Option<&Board> {
        self.boards.iter().find(|b| b.id == id)
    }

    /// Board whose identity prefix is the longest prefix of `identity`.
    ///
    /// note: longest-match rather than first-match. Registry order is
    /// alphabetical by directory name, so under first-match a catch-all board
    /// (empty prefix) sorting early would swallow every device.
    pub fn match_identity(&self, identity: &str) -> Option<&Board> {
        self.boards
            .iter()
            .filter_map(|b| b.identity_match_len(identity).map(|len| (len, b)))
            .max_by_key(|(len, _)| *len)
            .map(|(_, b)| b)
    }

    /// Deduped, sorted USB vendor ids across every board. Coarser than
    /// `usb_filter` — udev rules match on vendor id alone.
    pub fn usb_vids(&self) -> Vec<u16> {
        let mut vids: Vec<u16> = self
            .boards
            .iter()
            .flat_map(|b| b.manifest.identity.usb_vid.iter().copied())
            .collect();
        vids.sort_unstable();
        vids.dedup();
        vids
    }

    /// Deduped union of every board's USB ids — the filter `device::enumerate`
    /// runs on.
    pub fn usb_filter(&self) -> Vec<UsbId> {
        let mut out: Vec<UsbId> = Vec::new();
        for board in &self.boards {
            for id in board.usb_ids() {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        out
    }
}

fn parse_manifest(src: &str) -> Result<BoardManifest, String> {
    let manifest: BoardManifest = toml::from_str(src).map_err(|e| e.to_string())?;
    if manifest.identity.usb_vid.is_empty() {
        return Err("identity.usb_vid must list at least one vendor id".to_string());
    }
    if manifest.firmware.asset_pattern.matches(HW_PLACEHOLDER).count() > 1 {
        return Err(format!("firmware.asset_pattern may contain {HW_PLACEHOLDER} at most once"));
    }
    Ok(manifest)
}

fn push(boards: &mut Vec<Board>, board: Board) {
    match boards.iter().position(|b| b.id == board.id) {
        Some(i) => boards[i] = board,
        None => boards.push(board),
    }
}

/// User board roots, ascending precedence. Both are optional; a missing
/// directory is not an error.
fn user_board_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dir) = dirs::config_dir() {
        roots.push(dir.join("newerglow").join("boards"));
    }
    if let Some(dir) = std::env::var_os("NEWERGLOW_BOARDS_DIR") {
        roots.push(PathBuf::from(dir));
    }
    roots
}

/// Scan one board root, replacing same-id boards already in `boards`.
/// Every failure warns and skips, leaving any board of that id in place.
fn load_root(root: &Path, boards: &mut Vec<Board>) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for dir in dirs {
        let Some(id) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if id.starts_with('.') {
            continue;
        }
        if !valid_id(id) {
            warn!("ignoring board directory {}: id must match [a-z0-9][a-z0-9_-]*", dir.display());
            continue;
        }

        let path = dir.join("board.toml");
        let src = match std::fs::read_to_string(&path) {
            Ok(src) => src,
            Err(e) => {
                warn!("ignoring board '{id}': could not read {}: {e}", path.display());
                continue;
            }
        };
        match parse_manifest(&src) {
            Ok(manifest) => push(
                boards,
                Board {
                    id: id.to_string(),
                    manifest,
                    origin: BoardOrigin::Disk(dir.clone()),
                },
            ),
            Err(e) => warn!("ignoring board '{id}': {e}"),
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch board root that cleans itself up. Tests drive `load_root`
    /// directly rather than `Registry::load` — the env var the latter reads
    /// is process-global and tests run in parallel.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("newerglow-boards-test-{tag}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempRoot(dir)
        }

        fn write_board(&self, id: &str, manifest: &str) -> PathBuf {
            let dir = self.0.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("board.toml"), manifest).unwrap();
            dir
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const VALID: &str = r#"
display_name = "Dummy Board"
[identity]
prefix = "dummy"
usb_vid = [0x239A]
[firmware]
github_owner = "o"
github_repo = "r"
asset_pattern = "*.uf2"
"#;

    fn board(id: &str, prefix: &str, vids: &[u16], pids: &[u16]) -> Board {
        Board {
            id: id.to_string(),
            manifest: BoardManifest {
                display_name: id.to_string(),
                identity: Identity {
                    prefix: prefix.to_string(),
                    usb_vid: vids.to_vec(),
                    usb_pid: pids.to_vec(),
                },
                firmware: Firmware {
                    github_owner: "owner".into(),
                    github_repo: "repo".into(),
                    asset_pattern: "*.uf2".into(),
                    tag_prefix: String::new(),
                },
            },
            origin: BoardOrigin::Embedded,
        }
    }

    /// The real schema gate: every bundled board.toml must deserialize with
    /// deny_unknown_fields and satisfy the non-empty usb_vid rule.
    #[test]
    fn embedded_manifests_parse() {
        assert!(
            !generated::EMBEDDED_MANIFESTS.is_empty(),
            "no boards discovered under boards/"
        );
        for (id, src) in generated::EMBEDDED_MANIFESTS {
            parse_manifest(src).unwrap_or_else(|e| panic!("boards/{id}/board.toml: {e}"));
        }
    }

    #[test]
    fn longest_identity_prefix_wins() {
        let registry = Registry {
            boards: vec![
                board("aaa-generic", "", &[0x2E8A], &[]),
                board("motionhexa", "motionhexa", &[0x2E8A], &[]),
            ],
        };
        assert_eq!(
            registry.match_identity("motionhexa v1.0").map(|b| b.id.as_str()),
            Some("motionhexa")
        );
        assert_eq!(
            registry.match_identity("widget v1.0").map(|b| b.id.as_str()),
            Some("aaa-generic")
        );
    }

    #[test]
    fn unmatched_identity_without_catch_all() {
        let registry = Registry {
            boards: vec![board("motionhexa", "motionhexa", &[0x2E8A], &[])],
        };
        assert!(registry.match_identity("widget v1.0").is_none());
    }

    #[test]
    fn usb_filter_dedups_and_expands_pids() {
        let registry = Registry {
            boards: vec![
                board("a", "a", &[0x2E8A], &[]),
                board("b", "b", &[0x2E8A], &[]),
                board("c", "c", &[0x239A], &[0x0001, 0x0002]),
            ],
        };
        assert_eq!(
            registry.usb_filter(),
            vec![
                UsbId { vid: 0x2E8A, pid: None },
                UsbId { vid: 0x239A, pid: Some(0x0001) },
                UsbId { vid: 0x239A, pid: Some(0x0002) },
            ]
        );
    }

    #[test]
    fn disk_board_is_added_with_its_directory_as_origin() {
        let root = TempRoot::new("add");
        let dir = root.write_board("dummy", VALID);

        let mut boards = Vec::new();
        load_root(&root.0, &mut boards);

        assert_eq!(boards.len(), 1);
        assert_eq!(boards[0].id, "dummy");
        assert_eq!(boards[0].manifest.display_name, "Dummy Board");
        match &boards[0].origin {
            BoardOrigin::Disk(d) => assert_eq!(d, &dir),
            other => panic!("expected Disk origin, got {other:?}"),
        }
    }

    #[test]
    fn disk_board_replaces_embedded_board_of_same_id() {
        let root = TempRoot::new("replace");
        root.write_board("motionhexa", VALID);

        let mut boards = vec![board("motionhexa", "motionhexa", &[0x2E8A], &[])];
        load_root(&root.0, &mut boards);

        assert_eq!(boards.len(), 1, "replaced in place, not appended");
        assert_eq!(boards[0].manifest.display_name, "Dummy Board");
        assert!(matches!(boards[0].origin, BoardOrigin::Disk(_)));
    }

    #[test]
    fn malformed_disk_board_leaves_the_embedded_one_intact() {
        let root = TempRoot::new("malformed");
        root.write_board("motionhexa", "garbage{{{");
        // A directory with no board.toml at all is skipped just as quietly.
        std::fs::create_dir_all(root.0.join("empty")).unwrap();

        let mut boards = vec![board("motionhexa", "motionhexa", &[0x2E8A], &[])];
        load_root(&root.0, &mut boards);

        assert_eq!(boards.len(), 1);
        assert_eq!(boards[0].manifest.display_name, "motionhexa");
        assert!(matches!(boards[0].origin, BoardOrigin::Embedded));
    }

    #[test]
    fn missing_board_root_is_not_an_error() {
        let mut boards = Vec::new();
        load_root(Path::new("/nonexistent/newerglow/boards"), &mut boards);
        assert!(boards.is_empty());
    }

    #[test]
    fn manifest_rejects_empty_usb_vid() {
        let src = r#"
display_name = "x"
[identity]
prefix = "x"
usb_vid = []
[firmware]
github_owner = "o"
github_repo = "r"
asset_pattern = "*.uf2"
"#;
        assert!(parse_manifest(src).is_err());
    }

    #[test]
    fn manifest_rejects_repeated_hw_placeholder() {
        let src = r#"
display_name = "x"
[identity]
prefix = "x"
usb_vid = [1]
[firmware]
github_owner = "o"
github_repo = "r"
asset_pattern = "x-{hw}-hw{hw}.uf2"
"#;
        assert!(parse_manifest(src).is_err());
        assert!(parse_manifest(&src.replace("x-{hw}-", "x-*-")).is_ok());
    }

    #[test]
    fn manifest_rejects_unknown_field() {
        let src = r#"
display_name = "x"
id = "x"
[identity]
usb_vid = [1]
[firmware]
github_owner = "o"
github_repo = "r"
asset_pattern = "*.uf2"
"#;
        let err = parse_manifest(src).unwrap_err();
        assert!(err.contains("id"), "error should name the unknown field: {err}");
    }
}
