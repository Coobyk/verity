#!/usr/bin/env bash
# scripts/self-update.sh — fetch origin/main, fast-forward the working
# tree, rebuild the release binaries, and restart verity.service so the
# bridge picks up the new code. Runs from verity-self-update.timer;
# output lands in the journal (journalctl --user -u verity-self-update).
set -uo pipefail

# A pull can rewrite this very script while bash is still reading it —
# re-exec once from a private copy so the rest of the run is stable.
# The copy's $0 points into /tmp, so the real repo path rides in the env.
if [ -z "${VERITY_SELF_UPDATE_DIR:-}" ]; then
    REPO="$(cd "$(dirname "$0")/.." && pwd)" || exit 1
    tmp="$(mktemp /tmp/verity-self-update.XXXXXX.sh)"
    cp "$0" "$tmp"
    VERITY_SELF_UPDATE_DIR="$REPO" exec bash "$tmp" "$@"
fi
cd "$VERITY_SELF_UPDATE_DIR" || exit 1

log() { echo "[self-update] $*"; }

# Never touch the tree mid-rebase / mid-merge / mid-cherry-pick.
git_dir="$(git rev-parse --git-dir)"
for f in rebase-merge rebase-apply MERGE_HEAD CHERRY_PICK_HEAD; do
    [ -e "$git_dir/$f" ] && { log "$f present — another git op in flight, skipping"; exit 0; }
done

git fetch --quiet origin || { log "fetch failed — offline?"; exit 0; }

local_head="$(git rev-parse HEAD)"
remote_head="$(git rev-parse origin/main)"

[ "$local_head" = "$remote_head" ] && exit 0

if git merge-base --is-ancestor "$remote_head" HEAD; then
    log "local commits ahead of origin/main — nothing to pull"
    exit 0
fi
if ! git merge-base --is-ancestor HEAD "$remote_head"; then
    log "diverged from origin/main — leaving it for a human"
    exit 1
fi

log "updating ${local_head:0:8} → ${remote_head:0:8}"
# ff-only never merges and git refuses to touch files with local
# edits, so in-flight work in the tree survives; a conflict just skips
# this round.
if ! git merge --ff-only "$remote_head"; then
    log "ff-only refused (dirty tree over touched files) — skipping"
    exit 1
fi

if ! cargo build --release -p verity-cli -p verity-gui; then
    log "build failed — old binary keeps running"
    exit 1
fi

# try-restart only touches an active bridge; an asleep/stopped unit
# stays down and picks up the new binary on its next start.
if systemctl --user try-restart verity.service; then
    log "bridge restarted on $(git rev-parse --short HEAD)"
else
    log "restart failed"
    exit 1
fi
