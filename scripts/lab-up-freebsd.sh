#!/bin/sh
set -eu

IFACE="wg-waker-lab"
SERVER_ADDR="10.231.0.1/24"
CLIENT_ADDR="10.231.0.2/32"
LISTEN_PORT="51820"
FIREWALL_RULE="90"

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
STATE_DIR="$ROOT_DIR/.lab"
CLIENT_CONFIG="$STATE_DIR/waker-lab.conf"
MARKER="$STATE_DIR/freebsd-interface"
FIREWALL_MARKER="$STATE_DIR/freebsd-ipfw-rule"

if [ "$(uname -s)" != "FreeBSD" ]; then
    echo "This helper is for FreeBSD/GhostBSD." >&2
    exit 1
fi

if [ "$(id -u)" -ne 0 ]; then
    echo "Run this helper as root, for example: sudo ./scripts/lab-up-freebsd.sh" >&2
    exit 1
fi

if ifconfig "$IFACE" >/dev/null 2>&1; then
    echo "Interface $IFACE already exists; refusing to modify it." >&2
    exit 1
fi

FIREWALL_ENABLED=0
if [ "$(sysctl -n net.inet.ip.fw.enable 2>/dev/null || printf '0')" = "1" ]; then
    if ipfw list | grep -q "^$(printf '%05d' "$FIREWALL_RULE") "; then
        echo "IPFW rule $FIREWALL_RULE already exists; refusing to modify the firewall." >&2
        exit 1
    fi
    FIREWALL_ENABLED=1
fi
FIREWALL_ADDED=0

umask 077
mkdir -p "$STATE_DIR"

SERVER_PRIVATE=$(wg genkey)
SERVER_PUBLIC=$(printf '%s\n' "$SERVER_PRIVATE" | wg pubkey)
CLIENT_PRIVATE=$(wg genkey)
CLIENT_PUBLIC=$(printf '%s\n' "$CLIENT_PRIVATE" | wg pubkey)
printf '%s\n' "$SERVER_PRIVATE" > "$STATE_DIR/server.key"

cleanup_on_error() {
    if [ "$FIREWALL_ADDED" -eq 1 ]; then
        ipfw -q delete "$FIREWALL_RULE" >/dev/null 2>&1 || true
        rm -f "$FIREWALL_MARKER"
    fi
    if ifconfig "$IFACE" >/dev/null 2>&1; then
        ifconfig "$IFACE" destroy >/dev/null 2>&1 || true
    fi
}
trap cleanup_on_error HUP INT TERM EXIT

ifconfig wg create name "$IFACE"
ifconfig "$IFACE" inet "$SERVER_ADDR"
wg set "$IFACE" \
    listen-port "$LISTEN_PORT" \
    private-key "$STATE_DIR/server.key" \
    peer "$CLIENT_PUBLIC" \
    allowed-ips "$CLIENT_ADDR"
ifconfig "$IFACE" up

if [ "$FIREWALL_ENABLED" -eq 1 ]; then
    ipfw -q add "$FIREWALL_RULE" allow ip from 10.231.0.0/24 to 10.231.0.0/24 via "$IFACE"
    printf '%s\n' "$FIREWALL_RULE" > "$FIREWALL_MARKER"
    FIREWALL_ADDED=1
fi

cat > "$CLIENT_CONFIG" <<EOF
[Interface]
PrivateKey = $CLIENT_PRIVATE
Address = $CLIENT_ADDR

[Peer]
PublicKey = $SERVER_PUBLIC
AllowedIPs = 10.231.0.0/24
Endpoint = 127.0.0.1:$LISTEN_PORT
PersistentKeepalive = 5
EOF

printf '%s\n' "$IFACE" > "$MARKER"

if [ -n "${SUDO_UID:-}" ] && [ -n "${SUDO_GID:-}" ]; then
    chown -R "$SUDO_UID:$SUDO_GID" "$STATE_DIR"
fi

trap - HUP INT TERM EXIT

cat <<EOF
Waker lab WireGuard peer is up.

Client profile:
  $CLIENT_CONFIG

Run the fake services in one terminal:
  cd $ROOT_DIR
  cargo run -p waker-lab

Run Waker in another terminal:
  cd $ROOT_DIR
  WAKER_WG_CONFIG="$CLIENT_CONFIG" \\
  WAKER_FRITZ_IP="10.231.0.1" \\
  WAKER_PC_MAC="AA:BB:CC:DD:EE:FF" \\
  WAKER_PC_PROBE="10.231.0.1:2222" \\
  cargo run -p waker-app --bin waker

When finished:
  sudo ./scripts/lab-down-freebsd.sh
EOF
