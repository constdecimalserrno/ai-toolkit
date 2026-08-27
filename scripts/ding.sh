#!/usr/bin/env bash
# Play the notification ding. Wired to the Stop + Notification hooks.
# Sound resolution, first hit wins:
#   $CLAUDE_DING_SOUND  → your own file, anywhere
#   ~/.claude/ding.mp3  → drop a file here and it just works
#   Glass.aiff          → macOS built-in fallback
# `--which` prints the resolved path instead of playing it.
set -uo pipefail

for s in "${CLAUDE_DING_SOUND:-}" "$HOME/.claude/ding.mp3" /System/Library/Sounds/Glass.aiff; do
  [[ -n "$s" && -f "$s" ]] && { sound="$s"; break; }
done
: "${sound:=}"

[[ "${1:-}" == "--which" ]] && { echo "$sound"; exit 0; }
[[ -z "$sound" ]] && exit 0

# Detached + silenced: a hook must never block the turn or spew into the transcript.
afplay "$sound" >/dev/null 2>&1 &
exit 0
