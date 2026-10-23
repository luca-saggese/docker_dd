#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="Docker Daisy"
VERSION="${VERSION:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -n 1)}"
VERSION="${VERSION#v}"
IDENTITY="${DEVELOPER_ID_APPLICATION:-}"
OUTPUT_DIR="${OUTPUT_DIR:-$ROOT_DIR/dist}"
APP_PATH="$OUTPUT_DIR/$APP_NAME.app"
DMG_PATH="$OUTPUT_DIR/$APP_NAME-$VERSION-macos.dmg"
STAGING_DIR="$(mktemp -d "${TMPDIR:-/tmp}/docker-daisy-dmg.XXXXXX")"
MOUNT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/docker-daisy-mount.XXXXXX")"

cleanup() {
  hdiutil detach "$MOUNT_DIR" -quiet >/dev/null 2>&1 || true
  rm -rf "$STAGING_DIR" "$MOUNT_DIR"
}
trap cleanup EXIT

command -v cargo >/dev/null || { echo "Rust (cargo) is required." >&2; exit 2; }
command -v codesign >/dev/null || { echo "This script requires macOS codesign." >&2; exit 2; }
command -v hdiutil >/dev/null || { echo "This script requires macOS hdiutil." >&2; exit 2; }
[[ -f "$ROOT_DIR/assets/docker-daisy.icns" ]] || { echo "Missing app icon: assets/docker-daisy.icns" >&2; exit 2; }
if [[ -n "${NOTARY_PROFILE:-}" && -z "$IDENTITY" ]]; then
  echo "Notarization requires DEVELOPER_ID_APPLICATION." >&2
  exit 2
fi

cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml"
mkdir -p "$OUTPUT_DIR"
rm -rf "$APP_PATH"
mkdir -p "$APP_PATH/Contents/MacOS" "$APP_PATH/Contents/Resources"
cp "$ROOT_DIR/target/release/docker-daisy" "$APP_PATH/Contents/MacOS/docker-daisy"
cp "$ROOT_DIR/assets/docker-daisy.icns" "$APP_PATH/Contents/Resources/docker-daisy.icns"
cat > "$APP_PATH/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>docker-daisy</string>
  <key>CFBundleIdentifier</key><string>app.docker-daisy</string>
  <key>CFBundleName</key><string>Docker Daisy</string>
  <key>CFBundleDisplayName</key><string>Docker Daisy</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleIconFile</key><string>docker-daisy.icns</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
printf 'APPL????' > "$APP_PATH/Contents/PkgInfo"

if [[ -n "$IDENTITY" ]]; then
  codesign --force --deep --options runtime --timestamp --sign "$IDENTITY" "$APP_PATH"
else
  # Ad-hoc signing makes the local app bundle launchable; it is not Developer ID signing.
  codesign --force --deep --options runtime --sign - "$APP_PATH"
fi
codesign --verify --deep --strict --verbose=2 "$APP_PATH"

cp -R "$APP_PATH" "$STAGING_DIR/"
ln -s /Applications "$STAGING_DIR/Applications"
rm -f "$DMG_PATH"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGING_DIR" -ov -format UDZO "$DMG_PATH"

if [[ -n "$IDENTITY" ]]; then
  codesign --force --timestamp --sign "$IDENTITY" "$DMG_PATH"
  codesign --verify --verbose=2 "$DMG_PATH"
fi

hdiutil attach "$DMG_PATH" -readonly -nobrowse -mountpoint "$MOUNT_DIR" >/dev/null
[[ -d "$MOUNT_DIR/$APP_NAME.app" ]] || { echo "DMG is missing the app bundle." >&2; exit 1; }
[[ -L "$MOUNT_DIR/Applications" ]] || { echo "DMG is missing the Applications shortcut." >&2; exit 1; }
hdiutil detach "$MOUNT_DIR" -quiet >/dev/null

if [[ -n "${NOTARY_PROFILE:-}" ]]; then
  xcrun notarytool submit "$DMG_PATH" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$DMG_PATH"
  xcrun stapler validate "$DMG_PATH"
fi

if command -v shasum >/dev/null; then
  LC_ALL=C shasum -a 256 "$DMG_PATH"
fi
if [[ -n "$IDENTITY" ]]; then
  echo "Created Developer ID signed disk image: $DMG_PATH"
else
  echo "Created local ad-hoc signed app and unsigned disk image: $DMG_PATH"
  echo "Set DEVELOPER_ID_APPLICATION to sign the app and DMG for distribution."
fi
echo "Created app bundle: $APP_PATH"
