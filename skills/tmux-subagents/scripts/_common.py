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
# Detection markers (see skills/tmux-subagents/SKILL.md for rationale).
# aj is a line-oriented CLI using rustyline + termimad. `tmux capture-pane -p`
# strips ANSI, so these literal strings appear verbatim in pane snapshots.
# ---------------------------------------------------------------------------

IDLE_RE = re.compile(r"^you: ?$")
PERMISSION_RE = re.compile(r"^Allow this command\? \(y/n\): ?$")

STATE_WORKING = "working"
STATE_AWAITING_INPUT = "awaiting_input"
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
        # Snapshot the full scrollback at send time. read.py --since-last-send
        # finds this snapshot as a prefix of the current pane and returns the
        # suffix. We strip trailing blank lines because `capture-pane` pads the
        # output to the pane's height with empties.
        snapshot = _strip_trailing_blanks(capture_pane(self.session, history=True))
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

    Newlines in ``text`` are sent as Ctrl-S (rustyline's "insert newline"
    binding) so the message stays in a single submission. A final Enter
    submits, unless ``submit`` is False.
    """
    lines = text.split("\n")
    for i, line in enumerate(lines):
        if i > 0:
            tmux("send-keys", "-t", session, "C-s")
        if line:
            tmux("send-keys", "-t", session, "-l", line)
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


def diff_since_snapshot(current: str, snapshot: str) -> str:
    """Return the suffix of ``current`` that comes after ``snapshot``.

    If ``snapshot`` is no longer a prefix of ``current`` (e.g. it scrolled out
    of tmux history), fall back to returning ``current`` unchanged.
    """
    cur = _strip_trailing_blanks(current)
    snap = _strip_trailing_blanks(snapshot)
    if not snap:
        return cur
    if cur.startswith(snap):
        return cur[len(snap):].lstrip("\n")
    # Snapshot may have aged out of scrollback. Try a line-based suffix match
    # of the last few snapshot lines instead.
    snap_lines = snap.splitlines()
    cur_lines = cur.splitlines()
    tail = "\n".join(snap_lines[-10:])
    if tail and tail in cur:
        idx = cur.index(tail) + len(tail)
        return cur[idx:].lstrip("\n")
    return cur


def detect_state(record: AgentRecord) -> tuple[str, str]:
    """Return ``(state, last_line)``."""
    if not session_alive(record.session):
        return STATE_EXITED, ""
    cmd = pane_command(record.session)
    pane = capture_pane(record.session)
    last = _last_nonblank_line(pane)
    if cmd is None or cmd not in {"aj", "target/debug/aj", "target/release/aj"}:
        # Foreground is no longer aj => the binary exited (shell prompt shown).
        # We still consider this "exited" even if the tmux session lingers.
        return STATE_EXITED, last
    if PERMISSION_RE.match(last):
        return STATE_AWAITING_PERMISSION, last
    if IDLE_RE.match(last):
        return STATE_AWAITING_INPUT, last
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
