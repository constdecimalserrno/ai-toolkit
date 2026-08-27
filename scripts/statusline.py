#!/usr/bin/env python3
"""Statusline: Opus 5·xhigh │ ctx 42% 84k │ 5h 18% ⟳1h30m │ 7d 55% ⟳4d │ pace -12%

Everything comes from the JSON Claude Code pipes on stdin. Stdlib only, never crashes.
pace = even-burn line (elapsed % of 7d window) minus actual 7d usage.
+ = room to spend, - = burning too fast.
"""

import json
import os
import sys
import time

NO_COLOR = bool(os.environ.get("NO_COLOR"))
_SGR = {"reset": "0", "dim": "2", "bold": "1",
        "green": "32", "yellow": "33", "red": "31", "bold_red": "1;31"}


def paint(text, key):
    return text if NO_COLOR else "\033[{}m{}\033[0m".format(_SGR[key], text)


CTX_THRESHOLDS = [(60, "green"), (80, "yellow"), (90, "red"), (101, "bold_red")]
WINDOW_THRESHOLDS = [(50, "green"), (80, "yellow"), (90, "red"), (101, "bold_red")]
PACE_THRESHOLDS = [(5, "yellow"), (15, "red"), (101, "bold_red")]
SEVEN_DAYS = 7 * 24 * 3600


def colour_for(pct, table):
    for ceiling, colour in table:
        if pct < ceiling:
            return colour
    return "bold_red"


def fmt_tokens(n):
    try:
        n = int(n)
    except (TypeError, ValueError):
        return ""
    if n >= 1000000:
        return "{:.1f}M".format(n / 1000000.0)
    if n >= 1000:
        return "{}k".format(int(round(n / 1000.0)))
    return str(n)


def fmt_reset(epoch_seconds):
    try:
        delta = int(epoch_seconds) - int(time.time())
    except (TypeError, ValueError):
        return ""
    if delta <= 0:
        return "now"
    d, rem = divmod(delta, 86400)
    h, rem = divmod(rem, 3600)
    m, _ = divmod(rem, 60)
    if d:
        return "{}d{}h".format(d, h) if h else "{}d".format(d)
    if h:
        return "{}h{}m".format(h, m) if m else "{}h".format(h)
    return "{}m".format(m) if m else "<1m"


def pct_num(value):
    if value is None:
        return None
    try:
        return int(round(float(value)))
    except (TypeError, ValueError):
        return None


def seg_model(data):
    model = (data.get("model") or {}).get("display_name") or "?"
    effort = (data.get("effort") or {}).get("level")
    label = paint(model, "bold")
    return label + paint("·" + effort, "dim") if effort else label


def seg_context(data):
    cw = data.get("context_window") or {}
    pct = pct_num(cw.get("used_percentage"))
    if pct is None:
        return paint("ctx --", "dim")
    body = paint("ctx {}%".format(pct), colour_for(pct, CTX_THRESHOLDS))
    tok = fmt_tokens(cw.get("total_input_tokens"))
    return body + " " + paint(tok, "dim") if tok else body


def seg_window(rate_limits, key, label):
    window = (rate_limits or {}).get(key)
    if not isinstance(window, dict):
        return None
    pct = pct_num(window.get("used_percentage"))
    if pct is None:
        return None
    body = paint("{} {}%".format(label, pct), colour_for(pct, WINDOW_THRESHOLDS))
    reset = fmt_reset(window.get("resets_at"))
    if reset and reset != "now":
        body += " " + paint("⟳" + reset, "dim")
    return body


def seg_pace(rate_limits):
    window = (rate_limits or {}).get("seven_day")
    if not isinstance(window, dict):
        return None
    try:
        used = float(window.get("used_percentage"))
        remaining = int(window.get("resets_at")) - int(time.time())
    except (TypeError, ValueError):
        return None
    elapsed = max(0, min(SEVEN_DAYS, SEVEN_DAYS - remaining))
    delta = int(round(elapsed / float(SEVEN_DAYS) * 100.0 - used))
    if delta > 0:
        text, colour = "+{}%".format(delta), "green"
    elif delta < 0:
        text, colour = "{}%".format(delta), colour_for(-delta, PACE_THRESHOLDS)
    else:
        text, colour = "±0%", "green"
    return paint("pace " + text, colour)


def seg_plan(data):
    rl = data.get("rate_limits")
    if not isinstance(rl, dict):
        return [paint("plan n/a", "dim")]
    parts = [s for s in (seg_window(rl, "five_hour", "5h"),
                         seg_window(rl, "seven_day", "7d"),
                         seg_pace(rl)) if s]
    return parts or [paint("plan n/a", "dim")]


def dump_usage(data):
    """One TSV row for `monitor`: epoch, 5h %, 5h reset, 7d %, 7d reset, pace."""
    rl = data.get("rate_limits")
    if not isinstance(rl, dict):
        return
    five, seven = rl.get("five_hour") or {}, rl.get("seven_day") or {}
    five_pct, seven_pct = pct_num(five.get("used_percentage")), pct_num(seven.get("used_percentage"))
    if five_pct is None or seven_pct is None:
        return
    try:
        remaining = int(seven.get("resets_at")) - int(time.time())
    except (TypeError, ValueError):
        return
    elapsed = max(0, min(SEVEN_DAYS, SEVEN_DAYS - remaining))
    pace = int(round(elapsed / float(SEVEN_DAYS) * 100.0 - seven_pct))
    row = "\t".join(str(x) for x in (
        int(time.time()), five_pct, fmt_reset(five.get("resets_at")) or "?",
        seven_pct, fmt_reset(seven.get("resets_at")) or "?", pace))
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "usage.txt")
    tmp = path + ".tmp"
    with open(tmp, "w") as fh:
        fh.write(row + "\n")
    os.replace(tmp, path)


def main():
    try:
        raw = sys.stdin.read()
        data = json.loads(raw) if raw.strip() else {}
        if not isinstance(data, dict):
            raise ValueError("payload is not an object")
        try:
            dump_usage(data)
        except Exception:
            pass
        sys.stdout.write(paint(" │ ", "dim").join(
            [seg_model(data), seg_context(data)] + seg_plan(data)))
    except Exception:
        sys.stdout.write(paint("statusline", "dim"))
    sys.stdout.write("\n")
    sys.stdout.flush()


if __name__ == "__main__":
    main()
