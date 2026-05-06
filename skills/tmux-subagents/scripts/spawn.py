#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Spawn a new aj sub-agent in a detached tmux session."""

from __future__ import annotations

import argparse
import os
import shlex
import sys
import time
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as c  # noqa: E402


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("name", help="slug for this sub-agent (used in tmux session name)")
    ap.add_argument("--task", required=True, help="one-line description of the goal")
    ap.add_argument(
        "--message",
        "-m",
        help="initial user message: spawn waits for the prompt then sends this",
    )
    ap.add_argument("--cwd", default=None, help="working directory (default: current)")
    ap.add_argument(
        "--continue-thread",
        dest="continue_thread",
        nargs="?",
        const="",
        default=None,
        metavar="THREAD_ID",
        help="run `aj continue [THREAD_ID]` instead of a fresh `aj`",
    )
    ap.add_argument(
        "--aj-bin",
        default=os.environ.get("AJ_BIN", "aj"),
        help="aj executable (default: $AJ_BIN or `aj` from PATH)",
    )
    ap.add_argument("--replace", action="store_true", help="replace if name already exists")
    ap.add_argument(
        "--ready-timeout",
        type=float,
        default=60.0,
        help="seconds to wait for the prompt before --message is sent",
    )
    args, extra = ap.parse_known_args()
    if extra and extra[0] == "--":
        extra = extra[1:]

    name = c._validate_name(args.name)
    session = c.session_name(name)
    cwd = Path(args.cwd).resolve() if args.cwd else Path.cwd()
    if not cwd.is_dir():
        c.die(f"cwd does not exist: {cwd}")

    existing = c.project_dir() / f"{name}.json"
    if existing.exists() or c.session_alive(session):
        if not args.replace:
            c.die(
                f"sub-agent {name!r} already exists (session alive: "
                f"{c.session_alive(session)}). Use --replace or pick a new name."
            )
        if c.session_alive(session):
            c.tmux("kill-session", "-t", session, check=False)
        if existing.exists():
            existing.unlink()

    aj_cmd: list[str] = [args.aj_bin]
    if args.continue_thread is not None:
        aj_cmd.append("continue")
        if args.continue_thread:
            aj_cmd.append(args.continue_thread)
    aj_cmd.extend(extra)

    # Build the shell command tmux will run. We `cd` first, then exec aj so the
    # tmux pane's foreground command becomes `aj` (used for exit detection).
    shell_cmd = f"cd {shlex.quote(str(cwd))} && exec {shlex.join(aj_cmd)}"

    c.tmux("new-session", "-d", "-s", session, "-c", str(cwd), "bash", "-lc", shell_cmd)

    record = c.AgentRecord(
        name=name,
        session=session,
        cwd=str(cwd),
        cmd=aj_cmd,
        task=args.task,
        spawned_at=time.time(),
    )
    record.save()

    result = {
        "name": name,
        "session": session,
        "cwd": str(cwd),
        "cmd": aj_cmd,
        "task": args.task,
    }

    if args.message:
        state = c.wait_for_state(
            record,
            accept={c.STATE_AWAITING_INPUT, c.STATE_EXITED},
            timeout=args.ready_timeout,
        )
        if state == c.STATE_EXITED:
            result["initial_message_sent"] = False
            result["error"] = "agent exited before reaching prompt"
            c.print_json(result)
            return 2
        if state != c.STATE_AWAITING_INPUT:
            result["initial_message_sent"] = False
            result["error"] = f"timed out waiting for prompt (state={state})"
            c.print_json(result)
            return 3
        c.send_text(session, args.message)
        record.mark_send()
        result["initial_message_sent"] = True

    c.print_json(result)
    return 0


if __name__ == "__main__":
    sys.exit(main())
