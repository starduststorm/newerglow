#!/usr/bin/env bash
# Build the macOS .app bundle locally. Mirrors the assembly the CI
# release workflow does, minus the universal lipo + signing steps —
# this produces a current-arch unsigned bundle, fine for local testing
# of the user-visible app (window title, app menu, About dialog icon).
#
# Output: dist/Newer Glow.app
#
# Usage:
#   scripts/build-app.sh              # release build
#   scripts/build-app.sh --debug      # debug build
#   scripts/build-app.sh --open       # build, then `open` the bundle

set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
    echo "error: build-app.sh only builds macOS .app bundles" >&2
    exit 1
fi

profile="release"
do_open=0
for arg in "$@"; do
    case "$arg" in
        --debug) profile="debug" ;;
        --open) do_open=1 ;;
        *) echo "unknown arg: $arg" >&2; exit 2 ;;
    esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo_args=(--bin NewerGlow)
if [[ "$profile" = "release" ]]; then
    cargo_args+=(--release)
fi
cargo build "${cargo_args[@]}"

VERSION=$(grep -E '^version\s*=\s*"' Cargo.toml | head -1 | sed -E 's/.*"([^"]+)".*/\1/')
APP="dist/Newer Glow.app"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$profile/NewerGlow" "$APP/Contents/MacOS/Newer Glow"
chmod +x "$APP/Contents/MacOS/Newer Glow"
cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
sed "s/__VERSION__/${VERSION}/g" assets/Info.plist > "$APP/Contents/Info.plist"

echo "built: $APP (version ${VERSION}, profile ${profile})"

if [[ "$do_open" = 1 ]]; then
    open "$APP"
fi
