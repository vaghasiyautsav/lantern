#!/bin/bash
# Lantern updater — fetch, rebuild, reinstall. One script, two ways in.
#
#   lantern-update                 check, then rebuild and install
#   lantern-update --check         report only, change nothing
#   update.sh --handoff <repo> <data-dir>
#                                  what the app's Update button hands off to
#
# Why the fetching lives here and not in the app: invariant 7. The app never
# opens a connection off the local link on its own — no polling at launch, no
# telemetry — because an app that phones GitHub reveals who runs Lantern, from
# which address, and how often. That promise is kept by putting every network
# step in this script, which runs only when a person asks for it: from a
# terminal, or by clicking Update and confirming.
#
# --handoff exists because installing replaces the binaries the app and engine
# are running from, and on Linux you cannot write to a busy executable at all.
# In that mode this script is deliberately orphaned by the engine: the app
# quits, this keeps going, and it reopens Lantern when the build is done.
#
# Two rules, in both modes:
#   • uncommitted work is never touched — a dirty tree stops everything;
#   • fast-forward only — a merge commit made behind someone's back, or a
#     divergence resolved by a script, is not this tool's decision to make.
set -u

MODE=install
case "${1:-}" in
    --check)   MODE=check;   shift ;;
    --handoff) MODE=handoff; shift ;;
esac

# install.sh rewrites __REPO__ to the checkout it was run from.
REPO="${1:-${LANTERN_SRC:-__REPO__}}"
DATA="${2:-${LANTERN_DATA_DIR:-$HOME/.lantern}}"
STATE="$DATA/update.state"
STARTED="$(date +%s)"

mkdir -p "$DATA"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }

# The state file is how the app finds out how an update went: it quits partway
# through its own update, so the run that comes next reads this. One writer,
# one shape, so the parser in core::update stays five flat fields. Message text
# is squeezed onto one line and stripped of quotes — git and cargo both emit
# multi-line output with quotes in it, and this is read as JSON.
say() { # state step message [commit]
    local msg
    msg="$(printf '%s' "$3" | tr '\n\r\t' '   ' | tr -d '"\\' | cut -c1-400)"
    printf '{"state":"%s","step":"%s","message":"%s","commit":"%s","started":"%s"}\n' \
        "$1" "$2" "$msg" "${4:-}" "$STARTED" > "$STATE"
    printf '\n=== [%s] %s: %s\n' "$(date '+%H:%M:%S')" "$2" "$3"
}

# Reopening is best-effort in both directions: an update that installed fine
# but couldn't reopen the window is still a successful update, and a failed one
# must still put back the Lantern it closed.
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
    # Only reopen what we closed. Failing before that changed nothing, and
    # launching an app nobody asked for would be its own bug.
    [ "${CLOSED:-0}" = "1" ] && relaunch
    exit 1
}

echo "=================================================================="
echo "Lantern update · $(date) · repo $REPO"
echo "=================================================================="

cd "$REPO" 2>/dev/null || fail start "The source checkout at $REPO is gone.
Set LANTERN_SRC to your clone, e.g.  LANTERN_SRC=~/dev/lantern lantern-update"
command -v git >/dev/null 2>&1 || fail start "git isn't installed."
[ -d "$REPO/.git" ] || fail start "No git checkout at $REPO."

# 1. Refuse to touch a dirty tree ------------------------------------------
if [ -n "$(git status --porcelain 2>/dev/null)" ]; then
    fail start "There are uncommitted changes in $REPO. Lantern won't touch them — commit or stash them first:
$(git status --short | head -10)"
fi

BRANCH="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo main)"
BEFORE="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"

# Update whatever this branch tracks, not an assumed origin/main — and refuse
# outright on a branch that exists only on this machine, rather than fetching a
# ref that was never pushed.
UPSTREAM="$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null || true)"
if [ -z "$UPSTREAM" ]; then
    fail start "Branch $BRANCH isn't tracking anything on GitHub, so there's nothing to update from. Switch to main and try again."
fi
REMOTE="${UPSTREAM%%/*}"

# 2. Fetch ------------------------------------------------------------------
say running fetch "Fetching $UPSTREAM"
if ! OUT="$(git fetch "$REMOTE" 2>&1)"; then
    fail fetch "Couldn't reach GitHub: $OUT
This is the only step that talks to the internet; the app itself never does."
fi
echo "$OUT"

COUNT="$(git rev-list --count "HEAD..$UPSTREAM" 2>/dev/null || echo 0)"
if [ "$COUNT" = "0" ]; then
    AHEAD="$(git rev-list --count "$UPSTREAM..HEAD" 2>/dev/null || echo 0)"
    if [ "$AHEAD" != "0" ]; then
        echo "You are ahead of $UPSTREAM by $AHEAD commit(s) — nothing to pull."
        echo "(Push with: git -C $REPO push)"
    fi
    say ok done "Already up to date at $BEFORE" "$BEFORE"
    exit 0
fi

echo
bold "$COUNT new commit(s):"
git --no-pager log --oneline --no-decorate "HEAD..$UPSTREAM" | sed 's/^/  /'
echo

if [ "$MODE" = "check" ]; then
    echo "--check given, stopping here. Run without it to install."
    exit 0
fi

# 3. Get out of the app's way (handoff only) --------------------------------
# In terminal mode nothing is killed: installs are copy-then-rename, so a
# running Lantern keeps working on its old inode until someone restarts it.
if [ "$MODE" = "handoff" ]; then
    say running wait "Waiting for Lantern to quit"
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        pgrep -f 'lantern-gui' >/dev/null 2>&1 || break
        sleep 1
    done
    pkill -f 'lantern-gui' >/dev/null 2>&1 || true
    pkill -f 'lantern-gtk' >/dev/null 2>&1 || true
    CLOSED=1
    sleep 1
fi

# 4. Fast-forward and build -------------------------------------------------
say running merge "Fast-forwarding to $UPSTREAM"
if ! OUT="$(git merge --ff-only "$UPSTREAM" 2>&1)"; then
    fail merge "Couldn't fast-forward: $OUT — local history has diverged; sort it out by hand: git -C $REPO status"
fi
echo "$OUT"
AFTER="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"

say running build "Building $AFTER (this takes a few minutes)"
if ! bash "$REPO/install.sh"; then
    fail build "The build failed. Nothing was replaced — the log above says where it stopped. Your previous Lantern still works."
fi

# 5. Finish -----------------------------------------------------------------
if [ "$MODE" = "handoff" ]; then
    say running relaunch "Reopening Lantern" "$AFTER"
    relaunch
elif pgrep -f 'lantern-gtk' >/dev/null 2>&1 || pgrep -f 'lantern-gui' >/dev/null 2>&1; then
    bold "Lantern is running — quit and reopen it to use the new build."
fi

say ok done "Updated to $AFTER" "$AFTER"
echo "Now at $AFTER."
