#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Stop an aj sub-agent and remove its registry entry."""

from __future__ import annotations

import argparse
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as c  # noqa: E402


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("name")
    ap.add_argument("--force", action="store_true", help="kill the tmux session immediately")
    ap.add_argument(
        "--graceful-timeout",
        type=float,
        default=5.0,
        help="seconds to wait for aj to exit after Ctrl-C/Ctrl-D",
    )
    ap.add_argument("--keep-record", action="store_true", help="do not delete registry entry")
    args = ap.parse_args()

    record = c.AgentRecord.load(args.name)

    if not c.session_alive(record.session):
        if not args.keep_record:
            record.delete()
        print(f"{args.name}: already gone")
        return 0

    if args.force:
        c.tmux("kill-session", "-t", record.session, check=False)
    else:
        # Cancel any running work, then EOF the prompt to exit cleanly.
        for key in ("C-c", "C-d"):
            if not c.session_alive(record.session):
                break
            c.tmux("send-keys", "-t", record.session, key, check=False)
            time.sleep(0.2)
        deadline = time.monotonic() + args.graceful_timeout
        while time.monotonic() < deadline:
            if not c.session_alive(record.session):
                break
            state, _ = c.detect_state(record)
            if state == c.STATE_EXITED:
                break
            time.sleep(0.2)
        if c.session_alive(record.session):
            c.tmux("kill-session", "-t", record.session, check=False)

    if not args.keep_record:
        record.delete()
    print(f"{args.name}: stopped")
    return 0


if __name__ == "__main__":
    sys.exit(main())
