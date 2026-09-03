#!/bin/bash
# Build Lantern.dmg — run ON A MAC, after (or instead of) install.sh.
# Produces a drag-to-Applications disk image you can hand to any Mac.
#
#   bash packaging/make-dmg.sh
#
# Note on other Macs: the app is unsigned (no Apple Developer account),
# so the first launch there is right-click → Open → Open. After that it
# opens normally. Building on the target Mac via install.sh avoids even
# that, since locally-built apps aren't quarantined.
set -e
cd "$(dirname "$0")/.."

if [ "$(uname)" != "Darwin" ]; then
    echo "This script needs macOS (hdiutil, swiftc)."; exit 1
fi

# 1. Build everything.
if [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
cargo build --release -p lantern-gui -p lantern-cli

# 2. Stage a self-contained app bundle. Unlike the installed app (which
#    runs the engine out of ~/.lantern/bin), the DMG app carries the
#    engine inside Contents/Resources and installs it to ~/.lantern/bin
#    on first launch.
STAGE="$(mktemp -d)/Lantern"
APP="$STAGE/Lantern.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

ARCH="$(uname -m)"
swiftc -parse-as-library -O -target "${ARCH}-apple-macos13.0" \
    -o "$APP/Contents/MacOS/LanternShell" apps/macos-native/Lantern.swift

cp target/release/lantern-gui "$APP/Contents/Resources/"
cp target/release/lantern "$APP/Contents/Resources/"
cp assets/icon/Lantern.icns "$APP/Contents/Resources/Lantern.icns"

# First-launch bootstrap: copy the bundled engine into ~/.lantern/bin
# (where the shell expects it), then exec the native shell.
cat > "$APP/Contents/MacOS/Lantern" <<'BOOT'
#!/bin/bash
HERE="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$HOME/.lantern/bin"
for b in lantern-gui lantern; do
    if [ ! -x "$HOME/.lantern/bin/$b" ] || \
       ! cmp -s "$HERE/Resources/$b" "$HOME/.lantern/bin/$b"; then
        cp "$HERE/Resources/$b" "$HOME/.lantern/bin/$b"
    fi
done
exec "$HERE/MacOS/LanternShell"
BOOT
chmod +x "$APP/Contents/MacOS/Lantern"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Lantern</string>
  <key>CFBundleDisplayName</key><string>Lantern</string>
  <key>CFBundleIdentifier</key><string>local.lantern.gui</string>
  <key>CFBundleVersion</key><string>0.2.0</string>
  <key>CFBundleShortVersionString</key><string>0.2.0</string>
  <key>CFBundleExecutable</key><string>Lantern</string>
  <key>CFBundleIconFile</key><string>Lantern</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSAppTransportSecurity</key>
  <dict><key>NSAllowsLocalNetworking</key><true/></dict>
</dict></plist>
PLIST

# 3. Drag-to-install layout + image.
ln -s /Applications "$STAGE/Applications"
rm -f Lantern.dmg
hdiutil create -volname "Lantern" -srcfolder "$STAGE" -ov -format UDZO Lantern.dmg
rm -rf "$(dirname "$STAGE")"
echo
echo "Built $(pwd)/Lantern.dmg"
