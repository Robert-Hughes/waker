#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
STATE_DIR="$ROOT_DIR/.lab"
MARKER="$STATE_DIR/freebsd-interface"
FIREWALL_MARKER="$STATE_DIR/freebsd-ipfw-rule"
EXPECTED_IFACE="wg-waker-lab"
EXPECTED_FIREWALL_RULE="90"

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

FIREWALL_RULE=""
if [ -f "$FIREWALL_MARKER" ]; then
    FIREWALL_RULE=$(cat "$FIREWALL_MARKER")
    if [ "$FIREWALL_RULE" != "$EXPECTED_FIREWALL_RULE" ]; then
        echo "Unexpected IPFW rule in lab marker: $FIREWALL_RULE" >&2
        exit 1
    fi
fi

if [ -n "$FIREWALL_RULE" ] && ipfw list | grep -q "^$(printf '%05d' "$FIREWALL_RULE") "; then
    ipfw -q delete "$FIREWALL_RULE"
fi

if ifconfig "$IFACE" >/dev/null 2>&1; then
    ifconfig "$IFACE" destroy
fi

rm -f "$MARKER" "$FIREWALL_MARKER" "$STATE_DIR/server.key" "$STATE_DIR/waker-lab.conf"
rmdir "$STATE_DIR" 2>/dev/null || true

echo "Waker lab WireGuard peer is down."
