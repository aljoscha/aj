"""Shared helpers for the tmux-subagents skill.

Not meant to be executed directly. Imported by sibling scripts via a
``sys.path`` insert of the script's own directory.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass, asdict
from pathlib import Path
from typing import Iterable

# ---------------------------------------------------------------------------
# Detection markers.
#
# aj runs as a full inline-rendered TUI (crossterm raw mode + bracketed paste
# + Kitty keyboard protocol, but no alternate screen — so tmux scrollback
# still captures the chat history). The pane's bottom region is
# re-rendered every frame and looks like:
#
#     ... chat scrollback (scrolls up into tmux history) ...
#     [ optional spinner row: " ⠺ Working…" ]
#     ────────────   (upper editor rule)
#     [ editor body, may be multi-line ]
#     ────────────   (lower editor rule)
#     <model> @ <url>  ·  <cwd>      (footer)
#
# So state detection works by looking at the rendered pane content,
# not the last line: the footer is always last when aj is running.
# ---------------------------------------------------------------------------

# A loader frame from `aj_tui::components::loader` (DEFAULT_FRAMES) followed by
# the fixed message the event pump shows between AgentStart and AgentEnd.
WORKING_RE = re.compile(r"[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]\s+Working…")

# Editor borders are long lines of U+2500 BOX DRAWINGS LIGHT HORIZONTAL.
# Presence in a pane capture means the TUI has finished its initial render.
RULE_RE = re.compile(r"^\s*─{8,}\s*$")

STATE_WORKING = "working"
STATE_AWAITING_INPUT = "awaiting_input"
# Reserved for a future aj that gates tool execution behind a confirmation
# prompt; the current binary has no permission flow (it prints an explicit
# "no sandboxing or permission checks" banner at startup), so `detect_state`
# never emits this. Kept as a named constant so callers and `AWAITING_STATES`
# don't have to be rewritten when the state comes back.
STATE_AWAITING_PERMISSION = "awaiting_permission"
STATE_EXITED = "exited"

AWAITING_STATES = {STATE_AWAITING_INPUT, STATE_AWAITING_PERMISSION}

# ---------------------------------------------------------------------------
# Registry layout
#
#   ~/.aj/subagents/<project_slug>/<name>.json
#   ~/.aj/subagents/<project_slug>/<name>.last_send  (epoch float)
#
# project_slug = "<basename>-<short_hash_of_abs_cwd>" so we can have multiple
# unrelated projects without collisions while keeping the directory readable.
# ---------------------------------------------------------------------------


def registry_root() -> Path:
    return Path.home() / ".aj" / "subagents"


def project_slug(cwd: Path | None = None) -> str:
    cwd = (cwd or Path.cwd()).resolve()
    h = hashlib.sha1(str(cwd).encode()).hexdigest()[:8]
    base = re.sub(r"[^A-Za-z0-9_.-]", "_", cwd.name) or "root"
    return f"{base}-{h}"


def project_dir(cwd: Path | None = None) -> Path:
    d = registry_root() / project_slug(cwd)
    d.mkdir(parents=True, exist_ok=True)
    return d


def session_name(name: str) -> str:
    return f"aj-sub-{name}"


@dataclass
class AgentRecord:
    name: str
    session: str
    cwd: str
    cmd: list[str]
    task: str
    spawned_at: float

    @classmethod
    def load(cls, name: str) -> "AgentRecord":
        path = project_dir() / f"{_validate_name(name)}.json"
        if not path.exists():
            die(f"no such sub-agent: {name!r} (in {project_dir()})")
        return cls(**json.loads(path.read_text()))

    def save(self) -> None:
        path = project_dir() / f"{self.name}.json"
        path.write_text(json.dumps(asdict(self), indent=2))

    def mark_send(self) -> None:
        # Snapshot the full scrollback at send time, with the TUI's volatile
        # bottom region (spinner? + editor rules + editor body + footer)
        # stripped — those lines are re-rendered every frame and don't form
        # a stable prefix. read.py --since-last-send finds this stripped
        # snapshot as a prefix of the (also-stripped) current pane and
        # returns the suffix.
        snapshot = _strip_live_area(capture_pane(self.session, history=True))
        (project_dir() / f"{self.name}.last_send.txt").write_text(snapshot)
        (project_dir() / f"{self.name}.last_send").write_text(
            json.dumps({"time": time.time()})
        )

    def last_send_time(self) -> float | None:
        p = project_dir() / f"{self.name}.last_send"
        if not p.exists():
            return None
        try:
            return float(json.loads(p.read_text()).get("time"))
        except (ValueError, json.JSONDecodeError, TypeError):
            return None

    def last_send_snapshot(self) -> str | None:
        p = project_dir() / f"{self.name}.last_send.txt"
        return p.read_text() if p.exists() else None

    def delete(self) -> None:
        for suffix in (".json", ".last_send", ".last_send.txt"):
            p = project_dir() / f"{self.name}{suffix}"
            if p.exists():
                p.unlink()


_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$")


def _validate_name(name: str) -> str:
    if not _NAME_RE.match(name):
        die(
            f"invalid agent name {name!r}: must match {_NAME_RE.pattern}"
            " (letters/digits/_-., up to 64 chars)"
        )
    return name


def list_records() -> list[AgentRecord]:
    out: list[AgentRecord] = []
    for p in sorted(project_dir().glob("*.json")):
        try:
            out.append(AgentRecord(**json.loads(p.read_text())))
        except Exception as e:
            print(f"warning: skipping bad registry entry {p}: {e}", file=sys.stderr)
    return out


# ---------------------------------------------------------------------------
# tmux wrappers
# ---------------------------------------------------------------------------


def tmux(*args: str, check: bool = True, capture: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["tmux", *args],
        check=check,
        capture_output=capture,
        text=True,
    )


def session_alive(session: str) -> bool:
    r = tmux("has-session", "-t", session, check=False)
    return r.returncode == 0


def pane_command(session: str) -> str | None:
    """Return the foreground command of the session's active pane, or None."""
    r = tmux(
        "display-message",
        "-p",
        "-t",
        session,
        "#{pane_current_command}",
        check=False,
    )
    if r.returncode != 0:
        return None
    return r.stdout.strip() or None


