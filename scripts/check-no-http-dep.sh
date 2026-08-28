#!/usr/bin/env bash
# Guard: aj-app must not directly depend on an HTTP transport crate.
#
# aj-app is the transport-agnostic core. Provider networking is encapsulated
# by aj-models, so its transitive closure necessarily includes HTTP clients.
# The remote-control boundary is that aj-app does not own an HTTP stack itself.
# We check direct normal dependencies under every feature and target.
# Dev-dependencies support wire-level fixtures. They do not leak into a
# consuming binary.
set -euo pipefail

tree="$(cargo tree -p aj-app -e normal --depth 1 --all-features --target all --prefix none)"
root="$(printf '%s\n' "$tree" | sed -n '1p')"
if [[ "$root" != "aj-app "* ]]; then
    echo "error: could not identify aj-app at the root of its dependency tree" >&2
    exit 1
fi

# This allowlist makes the boundary fail closed. A new direct normal package,
# including a transport crate the project has never seen before, must be
# reviewed and added deliberately rather than slipping past name recognition.
approved='aj-agent
aj-conf
aj-models
aj-session
aj-tools
aj-wire
anyhow
arboard
base64
chrono
clap
flate2
iana-time-zone
image
notify
pulldown-cmark
rand
serde
serde_json
syntect
thiserror
tokio
tokio-util
tracing
unicode-segmentation
unicode-width
url'

direct="$(printf '%s\n' "$tree" | sed '1d' | awk '{print $1}' | LC_ALL=C sort -u)"
unapproved="$(LC_ALL=C comm -23 <(printf '%s\n' "$direct") <(printf '%s\n' "$approved"))"
stale="$(LC_ALL=C comm -13 <(printf '%s\n' "$direct") <(printf '%s\n' "$approved"))"

if [ -n "$unapproved" ]; then
    echo "error: aj-app has unapproved direct normal dependencies:" >&2
    printf '%s\n' "$unapproved" | sed 's/^/  /' >&2
    echo "Every new direct normal dependency requires architecture review and an explicit" >&2
    echo "allowlist update so HTTP transport crates fail closed." >&2
    exit 1
fi

if [ -n "$stale" ]; then
    echo "error: aj-app's direct dependency allowlist has stale entries:" >&2
    printf '%s\n' "$stale" | sed 's/^/  /' >&2
    echo "Remove stale approvals so re-adding a package requires architecture review." >&2
    exit 1
fi

echo "ok: aj-app has no direct HTTP transport dependency"
