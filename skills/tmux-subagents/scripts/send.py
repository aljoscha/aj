#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Send a user message (or raw keys) to an aj sub-agent."""

from __future__ import annotations

import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _common as c  # noqa: E402


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("name")
    ap.add_argument(
        "message",
        nargs="?",
        help="message text; use '-' to read from stdin; omit if --keys is given",
    )
    ap.add_argument(
        "--keys",
        action="append",
        default=[],
        help="send raw tmux key(s) instead of a message (e.g. --keys C-c). "
        "May be repeated.",
    )
    ap.add_argument(
        "--no-submit",
        action="store_true",
        help="do not press Enter after the message",
    )
    ap.add_argument(
        "--force",
        action="store_true",
        help="send even if the agent is not currently awaiting input",
    )
    ap.add_argument(
        "--wait",
        type=float,
        default=0.0,
        help="seconds to wait for the agent to be idle before sending (0 = no wait)",
    )
    args = ap.parse_args()

    record = c.AgentRecord.load(args.name)

    if not c.session_alive(record.session):
        c.die(f"sub-agent {args.name!r} is not running (session dead)")

    if args.wait > 0 and not args.keys:
        c.wait_for_state(
            record,
            accept=c.AWAITING_STATES | {c.STATE_EXITED},
            timeout=args.wait,
        )

    if not args.force and not args.keys:
        state, last = c.detect_state(record)
        if state not in c.AWAITING_STATES:
            c.die(
                f"agent {args.name!r} is in state {state!r}; not sending. "
                f"(last line: {last!r}). Use --force to override or --wait N."
            )

    if args.keys:
        c.send_keys(record.session, *args.keys)
    else:
        if args.message is None:
            c.die("missing message (or use --keys)")
        text = sys.stdin.read() if args.message == "-" else args.message
        text = text.rstrip("\n")
        c.send_text(record.session, text, submit=not args.no_submit)
        record.mark_send()

    return 0


if __name__ == "__main__":
    sys.exit(main())
