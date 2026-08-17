#!/bin/bash
# Lantern installer for macOS (and Linux).
# Builds from source, installs to ~/.lantern/bin, and on macOS creates a
# double-clickable Lantern.app in ~/Applications.
set -e

bold() { printf '\033[1m%s\033[0m\n' "$*"; }

bold "Lantern — serverless LAN messenger · installer"
echo

# 1. Compiler toolchain -------------------------------------------------------
if [ "$(uname)" = "Darwin" ] && ! xcode-select -p >/dev/null 2>&1; then
    bold "Step 1: Apple Command Line Tools are needed (one-time)."
    echo "A system dialog will open — click Install, wait for it to finish,"
    echo "then run this script again."
    xcode-select --install || true
    exit 0
fi

if [ "$(uname)" = "Linux" ]; then
    MISSING=""
    command -v cc >/dev/null 2>&1 || MISSING="build-essential"
    command -v pkg-config >/dev/null 2>&1 || MISSING="$MISSING pkg-config"
    if [ -n "$MISSING" ]; then
        bold "Step 1: build tools are needed:"
        echo "    sudo apt-get install -y $MISSING"
        echo "Run that, then run this script again."
        exit 0
    fi
    # GTK 4.10+ enables the native app; older systems still get the
    # engine + CLI + localhost interface.
    if pkg-config --atleast-version=4.10 gtk4 2>/dev/null; then
        BUILD_GTK=1
    else
        BUILD_GTK=0
        echo "note: GTK 4.10+ not found — skipping the native GTK app."
        echo "      For it, install libgtk-4-dev on Ubuntu 23.10+ / Debian 13+"
        echo "      and re-run. (Ubuntu 22.04 ships GTK 4.6 — too old.)"
    fi
fi

