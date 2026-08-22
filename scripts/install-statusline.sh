#!/usr/bin/env bash
# Installs the Claude Code statusline: model · ctx · 5h · 7d · pace
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/scripts/statusline.py"
D=~/.claude/statusline

if [[ ! -f "$SRC" ]]; then
  echo "missing $SRC" >&2
  exit 1
fi

mkdir -p "$D"
cp "$SRC" "$D/statusline.py"
chmod +x "$D/statusline.py"

python3 - "$D/statusline.py" <<'PY'
import json, os, sys
p = os.path.expanduser("~/.claude/settings.json")
s = json.load(open(p)) if os.path.exists(p) else {}
s["statusLine"] = {"type": "command", "command": sys.argv[1], "refreshInterval": 10}
json.dump(s, open(p, "w"), indent=2)
print("wrote", p)
PY

echo "installed → $D/statusline.py (restart claude)"