def capture_pane(session: str, history: bool = False) -> str:
    """Return the pane contents (visible by default; full scrollback if ``history``)."""
    args = ["capture-pane", "-p", "-t", session]
    if history:
        args[1:1] = ["-S", "-"]  # -S - means start from beginning of history
    r = tmux(*args, check=False)
    if r.returncode != 0:
        return ""
    return r.stdout


def send_text(session: str, text: str, submit: bool = True) -> None:
    """Send a message to the aj prompt.

    Newlines in ``text`` are sent as Ctrl-J (a raw LF byte, recognized by
    ``aj_tui::keys::is_newline_event`` alongside Alt+Enter and Shift+Enter)
    so the message stays in a single submission. A final plain Enter
    submits, unless ``submit`` is False — Enter is the default
    ``tui.input.submit`` binding and isn't claimed by the newline matcher.
    """
    lines = text.split("\n")
    for i, line in enumerate(lines):
        if i > 0:
            tmux("send-keys", "-t", session, "C-j")
        if line:
            # `--` so tmux doesn't interpret lines beginning with `-` as flags.
            tmux("send-keys", "-t", session, "-l", "--", line)
    if submit:
        tmux("send-keys", "-t", session, "Enter")


def send_keys(session: str, *keys: str) -> None:
    tmux("send-keys", "-t", session, *keys)


# ---------------------------------------------------------------------------
# State detection
# ---------------------------------------------------------------------------


def _last_nonblank_line(pane: str) -> str:
    for line in reversed(pane.splitlines()):
        if line.strip():
            return line.rstrip()
    return ""


def _strip_trailing_blanks(text: str) -> str:
    lines = text.splitlines()
    while lines and not lines[-1].strip():
        lines.pop()
    return "\n".join(lines)


def _strip_live_area(pane: str) -> str:
    """Strip the TUI's volatile bottom render area from a pane capture.

    The bottom of the pane is re-rendered on every frame (optional
    spinner row, two ``────`` editor rules, the editor body, and the
    footer); those rows don't accumulate in tmux scrollback, so diffing
    pre/post-send panes only makes sense after dropping them. What's left
    is the append-only chat scrollback that `read.py --since-last-send`
    actually wants to compare.

    Layout consumed from the bottom up (each step is best-effort —
    missing rows are skipped, never fatal):

      trailing blank rows
      footer (model identifier line)
      lower editor rule(s)
      editor body
      upper editor rule(s)
      optional spinner row (" ⠺ Working…")
      blank rows that result
    """
    lines = pane.splitlines()
    # Trailing blank rows (capture-pane pads to pane height).
    while lines and not lines[-1].strip():
        lines.pop()
    if not lines:
        return ""
    # Footer (the model identifier line). Always present once the TUI has
    # rendered; if it's not there we still want to drop the last row to
    # stay symmetric with snapshots that did include it.
    lines.pop()
    # Lower editor rule (one row; loop guards against re-wrap artefacts).
    while lines and RULE_RE.match(lines[-1]):
        lines.pop()
    # Editor body — walk back until we hit the upper rule.
    while lines and not RULE_RE.match(lines[-1]):
        lines.pop()
    # Upper editor rule.
    while lines and RULE_RE.match(lines[-1]):
        lines.pop()
    # Spinner row (only present while a turn is in flight).
    if lines and WORKING_RE.search(lines[-1]):
        lines.pop()
    # Blank rows left behind by the strip.
    while lines and not lines[-1].strip():
        lines.pop()
    return "\n".join(lines)


