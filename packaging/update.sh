#!/bin/bash
# lantern-update — pull the latest Lantern from GitHub and reinstall it.
#
#   update.sh <repo-dir> <data-dir>
#
# Started detached by the engine (lantern_core::update::start), never as a
# child of the app, because it replaces the binaries the app and engine are
# running from — on Linux you cannot overwrite a running executable at all.
# Being orphaned is the point: the app quits, this keeps going, and it opens
# the new Lantern when it's done.
#
# Contract with the shells:
#   • every step appends to the log this script's stdout is redirected to;
#   • the outcome lands in <data-dir>/update.state, which the relaunched app
#     reads to tell the person how their update went;
#   • uncommitted work is never touched — a dirty tree aborts before fetch.
set -u

REPO="${1:?repo dir required}"
DATA="${2:?data dir required}"
STATE="$DATA/update.state"
STARTED="$(date +%s)"

mkdir -p "$DATA"

# One writer, one shape, so the parser in update.rs stays five flat fields.
# Message text is squeezed onto one line and stripped of quotes/backslashes:
# git and cargo both emit multi-line output with quotes in it, and this file
# is read as JSON.
say() { # state step message [commit]
    local msg
    msg="$(printf '%s' "$3" | tr '\n\r\t' '   ' | tr -d '"\\' | cut -c1-400)"
    printf '{"state":"%s","step":"%s","message":"%s","commit":"%s","started":"%s"}\n' \
        "$1" "$2" "$msg" "${4:-}" "$STARTED" > "$STATE"
    printf '\n=== [%s] %s: %s\n' "$(date '+%H:%M:%S')" "$2" "$3"
}

# Reopening is best-effort in both directions: an update that installed fine
# but couldn't reopen the window is still a successful update, and a failed
# one must still put back the Lantern it closed.
relaunch() {
    if [ "$(uname)" = "Darwin" ]; then
        open -a /Applications/Lantern.app 2>/dev/null \
            || open -a "$HOME/Applications/Lantern.app" 2>/dev/null || true
    elif [ -x "$HOME/.lantern/bin/lantern-gtk" ]; then
        nohup "$HOME/.lantern/bin/lantern-gtk" >/dev/null 2>&1 &
    elif [ -x "$HOME/.lantern/bin/lantern-gui" ]; then
        nohup "$HOME/.lantern/bin/lantern-gui" >/dev/null 2>&1 &
    fi
}

fail() {
    say failed "$1" "$2"
    # Only reopen if we'd already closed it — failing before that changed
    # nothing, and launching an app nobody asked for would be its own bug.
    [ "${CLOSED:-0}" = "1" ] && relaunch
    exit 1
}

echo "=================================================================="
echo "Lantern update · $(date) · repo $REPO"
echo "=================================================================="

cd "$REPO" 2>/dev/null || fail start "The source checkout at $REPO is gone."
command -v git >/dev/null 2>&1 || fail start "git isn't installed."

# 1. Refuse to touch a dirty tree ------------------------------------------
if [ -n "$(git status --porcelain 2>/dev/null)" ]; then
    fail start "There are uncommitted changes in $REPO. Lantern won't touch them — commit or stash them first."
fi

BRANCH="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo main)"
BEFORE="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"

# 2. Wait for the app to let go of its binaries ----------------------------
# The app quits itself right after asking for this; give it a moment, then
# stop the engine so the install step can replace it. Killing it here is safe:
# messages in flight are the peer's problem to retry, and the new engine
# starts with the same identity and history.
say running wait "Waiting for Lantern to quit"
for _ in 1 2 3 4 5 6 7 8 9 10; do
    pgrep -f 'lantern-gui' >/dev/null 2>&1 || break
    sleep 1
done
pkill -f 'lantern-gui' >/dev/null 2>&1 || true
pkill -f 'lantern-gtk' >/dev/null 2>&1 || true
CLOSED=1
sleep 1

# 3. Fetch and fast-forward ------------------------------------------------
say running fetch "Fetching $BRANCH from GitHub"
if ! OUT="$(git fetch origin "$BRANCH" 2>&1)"; then
    fail fetch "Couldn't reach GitHub: $OUT"
fi
echo "$OUT"

say running merge "Fast-forwarding to origin/$BRANCH"
if ! OUT="$(git merge --ff-only "origin/$BRANCH" 2>&1)"; then
    fail merge "Couldn't fast-forward: $OUT — the branch has diverged; sort it out by hand."
fi
echo "$OUT"
AFTER="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"

if [ "$BEFORE" != "$AFTER" ]; then
    say running build "Building $AFTER (this takes a few minutes)"
    if ! bash "$REPO/install.sh"; then
        fail build "The build failed. Nothing was replaced — the log above says where it stopped. Your previous Lantern still works."
    fi
    say running relaunch "Reopening Lantern" "$AFTER"
fi

# 4. Reopen ---------------------------------------------------------------
relaunch

if [ "$BEFORE" = "$AFTER" ]; then
    say ok done "Already up to date at $AFTER" "$AFTER"
else
    say ok done "Updated to $AFTER" "$AFTER"
fi
echo "Update finished at $(date)"
