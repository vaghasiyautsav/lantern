#!/bin/bash
# Lantern updater — fetch, rebuild, reinstall.
#
# This is deliberately NOT part of the app. Invariant 7 says the app never
# opens a connection off the local link, and it names update checks
# specifically: an app that polls GitHub at launch tells a third party who is
# running Lantern, from which address, and how often. That is the one thing
# the product promises it does not do.
#
# So the network access lives here, in a tool a person runs on purpose, and
# the app binary stays honest. Run it whenever you want the newest build:
#
#     lantern-update            # check, then rebuild and install
#     lantern-update --check    # report only, change nothing
#
set -e

# install.sh rewrites this to the checkout it was run from.
REPO="${LANTERN_SRC:-__REPO__}"
CHECK_ONLY=0
[ "${1:-}" = "--check" ] && CHECK_ONLY=1

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
die()  { printf '\033[1m%s\033[0m\n' "$*" >&2; exit 1; }

[ -d "$REPO/.git" ] || die "No git checkout at $REPO
Set LANTERN_SRC to your clone, e.g.  LANTERN_SRC=~/dev/lantern lantern-update"

cd "$REPO"

bold "Lantern updater · $REPO"
echo

# A dirty tree means someone is mid-edit. Rebasing over that loses work or
# stops halfway with a conflict, so refuse rather than guess.
if [ -n "$(git status --porcelain)" ]; then
    die "Working tree has uncommitted changes — commit or stash first:
$(git status --short | head -10)"
fi

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
echo "Fetching origin ($BRANCH)…"
git fetch --quiet origin "$BRANCH" || die "Fetch failed — no network, or the remote rejected us.
This is the only step that talks to the internet; the app itself never does."

LOCAL="$(git rev-parse HEAD)"
REMOTE="$(git rev-parse "origin/$BRANCH")"

if [ "$LOCAL" = "$REMOTE" ]; then
    echo "Already current at $(git rev-parse --short HEAD) — nothing to do."
    exit 0
fi

COUNT="$(git rev-list --count "HEAD..origin/$BRANCH")"
if [ "$COUNT" = "0" ]; then
    echo "You are ahead of origin by $(git rev-list --count "origin/$BRANCH..HEAD") commit(s)."
    echo "Nothing to pull. (Push with: git -C $REPO push)"
    exit 0
fi

echo
bold "$COUNT new commit(s):"
git --no-pager log --oneline --no-decorate "HEAD..origin/$BRANCH" | sed 's/^/  /'
echo

if [ "$CHECK_ONLY" = "1" ]; then
    echo "--check given, stopping here. Run without it to install."
    exit 0
fi

# Fast-forward only. A merge commit created behind the user's back is a
# surprise, and if it cannot fast-forward the history has diverged and that
# is a decision for a person, not a script.
echo "Updating…"
git merge --ff-only "origin/$BRANCH" || die "Cannot fast-forward — local history has diverged.
Sort it out by hand: git -C $REPO status"

echo
bash "$REPO/install.sh"

echo
if pgrep -x lantern-gtk >/dev/null 2>&1 || pgrep -x lantern-gui >/dev/null 2>&1; then
    bold "Lantern is running — quit and reopen it to use the new build."
fi
echo "Now at $(git rev-parse --short HEAD)."
