#!/usr/bin/env bash
# Installs the ding: copies scripts/ding.sh → ~/.claude/ and wires the
# Stop + Notification hooks in ~/.claude/settings.json. Doesn't wipe the rest.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/scripts/ding.sh"
D=~/.claude

if [[ ! -f "$SRC" ]]; then
  echo "missing $SRC" >&2
  exit 1
fi

mkdir -p "$D"
cp "$SRC" "$D/ding.sh"
chmod +x "$D/ding.sh"

python3 - "$D/ding.sh" <<'PY'
import json, os, sys
cmd = sys.argv[1]
p = os.path.expanduser("~/.claude/settings.json")
s = json.load(open(p)) if os.path.exists(p) else {}
hooks = s.setdefault("hooks", {})

# Stop = agent finished a turn. Notification = permission prompt / idle nudge.
for event in ("Stop", "Notification"):
    groups = hooks.setdefault(event, [])
    # Idempotent: drop any prior ding entry, then re-add one clean copy.
    for g in groups:
        g["hooks"] = [h for h in g.get("hooks", []) if "ding.sh" not in str(h.get("command", ""))]
    groups[:] = [g for g in groups if g.get("hooks")]
    groups.append({"hooks": [{"type": "command", "command": cmd, "async": True}]})

json.dump(s, open(p, "w"), indent=2)
print("wrote", p)
PY

echo "installed → $D/ding.sh (restart claude, or open /hooks once)"
