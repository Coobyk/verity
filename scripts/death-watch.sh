#!/usr/bin/env bash
# Watch ~/verity/deaths for new *.md reports and post them into Healthity's
# topic as document files via `verity send --file`. Sent filenames are
# tracked in $STATE so restarts never repost.
set -u
DEATHS=/data/home/verity/deaths
STATE=/data/home/.local/state/verity-healthity-sent
VERITY=/data/home/verity/target/release/verity
mkdir -p "$(dirname "$STATE")"; touch "$STATE"
while true; do
  for f in "$DEATHS"/*.md; do
    [ -e "$f" ] || continue
    base=$(basename "$f")
    grep -qxF "$base" "$STATE" && continue
    if "$VERITY" send Healthity "$f" --file >/dev/null 2>&1; then
      echo "$base" >> "$STATE"
    fi
  done
  sleep 10
done
