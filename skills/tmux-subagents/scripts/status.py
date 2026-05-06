#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Report status of one or more aj sub-agents."""

from __future__ import annotations

import argparse
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as c  # noqa: E402


def status_for(record: c.AgentRecord) -> dict:
    state, last = c.detect_state(record)
    last_send = record.last_send_time()
    return {
        "name": record.name,
        "session": record.session,
        "task": record.task,
        "cwd": record.cwd,
        "state": state,
        "last_line": last,
        "session_alive": c.session_alive(record.session),
        "spawned_at": record.spawned_at,
        "age_s": round(time.time() - record.spawned_at, 1),
        "last_send_at": last_send,
        "since_last_send_s": (round(time.time() - last_send, 1) if last_send else None),
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("names", nargs="*", help="agent names (default: all)")
    ap.add_argument("--text", action="store_true", help="human-readable output")
    args = ap.parse_args()

    if args.names:
        records = [c.AgentRecord.load(n) for n in args.names]
    else:
        records = c.list_records()

    rows = [status_for(r) for r in records]

    if args.text:
        if not rows:
            print("(no sub-agents registered for this project)")
            return 0
        for r in rows:
            print(
                f"{r['name']:<20} {r['state']:<22} age={r['age_s']}s  "
                f"task={r['task']!r}"
            )
        return 0

    c.print_json(rows)
    return 0


if __name__ == "__main__":
    sys.exit(main())
