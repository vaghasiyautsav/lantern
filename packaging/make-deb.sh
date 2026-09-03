#!/bin/bash
# Build lantern_<version>_<arch>.deb — run on Linux after a release build.
set -e
cd "$(dirname "$0")/.."

VERSION=0.2.0
ARCH=$(dpkg --print-architecture 2>/dev/null || echo amd64)
STAGE=$(mktemp -d)/lantern-deb

mkdir -p "$STAGE/DEBIAN" \
         "$STAGE/usr/bin" \
         "$STAGE/usr/share/applications"

for s in 512 256 128 48; do
    mkdir -p "$STAGE/usr/share/icons/hicolor/${s}x${s}/apps"
    cp "assets/icon/png/lantern-${s}.png" \
       "$STAGE/usr/share/icons/hicolor/${s}x${s}/apps/lantern.png"
done

cp target/release/lantern-gtk "$STAGE/usr/bin/"
cp target/release/lantern-gui "$STAGE/usr/bin/"
cp target/release/lantern     "$STAGE/usr/bin/"
cp target/release/lantern-doctor "$STAGE/usr/bin/"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: lantern
Version: $VERSION
Section: net
Priority: optional
Architecture: $ARCH
Depends: libgtk-4-1 (>= 4.10), libc6 (>= 2.34)
Maintainer: Lantern (local build)
Description: Serverless LAN messenger
 Encrypted peer-to-peer messaging and file transfer for local networks.
 No accounts, no cloud, no server. Native GTK4 app (lantern-gtk),
 localhost web interface (lantern-gui), CLI (lantern), and the
 network diagnostic (lantern-doctor).
EOF

cat > "$STAGE/usr/share/applications/lantern.desktop" <<'EOF'
[Desktop Entry]
Name=Lantern
Comment=Serverless LAN messenger
Exec=lantern-gtk
Icon=lantern
Terminal=false
Type=Application
Categories=Network;InstantMessaging;
Keywords=lan;chat;messenger;file;transfer;
EOF

OUT="lantern_${VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$STAGE" "$OUT"
rm -rf "$(dirname "$STAGE")"
echo "Built $(pwd)/$OUT"
