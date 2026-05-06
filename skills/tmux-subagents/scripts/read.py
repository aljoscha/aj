#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Read transcript output from an aj sub-agent."""

from __future__ import annotations

import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as c  # noqa: E402


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("name")
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--lines", type=int, default=None, help="last N lines of visible pane")
    g.add_argument("--full", action="store_true", help="full scrollback history")
    g.add_argument(
        "--since-last-send",
        action="store_true",
        help="output produced after the most recent send.py invocation",
    )
    ap.add_argument(
        "--strip-blank",
        action="store_true",
        help="drop trailing blank lines",
    )
    args = ap.parse_args()

    record = c.AgentRecord.load(args.name)
    if not c.session_alive(record.session):
        c.die(f"sub-agent {args.name!r} is not running (session dead)", code=2)

    if args.since_last_send:
        history = c.capture_pane(record.session, history=True)
        snapshot = record.last_send_snapshot()
        if snapshot is None:
            text = history  # never sent: show everything
        else:
            text = c.diff_since_snapshot(history, snapshot)
    elif args.full:
        text = c.capture_pane(record.session, history=True)
    else:
        text = c.capture_pane(record.session, history=False)
        if args.lines is not None:
            text = "\n".join(text.splitlines()[-args.lines :])

    if args.strip_blank:
        text = text.rstrip() + "\n"

    sys.stdout.write(text)
    if not text.endswith("\n"):
        sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
