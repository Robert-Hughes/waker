#!/bin/sh
set -eu

IFACE="wg-waker-lab"
SERVER_ADDR="10.231.0.1/24"
CLIENT_ADDR="10.231.0.2/32"
LISTEN_PORT="51820"

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
STATE_DIR="$ROOT_DIR/.lab"
CLIENT_CONFIG="$STATE_DIR/waker-lab.conf"
MARKER="$STATE_DIR/freebsd-interface"

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

umask 077
mkdir -p "$STATE_DIR"

SERVER_PRIVATE=$(wg genkey)
SERVER_PUBLIC=$(printf '%s\n' "$SERVER_PRIVATE" | wg pubkey)
CLIENT_PRIVATE=$(wg genkey)
CLIENT_PUBLIC=$(printf '%s\n' "$CLIENT_PRIVATE" | wg pubkey)
printf '%s\n' "$SERVER_PRIVATE" > "$STATE_DIR/server.key"

cleanup_on_error() {
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
