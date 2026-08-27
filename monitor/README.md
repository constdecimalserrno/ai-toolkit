# monitor

`monitor` — ratatui system dashboard: CPU (global + per-core), RAM/swap, memory by process, disks, disk usage by project, Claude sessions (context size + what each is working on), per-process network (via `nettop`), Claude plan usage, clock. Keeps the Mac awake while it runs (`caffeinate -dimsu -w <pid>`).

```
cargo install --path monitor   # from the ai-toolkit root; installs `monitor` to ~/.cargo/bin
monitor                  # q/esc to quit
```

Sessions come from `~/.claude/projects/*/*.jsonl` (touched in the last 30 min), parsed by an embedded `python3` snippet every 15s: context tokens from the last reply's usage, the label from the first user message. AI processes (highlighted in the network table) are matched by name/exe against `AI_HINTS` in `src/main.rs`. Network rates are macOS-only. Project sizes come from `du -sk` over `$MONITOR_PROJECT_ROOT` (default `~/Documents`) in a background thread (a directory counts as a project if it holds `.git`, `context/`, `docs/`, or `CLAUDE.md`; descent stops there). Cold scan ~2.5 min, refresh every 30 min.

The CLAUDE panel (5h / 7d gauges + pace) reads `~/.claude/statusline/usage.txt`, which `~/.claude/statusline/statusline.py` writes on every statusline refresh — so it only updates while a Claude Code session is open, and goes `(stale)` after 5 min. Format is one tab-separated row: `epoch, 5h %, 5h reset, 7d %, 7d reset, pace`.
