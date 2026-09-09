#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
STATE_DIR="$ROOT_DIR/.lab"
MARKER="$STATE_DIR/freebsd-interface"
EXPECTED_IFACE="wg-waker-lab"

if [ "$(uname -s)" != "FreeBSD" ]; then
    echo "This helper is for FreeBSD/GhostBSD." >&2
    exit 1
fi

if [ "$(id -u)" -ne 0 ]; then
    echo "Run this helper as root, for example: sudo ./scripts/lab-down-freebsd.sh" >&2
    exit 1
fi

if [ ! -f "$MARKER" ]; then
    echo "No Waker lab marker exists; refusing to destroy any interface." >&2
    exit 1
fi

IFACE=$(cat "$MARKER")
if [ "$IFACE" != "$EXPECTED_IFACE" ]; then
    echo "Unexpected interface name in lab marker: $IFACE" >&2
    exit 1
fi

if ifconfig "$IFACE" >/dev/null 2>&1; then
    ifconfig "$IFACE" destroy
fi

rm -f "$MARKER" "$STATE_DIR/server.key" "$STATE_DIR/waker-lab.conf"
rmdir "$STATE_DIR" 2>/dev/null || true

echo "Waker lab WireGuard peer is down."
