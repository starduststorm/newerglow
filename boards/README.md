# Boards

One directory per update target. Everything that defines a board — how to
recognize it, where its firmware comes from, and how it looks in the app —
lives here, so adding or removing a target never touches `src/`.

```
boards/
  motionhexa/
    board.toml     # required — the manifest
    model.obj      # optional — 3D model shown on the device card
    model.mtl      # optional — materials for model.obj
    model.png      # optional — baked static fallback (see below)
    icon.png       # optional — card image for boards without a model
```

The **directory name is the board id**. It must match `[a-z0-9][a-z0-9_-]*`.
Asset files are picked up by name; there is no field pointing at them.

## board.toml

```toml
display_name = "motionhexa"

[identity]
# Prefix of the device's IDENTIFY response (post `ID:`) that selects this
# board. Empty matches anything, so it only wins when nothing more specific
# does — longest matching prefix wins.
prefix = "motionhexa"
# USB vendor ids this board enumerates under. Required, non-empty:
# enumeration filters on the union across all registered boards.
usb_vid = [0x2E8A]
# Optional product-id narrowing. Empty accepts any pid under usb_vid.
usb_pid = []

[firmware]
github_owner = "starduststorm"
github_repo = "motionhexa"
# Glob matched against release asset names.
asset_pattern = "*.uf2"
# Release tags must start with this; the rest (minus one optional leading
# `v`) is the version. Tags that don't match are dropped from the list.
# Empty accepts both `v1.2.3` and `1.2.3`.
tag_prefix = "fw-v"
```

Unknown keys are rejected, so a typo fails loudly rather than silently
defaulting.

## Card image

The device card falls back in this order:

1. Live 3D render of `model.obj` (+ `model.mtl` if present)
2. `model.png` — used when the GPU path can't be set up or can't sustain
   24 fps
3. `icon.png`
4. A generic USB placeholder

`model.png` is generated, not hand-drawn. After changing a `model.obj` or
`model.mtl`, re-bake and commit the result:

```sh
cargo run --release --bin bake-static-images --features bake
```

That walks every board here and rewrites `model.png` for each one shipping a
`model.obj`.

## Where boards come from at runtime

Boards are read from three places, each overriding the last **by id**:

1. This directory, embedded into the binary at build time by `build.rs`
2. `<config dir>/newerglow/boards/`
   - macOS `~/Library/Application Support/newerglow/boards/`
   - Linux `~/.config/newerglow/boards/`
   - Windows `%APPDATA%\newerglow\boards\`
3. `$NEWERGLOW_BOARDS_DIR`, when set

A board id present in more than one place takes its **manifest** wholesale
from the last one — fields are not merged. Its **assets** fall back per file
to the embedded board of the same id, so overriding a board just to point it
at a different repo doesn't lose the bundled 3D model.

A board that fails to load (bad id, missing or malformed `board.toml`, empty
`usb_vid`) is logged and skipped; anything already registered under that id
stays. Run with `RUST_LOG=info` to see those warnings.

## Adding a board

In-repo: drop a directory here and rebuild. `build.rs` discovers it — the
build fails if `board.toml` is missing or unparseable.

As a user, with no rebuild: drop the same directory into the config-dir path
above and relaunch the app.

## A note on Linux

The udev rule that grants device access is generated from the registered
boards' vendor ids. Adding a board with a new vendor id makes the app's
authorize banner reappear so the rule can be reinstalled to cover it.
