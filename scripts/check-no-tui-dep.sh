#!/usr/bin/env bash
# Guard: aj-app must not depend on the vaxis TUI backend.
#
# aj-app is the frontend-agnostic core. Keeping the vaxis backend out of its
# runtime dependency closure preserves the core/frontend boundary: rendering
# lives in the `aj` binary, not in the shared library. See
# docs/aj-app-extraction-spec.md. We check the normal (runtime) dependency
# closure only; dev-dependencies do not leak into a consuming binary.
set -euo pipefail

banned='^(vaxis|vaxis-ucd|vaxis-derive)( |$)'

tree="$(cargo tree -p aj-app -e normal --prefix none)"
violations="$(printf '%s\n' "$tree" | grep -E "$banned" || true)"

if [ -n "$violations" ]; then
    echo "error: aj-app must not depend on the vaxis TUI backend, but its" >&2
    echo "dependency closure contains:" >&2
    printf '  %s\n' "$violations" >&2
    exit 1
fi

echo "ok: aj-app depends on no TUI backend (vaxis)"
