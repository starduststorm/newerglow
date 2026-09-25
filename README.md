# Newer Glow

Cross-platform firmware updater starduststorm device updates, and generically for RP2040/RP2350 devices.

Download the app on the [releases page](../../releases) or build and run headless.

---

## macOS

### Pre-built app

Download `NewerGlow-macos-universal.dmg` from the releases page, open it, and
drag **Newer Glow.app** to Applications.

The app is currently distributed unsigned and unnotarized. On first launch
Gatekeeper says *"developer cannot be verified"* — **right-click → Open**, then
click **Open** in the dialog. macOS remembers the approval; after that it's a
normal double-click.

### Build from Source

You'll need [Rust](https://rustup.rs). 

#### The app

```sh
scripts/build-app.sh              # -> dist/Newer Glow.app
```

#### Headless

```sh
cargo build --release --bin newerglow-cli
./target/release/newerglow-cli --help
./target/release/newerglow-cli firmware.uf2
```

If more than one device is connected it asks which; type `b1`, `b2`, … to blink
a device's LEDs and identify it.

---

## Linux

### Pre-built app

Download `NewerGlow-x86_64.AppImage` from the releases page, make it
executable, and run it:

```sh
chmod +x NewerGlow-x86_64.AppImage
./NewerGlow-x86_64.AppImage
```

**First run asks once for your password.** Linux restricts serial port access by
default, so the app installs a udev rule granting your console user an ACL on
your devices via systemd-logind's `uaccess` mechanism — no group membership, no
logout. It goes through PolicyKit (`pkexec`) and is a one-time action; later
updates don't prompt. Unplug and replug the device afterwards.

### Build from Source

You'll need [Rust](https://rustup.rs). The apt-get here includes the gui stack.

```sh
sudo apt-get install libudev-dev libgtk-3-dev libxkbcommon-dev \
    libxcb-shape0-dev libxcb-xfixes0-dev libssl-dev \
    libgl1-mesa-dev libvulkan-dev
cargo build --release --bin NewerGlow
./target/release/NewerGlow
```

#### Headless

```sh
cargo build --release --no-default-features --features cli --bin newerglow-cli
./target/release/newerglow-cli --install-udev
./target/release/newerglow-cli firmware.uf2
```

`--install-udev` does the same one-time PolicyKit setup as the app, so whichever
you run first covers both.

---

## Windows

### Pre-built app

Download `NewerGlow-windows-x64.zip` from the releases page, unzip it, and run
`Newer Glow.exe`. It's portable — there's no installer.

The `.exe` is currently unsigned. On first launch SmartScreen says *"Windows
protected your PC"* — click **More info** → **Run anyway**. Windows remembers
the approval per binary.

### Build from source

You'll need [Rust](https://rustup.rs).

```powershell
cargo build --release                           # builds both app and cli
.\target\release\NewerGlow.exe                  # the app
.\target\release\newerglow-cli.exe firmware.uf2 # headless
```
---

## Adding a device

Every update target is one folder under [`boards/`](boards/) holding its
manifest, 3D model, and icon. 

To add a device to your own copy without rebuilding anything, drop the same
folder into your config directory and relaunch:

| Platform | Path |
|---|---|
| macOS | `~/Library/Application Support/newerglow/boards/` |
| Linux | `~/.config/newerglow/boards/` |
| Windows | `%APPDATA%\newerglow\boards\` |

See [`boards/README.md`](boards/README.md) for the manifest format.
