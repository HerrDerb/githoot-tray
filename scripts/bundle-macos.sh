#!/usr/bin/env bash
#
# Wraps an already-built macOS binary in a .app bundle, ad-hoc signs it, and zips it.
#
# Lives in the repo rather than inline in the release workflow so that what CI produces can be
# reproduced byte-for-byte on a developer's Mac. The bundle is not cosmetic: `LSUIElement` can only
# be set in an Info.plist, and without it the app takes a Dock icon and a menu-bar application name
# — wrong for something whose whole UI is one status item.
#
# Usage: scripts/bundle-macos.sh <binary> <out-dir> <version>
#   e.g. scripts/bundle-macos.sh target/release/git-system-tray dist 1.4.1

set -euo pipefail

USAGE="usage: bundle-macos.sh <binary> <out-dir> <version>"
BIN="${1:?$USAGE}"
OUT="${2:?$USAGE}"
VERSION="${3:?$USAGE}"

NAME="git-system-tray"
BUNDLE_ID="com.github.herrderb.git-system-tray"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

APP="$OUT/$NAME.app"
ZIP="$OUT/${NAME}-macos-aarch64.zip"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "error: this script needs macOS (sips, iconutil, codesign, ditto)" >&2
  exit 1
fi

if [ ! -f "$BIN" ]; then
  echo "error: no such binary: $BIN" >&2
  exit 1
fi

# ── Bundle skeleton ───────────────────────────────────────────────────────────
# Removed first, not overwritten: a stale file left behind from a previous layout would be signed
# and shipped along with everything else.
rm -rf "$APP" "$ZIP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BIN" "$APP/Contents/MacOS/$NAME"
chmod +x "$APP/Contents/MacOS/$NAME"

# ── Info.plist ────────────────────────────────────────────────────────────────
# LSUIElement is the reason this bundle exists at all: menu-bar only, no Dock icon, no app menu.
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>$NAME</string>
	<key>CFBundleDisplayName</key>
	<string>GitHub Tray Icon</string>
	<key>CFBundleIdentifier</key>
	<string>$BUNDLE_ID</string>
	<key>CFBundleExecutable</key>
	<string>$NAME</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
	<key>LSUIElement</key>
	<true/>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
PLIST

# ── Icon ──────────────────────────────────────────────────────────────────────
# Only ever seen in Finder's Get Info and the Login Items list — the menu-bar image comes from the
# embedded PNGs at runtime, not from here. So this is best effort: a bundle with a dangling
# CFBundleIconFile just falls back to a generic icon, which is not worth failing a release over.
#
# The source is 98x96, so squaring it stretches it by about 2%. Visible only if you go looking.
SOURCE_ICON="$REPO_ROOT/assets/github.png"
ICONSET="$OUT/AppIcon.iconset"
if [ -f "$SOURCE_ICON" ] && command -v iconutil >/dev/null 2>&1; then
  rm -rf "$ICONSET"
  mkdir -p "$ICONSET"
  icon_ok=1
  for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$SOURCE_ICON" \
      --out "$ICONSET/icon_${size}x${size}.png" >/dev/null 2>&1 || icon_ok=0
    retina=$((size * 2))
    sips -z "$retina" "$retina" "$SOURCE_ICON" \
      --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null 2>&1 || icon_ok=0
  done
  if [ "$icon_ok" = 1 ] \
    && iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns" >/dev/null 2>&1; then
    echo "icon:    AppIcon.icns"
  else
    echo "icon:    skipped (generation failed); bundle is still valid" >&2
  fi
  rm -rf "$ICONSET"
else
  echo "icon:    skipped (no source PNG or no iconutil); bundle is still valid" >&2
fi

# ── Signature ─────────────────────────────────────────────────────────────────
# Ad-hoc (`--sign -`): no Apple Developer account, no CI secrets. It does NOT clear Gatekeeper —
# a downloaded copy still needs `xattr -dr com.apple.quarantine` — but it gives the bundle a stable
# code identity, which is what macOS keys per-app permissions off.
#
# No `--deep`: Apple deprecated it for signing, and there is nothing nested here to reach anyway.
codesign --force --sign - "$APP"
codesign --verify --strict "$APP"

# ── In-zip README ─────────────────────────────────────────────────────────────
# Gatekeeper quarantines an ad-hoc-signed download and refuses to open it until the quarantine
# attribute is stripped. README.md documents the fix, but a user who downloaded straight from the
# Releases page may never see that file — this puts the same instructions where they'll actually
# be found. Keep this in sync with README.md's "macOS" section if that changes.
STAGE="$OUT/zip-stage"
rm -rf "$STAGE"
mkdir -p "$STAGE"
ditto "$APP" "$STAGE/$NAME.app"

cat > "$STAGE/README.txt" <<TXT
git-system-tray $VERSION (macOS, Apple Silicon)
================================================

This build is ad-hoc signed, not notarized -- that would need a paid Apple Developer
account -- so Gatekeeper quarantines it after download and refuses to open it.

Clear the quarantine flag once, then launch normally:

    xattr -dr com.apple.quarantine git-system-tray.app
    open git-system-tray.app

Notes:
- For the review indicator to work, gh must be findable. An app launched from Finder
  inherits a minimal PATH, so it also checks /opt/homebrew/bin and /usr/local/bin.
- Only Apple Silicon (aarch64) is built here. On Intel, "cargo build --release" from
  source works fine.
TXT

# ── Zip ───────────────────────────────────────────────────────────────────────
# `ditto`, not `zip`: it is the only one that reliably preserves the bundle's structure and the
# executable bit through a round trip, which is the difference between an app that opens and one
# that reports itself as damaged.
#
# No `--keepParent`: the source here is the staging directory, and we want its *contents*
# (the .app and the README side by side) at the zip's top level, not the staging dir itself.
ditto -c -k "$STAGE" "$ZIP"
rm -rf "$STAGE"

echo "bundle:  $APP"
echo "zip:     $ZIP"
echo "version: $VERSION"
