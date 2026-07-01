#!/usr/bin/env bash
# Guard: aj-app must not depend on aj-tui or vaxis.
#
# That no-TUI-dependency rule is the invariant that keeps aj-app shareable
# between the aj (aj-tui) and aj-next (vaxis) frontends. See
# docs/aj-app-extraction-spec.md. We check the normal (runtime) dependency
# closure only; dev-dependencies do not leak into a consuming binary.
set -euo pipefail

banned='^(aj-tui|aj-tui-testkit|vaxis|vaxis-ucd|vaxis-derive)( |$)'

tree="$(cargo tree -p aj-app -e normal --prefix none)"
violations="$(printf '%s\n' "$tree" | grep -E "$banned" || true)"

if [ -n "$violations" ]; then
    echo "error: aj-app must not depend on a TUI backend, but its dependency" >&2
    echo "closure contains:" >&2
    printf '  %s\n' "$violations" >&2
    exit 1
fi

echo "ok: aj-app depends on no TUI backend (aj-tui / vaxis)"
