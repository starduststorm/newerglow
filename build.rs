//! Discovers `boards/<id>/` at compile time and generates two files into OUT_DIR:
//!
//!   boards_generated.rs       — EMBEDDED_MANIFESTS: &[(id, board.toml source)].
//!                               Always compiled in; the lib parses it at startup
//!                               through the same code path as on-disk boards.
//!   board_assets_generated.rs — embedded_assets(id) -> BoardAssets, include_bytes!
//!                               of each board's model/icon files. Only include!d
//!                               from src/ui/board_assets.rs, so a CLI-only build
//!                               never expands it and never carries the model.
//!
//! Validation here is syntax-only. The typed schema check lives in
//! src/boards.rs's tests so the required-key list has exactly one home.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Asset files a board dir may contain, paired with the BoardAssets field
/// each one populates.
const ASSET_FILES: &[(&str, &str)] = &[
    ("model_obj", "model.obj"),
    ("model_mtl", "model.mtl"),
    ("model_png", "model.png"),
    ("icon_png", "icon.png"),
];

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let boards_dir = manifest_dir.join("boards");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed=build.rs");
    // note: cargo fingerprints a watched directory recursively by mtime, so
    // this line — not the per-file ones below — is what catches a board dir
    // being added or a file being deleted.
    println!("cargo:rerun-if-changed=boards");

    let mut ids: Vec<String> = std::fs::read_dir(&boards_dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", boards_dir.display()))
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    ids.sort();

    let mut manifests = String::from("pub(crate) static EMBEDDED_MANIFESTS: &[(&str, &str)] = &[\n");
    let mut assets = String::from("fn embedded_assets(id: &str) -> BoardAssets {\n    match id {\n");

    for id in &ids {
        assert!(
            !id.is_empty()
                && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                && id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
            "board directory name {id:?} must match [a-z0-9][a-z0-9_-]*"
        );

        let dir = boards_dir.join(id);
        let toml_path = dir.join("board.toml");
        assert!(
            toml_path.is_file(),
            "boards/{id}/board.toml is missing"
        );
        let src = std::fs::read_to_string(&toml_path)
            .unwrap_or_else(|e| panic!("{}: {e}", toml_path.display()));
        src.parse::<toml::Table>()
            .unwrap_or_else(|e| panic!("{}: invalid TOML: {e}", toml_path.display()));
        watch(&toml_path);

        writeln!(manifests, "    ({id:?}, include_str!({})),", lit(&toml_path)).unwrap();

        writeln!(assets, "        {id:?} => BoardAssets {{").unwrap();
        for (field, file) in ASSET_FILES {
            let path = dir.join(file);
            if path.is_file() {
                watch(&path);
                writeln!(
                    assets,
                    "            {field}: Some(Cow::Borrowed(include_bytes!({}).as_slice())),",
                    lit(&path)
                )
                .unwrap();
            } else {
                writeln!(assets, "            {field}: None,").unwrap();
            }
        }
        writeln!(assets, "        }},").unwrap();
    }

    manifests.push_str("];\n");
    assets.push_str("        _ => BoardAssets::default(),\n    }\n}\n");

    std::fs::write(out_dir.join("boards_generated.rs"), manifests).unwrap();
    std::fs::write(out_dir.join("board_assets_generated.rs"), assets).unwrap();
}

fn watch(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
}

/// Absolute path as a Rust string literal. `include_bytes!`/`include_str!`
/// resolve relative to the including file, which lives in OUT_DIR — so the
/// path must be absolute, and `{:?}` escapes the backslashes in Windows paths.
fn lit(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("board paths must be UTF-8"))
}