# 2. Rust ---------------------------------------------------------------------
if [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
if ! command -v cargo >/dev/null 2>&1; then
    bold "Step 2: installing Rust (rustup, official installer)…"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    . "$HOME/.cargo/env"
fi

# 3. Build --------------------------------------------------------------------
cd "$(dirname "$0")"
bold "Step 3: building Lantern (first build takes a few minutes)…"
cargo build --release -p lantern-gui -p lantern-cli
if [ "${BUILD_GTK:-0}" = "1" ]; then
    cargo build --release -p lantern-gtk
fi

# 4. Install ------------------------------------------------------------------
bold "Step 4: installing…"
mkdir -p "$HOME/.lantern/bin"

# Copy-then-rename, never a plain cp over the destination: an in-app update
# reinstalls while a Lantern may still be running, and Linux refuses to write
# to a busy executable (ETXTBSY) while macOS lets you corrupt one. Renaming
# swaps the directory entry, leaving any running process on its old inode —
# and unlike unlink-then-copy there is no moment where the binary is missing,
# so a failed copy can't leave the machine without a Lantern.
install_bin() { # <built file> <destination>
    tmp="$2.new.$$"
    cp "$1" "$tmp"
    chmod 755 "$tmp"
    mv -f "$tmp" "$2"
}
install_bin target/release/lantern-gui "$HOME/.lantern/bin/lantern-gui"
install_bin target/release/lantern "$HOME/.lantern/bin/lantern"
install_bin target/release/lantern-doctor "$HOME/.lantern/bin/lantern-doctor"
if [ "${BUILD_GTK:-0}" = "1" ] && [ -f target/release/lantern-gtk ]; then
    install_bin target/release/lantern-gtk "$HOME/.lantern/bin/lantern-gtk"
fi

# The updater, as a tool a person can run deliberately (`lantern-update`) as
# well as the script the in-app button hands off to. One script, two ways in.
# The checkout it was built from is baked in so it knows what to pull.
if [ -f packaging/update.sh ]; then
    tmp="$HOME/.lantern/bin/lantern-update.new.$$"
    sed "s|__REPO__|$(pwd)|" packaging/update.sh > "$tmp"
    chmod 755 "$tmp"
    mv -f "$tmp" "$HOME/.lantern/bin/lantern-update"
fi

# Linux: desktop entry + icon so Lantern appears in the app launcher.
if [ "$(uname)" = "Linux" ]; then
    for s in 512 256 128 48; do
        mkdir -p "$HOME/.local/share/icons/hicolor/${s}x${s}/apps"
        cp "assets/icon/png/lantern-${s}.png" \
           "$HOME/.local/share/icons/hicolor/${s}x${s}/apps/lantern.png"
    done
    mkdir -p "$HOME/.local/share/applications"
    if [ "${BUILD_GTK:-0}" = "1" ]; then
        LAUNCH_EXEC="$HOME/.lantern/bin/lantern-gtk"
    else
        LAUNCH_EXEC="sh -c '$HOME/.lantern/bin/lantern-gui & sleep 1; xdg-open http://localhost:3999'"
    fi
    # The filename must equal the GTK application id. A Wayland compositor
    # only knows a window by its app_id, and finds its icon by looking for
    # <app_id>.desktop — so a file named lantern.desktop leaves the running
    # app with a generic fallback icon even though search finds it fine.
    # StartupWMClass covers the same match on X11.
    rm -f "$HOME/.local/share/applications/lantern.desktop"
    cat > "$HOME/.local/share/applications/local.lantern.gtk.desktop" <<EOF
[Desktop Entry]
Name=Lantern
Comment=Serverless LAN messenger
Exec=$LAUNCH_EXEC
Terminal=false
Icon=lantern
Type=Application
Categories=Network;InstantMessaging;
StartupWMClass=local.lantern.gtk
EOF
    update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
    echo "Added Lantern to your application launcher."
fi

# 5. macOS app bundle ---------------------------------------------------------
if [ "$(uname)" = "Darwin" ]; then
    # Prefer the system /Applications (visible in Finder, Spotlight, and
    # Launchpad); fall back to ~/Applications only if it isn't writable.
    if [ -w "/Applications" ]; then
        APP="/Applications/Lantern.app"
    else
        APP="$HOME/Applications/Lantern.app"
        echo "note: /Applications not writable — installing to ~/Applications"
        echo "      (Finder: Go → Go to Folder… → ~/Applications)"
    fi
    # Build the bundle beside the old one and swap at the end, so a failed
    # compile leaves the working Lantern in place instead of a gutted one —
    # which matters most during an in-app update, where the app being
    # replaced is the app that asked for the update.
    STAGE="$APP.new"
    rm -rf "$STAGE"
    mkdir -p "$STAGE/Contents/MacOS"

    # Native SwiftUI app — no web view. Compiled locally with Apple's
    # swiftc from the Command Line Tools; no Xcode needed. Falls back to a
    # browser launcher only if swiftc is somehow unavailable.
    EXECUTABLE="Lantern"
    if command -v swiftc >/dev/null 2>&1; then
        bold "Step 5: compiling the native macOS app (SwiftUI)…"
        ARCH="$(uname -m)"
        swiftc -parse-as-library -O \
            -target "${ARCH}-apple-macos13.0" \
            -o "$STAGE/Contents/MacOS/Lantern" \
            apps/macos-native/Lantern.swift
    else
        echo "swiftc not found — falling back to browser launcher"
        EXECUTABLE="lantern-launch"
        cat > "$STAGE/Contents/MacOS/lantern-launch" <<'LAUNCH'
#!/bin/bash
PORT=3999
if ! curl -s "http://localhost:$PORT/api/me" >/dev/null 2>&1; then
    mkdir -p "$HOME/.lantern"
    nohup "$HOME/.lantern/bin/lantern-gui" --gui-port "$PORT" \
        >> "$HOME/.lantern/gui.log" 2>&1 &
    for _ in $(seq 1 40); do
        curl -s "http://localhost:$PORT/api/me" >/dev/null 2>&1 && break
        sleep 0.25
    done
fi
open "http://localhost:$PORT"
LAUNCH
        chmod +x "$STAGE/Contents/MacOS/lantern-launch"
    fi

    mkdir -p "$STAGE/Contents/Resources"
    cp assets/icon/Lantern.icns "$STAGE/Contents/Resources/Lantern.icns"

    cat > "$STAGE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Lantern</string>
  <key>CFBundleDisplayName</key><string>Lantern</string>
  <key>CFBundleIdentifier</key><string>local.lantern.gui</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleExecutable</key><string>${EXECUTABLE}</string>
  <key>CFBundleIconFile</key><string>Lantern</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSAppTransportSecurity</key>
  <dict><key>NSAllowsLocalNetworking</key><true/></dict>
</dict></plist>
PLIST
    # Swap: the old bundle only goes away once the new one is complete.
    rm -rf "$APP"
    mv "$STAGE" "$APP"
    # Nudge Finder/Dock to notice the icon.
    touch "$APP"
    echo "Created $APP"
fi

echo
bold "Done."
if [ "$(uname)" = "Darwin" ]; then
    echo "  • Double-click  Lantern  in ${APP%/Lantern.app}  (or ⌘-Space and type Lantern)"
else
    echo "  • Run: ~/.lantern/bin/lantern-gui   then open http://localhost:3999"
fi
echo "  • The window opens at http://localhost:3999 — only this machine can reach it."
echo "  • Other machines running Lantern on this network appear automatically."
echo
echo "Try it right now with a second instance on this Mac:"
echo "  ~/.lantern/bin/lantern-gui --name Second --data-dir ~/.lantern2 \\"
echo "      --discovery-port 3940 --targets 3939,3940 --gui-port 4000"
echo "  (your main instance needs --targets 3939,3940 too for same-machine chat;"
echo "   across two different machines the defaults just work)"
