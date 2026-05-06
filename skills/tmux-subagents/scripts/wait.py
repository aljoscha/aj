#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Block until one/all named aj sub-agents need attention.

Exits 0 on success, 124 on timeout.
"""

from __future__ import annotations

import argparse
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as c  # noqa: E402


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("names", nargs="*", help="agent names (default: all registered)")
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--any", dest="mode", action="store_const", const="any", help="(default)")
    g.add_argument("--all", dest="mode", action="store_const", const="all")
    ap.set_defaults(mode="any")
    ap.add_argument("--timeout", type=float, default=None, help="seconds (default: forever)")
    ap.add_argument("--poll", type=float, default=0.5, help="poll interval (s)")
    ap.add_argument(
        "--exclude-exited",
        action="store_true",
        help="do not treat agent exit as a wake-up condition",
    )
    args = ap.parse_args()

    records = (
        [c.AgentRecord.load(n) for n in args.names] if args.names else c.list_records()
    )
    if not records:
        c.die("no sub-agents to wait on")

    accept = set(c.AWAITING_STATES)
    if not args.exclude_exited:
        accept.add(c.STATE_EXITED)

    deadline = None if args.timeout is None else time.monotonic() + args.timeout

    while True:
        statuses = []
        for r in records:
            state, last = c.detect_state(r)
            statuses.append((r, state, last))

        ready = [(r, s, last) for (r, s, last) in statuses if s in accept]

        if args.mode == "any" and ready:
            done = ready
            break
        if args.mode == "all" and len(ready) == len(records):
            done = ready
            break

        if deadline is not None and time.monotonic() >= deadline:
            c.print_json(
                {
                    "timeout": True,
                    "mode": args.mode,
                    "agents": [
                        {"name": r.name, "state": s, "last_line": last}
                        for (r, s, last) in statuses
                    ],
                }
            )
            return 124

        time.sleep(args.poll)

    c.print_json(
        {
            "timeout": False,
            "mode": args.mode,
            "ready": [
                {"name": r.name, "state": s, "last_line": last, "task": r.task}
                for (r, s, last) in done
            ],
        }
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