def diff_since_snapshot(current: str, snapshot: str) -> str:
    """Return the suffix of ``current`` that came after ``snapshot``.

    ``current`` is a raw pane capture; its volatile bottom region is
    stripped here so it can be compared against the already-stripped
    ``snapshot`` written by :meth:`AgentRecord.mark_send`. If the
    snapshot is no longer a prefix (e.g. it scrolled out of tmux
    history), fall back to a tail-match over the last few snapshot
    lines, and finally to returning the stripped current unchanged.
    """
    cur = _strip_live_area(current)
    snap = _strip_trailing_blanks(snapshot)
    if not snap:
        return cur
    if cur.startswith(snap):
        return cur[len(snap):].lstrip("\n")
    # Snapshot may have aged out of scrollback. Try a line-based suffix match
    # of the last few snapshot lines instead.
    snap_lines = snap.splitlines()
    tail = "\n".join(snap_lines[-10:])
    if tail and tail in cur:
        idx = cur.index(tail) + len(tail)
        return cur[idx:].lstrip("\n")
    return cur


def detect_state(record: AgentRecord) -> tuple[str, str]:
    """Return ``(state, last_line)``.

    State machine:

    - foreground command is no longer ``aj``  → :data:`STATE_EXITED`.
    - pane contains a loader frame (" ⠺ Working…")  → :data:`STATE_WORKING`.
    - pane contains at least one editor rule  → :data:`STATE_AWAITING_INPUT`
      (the TUI is fully rendered and idle at the prompt).
    - otherwise the TUI hasn't finished its first render; report
      :data:`STATE_WORKING` so callers keep polling instead of trying
      to send before the editor is up.

    :data:`STATE_AWAITING_PERMISSION` is never emitted by this version;
    see the constant's docstring.
    """
    if not session_alive(record.session):
        return STATE_EXITED, ""
    cmd = pane_command(record.session)
    pane = capture_pane(record.session)
    last = _last_nonblank_line(pane)
    if cmd is None or cmd not in {"aj", "target/debug/aj", "target/release/aj"}:
        # Foreground is no longer aj => the binary exited (shell prompt shown).
        # We still consider this "exited" even if the tmux session lingers.
        return STATE_EXITED, last
    has_rule = False
    for line in pane.splitlines():
        if WORKING_RE.search(line):
            return STATE_WORKING, last
        if not has_rule and RULE_RE.match(line):
            has_rule = True
    if has_rule:
        return STATE_AWAITING_INPUT, last
    # TUI not finished rendering yet; keep callers in their polling loop.
    return STATE_WORKING, last


def wait_for_state(
    record: AgentRecord,
    accept: Iterable[str],
    timeout: float | None,
    poll: float = 0.5,
) -> str:
    """Block until ``record`` reaches one of ``accept`` states, or timeout.

    Returns the final state. On timeout returns the last observed state.
    """
    accept_set = set(accept)
    deadline = None if timeout is None else time.monotonic() + timeout
    state = STATE_WORKING
    while True:
        state, _ = detect_state(record)
        if state in accept_set:
            return state
        if deadline is not None and time.monotonic() >= deadline:
            return state
        time.sleep(poll)


# ---------------------------------------------------------------------------
# Misc helpers
# ---------------------------------------------------------------------------


def die(msg: str, code: int = 1) -> "None":
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(code)


def print_json(obj) -> None:
    json.dump(obj, sys.stdout, indent=2, default=str)
    sys.stdout.write("\n")


def add_path() -> None:
    """Each script calls this so it can ``import _common``."""
    here = os.path.dirname(os.path.abspath(__file__))
    if here not in sys.path:
        sys.path.insert(0, here)
