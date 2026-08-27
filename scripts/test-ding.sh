#!/usr/bin/env bash
# Asserts ding.sh picks the right sound. Run: ./scripts/test-ding.sh
set -uo pipefail
D="$(cd "$(dirname "$0")" && pwd)/ding.sh"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
fail=0
check() { [[ "$2" == "$3" ]] || { echo "FAIL $1: got '$2' want '$3'"; fail=1; }; }

mkdir -p "$tmp/.claude"
touch "$tmp/mine.wav" "$tmp/.claude/ding.mp3"

check "env var wins" \
  "$(CLAUDE_DING_SOUND="$tmp/mine.wav" HOME="$tmp" "$D" --which)" "$tmp/mine.wav"
check "~/.claude/ding.mp3 next" \
  "$(CLAUDE_DING_SOUND="$tmp/nope.wav" HOME="$tmp" "$D" --which)" "$tmp/.claude/ding.mp3"

mkdir -p "$tmp/empty"
check "falls back to Glass" \
  "$(CLAUDE_DING_SOUND= HOME="$tmp/empty" "$D" --which)" "/System/Library/Sounds/Glass.aiff"

[[ $fail == 0 ]] && echo "ding.sh ok"
exit $fail
