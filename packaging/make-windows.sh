#!/bin/bash
# Build the Windows package — from a Mac or a Linux box, no Windows needed.
#
#   bash packaging/make-windows.sh        → dist/lantern-windows-x64.zip
#
# Cross-compiles to x86_64-pc-windows-gnu with mingw-w64. This works because
# nothing in the engine binds to a platform: `ring` and bundled SQLite both
# build under mingw, and the interface is a browser rather than a native
# toolkit. Verified 18 Aug 2026 on an arm64 Mac — 52 s, clean.
#
# There is no native Windows shell yet (DESIGN §5.3 keeps WinUI as future
# work), so what ships is the engine plus the localhost interface: run
# Start-Lantern.cmd and the app is a browser window at localhost:3999. Same
# protocol, same encryption, same LAN-only promise as the Mac and Linux
# builds — the window is just glass.
#
# One-time setup:
#   rustup target add x86_64-pc-windows-gnu
#   macOS:  brew install mingw-w64
#   Ubuntu: sudo apt-get install -y mingw-w64
#
# CI (.github/workflows/build.yml) also produces this on windows-latest, built
# natively. Prefer that for anything you hand to someone else; this script is
# for a quick local .exe.
set -e

TARGET=x86_64-pc-windows-gnu
OUT=dist/lantern-windows

cd "$(dirname "$0")/.."

command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1 || {
    echo "mingw-w64 not found. Install it (see the header of this script)." >&2
    exit 1
}
rustup target list --installed | grep -qx "$TARGET" || {
    echo "Rust target missing:  rustup target add $TARGET" >&2
    exit 1
}

echo "Building for $TARGET…"
# The linker is named explicitly rather than left to a global config, so this
# script works on a machine that has never been set up for cross-compiling.
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
    cargo build --release --target "$TARGET" -p lantern-gui -p lantern-cli

rm -rf "$OUT"
mkdir -p "$OUT"
for exe in lantern-gui lantern lantern-doctor; do
    cp "target/$TARGET/release/$exe.exe" "$OUT/"
done
cp assets/icon/lantern.ico "$OUT/"

# CRLF on purpose: Notepad still shows LF-only text as one long line, and this
# is the first thing a Windows user opens.
mk_crlf() { sed 's/$/\r/' > "$1"; }

mk_crlf "$OUT/Start-Lantern.cmd" <<'CMD'
@echo off
rem Starts the engine, then opens the interface. Localhost only: nothing
rem outside this machine can reach the window.
start "" "%~dp0lantern-gui.exe"
timeout /t 2 >nul
start "" http://localhost:3999
CMD

# Start menu entry. WScript.Shell is built into Windows, so this needs
# nothing installed, and -Command avoids the execution-policy block a .ps1
# would hit. One line, PowerShell single-quotes, no escaped double quotes —
# the ^-continuation-plus-backslash-quote form is a well-known way to ship
# something that only breaks on the user's machine.
# ponytail: breaks if the install path contains a single quote. Switch to
# a here-string if that ever shows up.
mk_crlf "$OUT/Add-To-Start-Menu.cmd" <<'CMD'
@echo off
rem Puts Lantern in the Start menu. Run once. Nothing is installed or copied:
rem the shortcut points back at this folder, so keep the folder where it is.
powershell -NoProfile -Command "$s=(New-Object -ComObject WScript.Shell).CreateShortcut($env:APPDATA+'\Microsoft\Windows\Start Menu\Programs\Lantern.lnk'); $s.TargetPath='%~dp0Start-Lantern.cmd'; $s.WorkingDirectory='%~dp0'; $s.IconLocation='%~dp0lantern.ico'; $s.WindowStyle=7; $s.Description='Serverless LAN messenger'; $s.Save()"
echo Added Lantern to the Start menu.
pause
CMD

mk_crlf "$OUT/README.txt" <<'TXT'
Lantern for Windows (interim shell)
-----------------------------------
Double-click Start-Lantern.cmd. The engine starts and your browser opens
http://localhost:3999 - that page is the app, and only this machine can
reach it.

Want it in the Start menu? Run Add-To-Start-Menu.cmd once. It only makes a
shortcut back to this folder, so don't move the folder afterwards.

lantern.exe is the headless command-line client, not the app - if you got a
terminal asking you to type /msg, that's the one you opened.

Windows Firewall asks once, on first run. Allow Lantern on private
networks, or nobody on your LAN can see you: discovery needs UDP 3939, and
transfers need one more UDP port.

Nothing is installed and nothing is written outside your user folder. Your
identity key and message history live in %USERPROFILE%\.lantern - copy that
folder to move your identity, delete it to start over.

There is no native Windows window yet. Everything under the glass is the
same engine the Mac and Linux apps run: no server, no accounts, no cloud,
nothing leaves the LAN.
TXT

( cd dist && rm -f lantern-windows-x64.zip \
  && zip -qr lantern-windows-x64.zip lantern-windows )

echo
echo "dist/lantern-windows-x64.zip"
ls -lh dist/lantern-windows-x64.zip | awk '{print "  " $5}'
echo "  contents:"
( cd "$OUT" && ls -1 | sed 's/^/    /' )
