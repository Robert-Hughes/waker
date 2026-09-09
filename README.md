# Waker

Waker is a small Rust application for waking a PC behind a FRITZ!Box without installing a system-wide VPN.

The application creates WireGuard entirely inside its own process. Only connections deliberately opened by Waker enter the tunnel; the operating system routing table, DNS configuration, default route, and other applications are untouched.

## Architecture

```text
Waker UI (eframe / egui / winit)
        |
        v
wake state machine (waker-core)
        |
        v
FRITZ TR-064 + TCP probe (waker-net)
        |
        v
smoltcp userspace TCP/IP stack
        |
        v
GotaTun WireGuard device
        |
        v
ordinary host UDP socket
        |
        v
FRITZ!Box WireGuard endpoint
```

There is no Android `VpnService` and no operating-system TUN interface. GotaTun receives and emits complete IP packets through its `IpSend`/`IpRecv` interfaces; Waker connects those packet interfaces directly to smoltcp.

The wake sequence is:

1. Resolve the configured WireGuard peer endpoint using the normal host network.
2. Start an in-process GotaTun device and private smoltcp interface.
3. Open a TCP connection through that private stack to the FRITZ!Box TR-064 service on port 49000. This also causes the WireGuard handshake to occur.
4. Send `Hosts:1#X_AVM-DE_WakeOnLANByMACAddress` for the configured MAC address.
5. Repeatedly attempt a TCP connection through the same private stack to the configured PC address/port.
6. Report success when the PC accepts the connection, or fail after the configured timeout.
7. Destroy the WireGuard device and private network state.

## Workspace

- `crates/waker-core`: platform-independent wake state machine and core types.
- `crates/waker-net`: WireGuard profile parsing, GotaTun/smoltcp bridge, private TCP client, FRITZ TR-064 request, and PC probing.
- `crates/waker-app`: Waker UI and desktop/Android entry points.
- `crates/waker-lab`: fake FRITZ!Box and fake PC services for end-to-end testing through a local WireGuard peer.

The networking and wake crates contain no Android-specific code. Desktop is the primary development and diagnostic target.

## Desktop build

```sh
cd ~/src/waker
cargo run -p waker-app --bin waker
```

Waker currently exposes its setup values in the collapsible **Development settings** section. They can also be supplied as environment variables:

```sh
export WAKER_WG_CONFIG="$HOME/path/to/fritz-wireguard.conf"
export WAKER_FRITZ_IP="192.168.178.1"
export WAKER_PC_MAC="AA:BB:CC:DD:EE:FF"
export WAKER_PC_PROBE="192.0.2.42:22"
cargo run -p waker-app --bin waker
```

The probe should be a TCP port that becomes available reliably after the PC boots, for example SSH, RDP, or another known service. It is a stronger readiness check than merely receiving ICMP.

`waker.local.conf` is ignored by Git and is the default desktop profile path, so a convenient development setup is:

```sh
cp /path/to/exported-fritz-wireguard.conf ./waker.local.conf
```

Waker parses only the WireGuard fields it needs. `DNS` and other wg-quick-only fields are ignored because Waker never changes system DNS or routes.

## Local WireGuard lab

The lab is designed to test the complete packet path without using the real FRITZ!Box:

```text
Waker
  -> smoltcp
  -> GotaTun client
  -> encrypted UDP over 127.0.0.1
  -> FreeBSD kernel WireGuard peer
  -> fake FRITZ TCP :49000
  -> fake PC TCP :2222
```

The fake FRITZ service accepts the same WOL SOAP action used by the real backend. After receiving it, the fake PC waits for a configurable delay before beginning to accept probe connections.

On GhostBSD/FreeBSD, use the helper scripts described in `docs/LAB.md`. They create only an ephemeral `wg-waker-lab` interface and a generated test profile; no production keys are involved.

The fake services themselves run unprivileged:

```sh
cargo run -p waker-lab
```

## Android

The Android application is also pure Rust. `waker-app` builds as a `cdylib`, uses winit's `NativeActivity` backend through eframe, and requests only normal internet access. It does **not** request Android VPN permission.

The Cargo manifest is prepared for `cargo-apk` with display name **Waker** and package name `app.waker.android`.

The current GhostBSD development machine does not yet have an Android SDK/NDK, `cargo-apk`, or the `aarch64-linux-android` Rust target installed, so APK compilation has not yet been performed here. Android configuration import/persistent secret storage is intentionally not implemented yet; the shared networking core is complete independently of that platform plumbing.

## Validation

Run the normal validation suite with:

```sh
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

For detailed networking diagnostics:

```sh
RUST_LOG=waker=trace cargo run -p waker-app --bin waker
```

WireGuard private keys and preshared keys are never emitted to tracing output.

## Security model

Waker's inner network exists only in process memory. The host operating system sees an ordinary UDP flow to the WireGuard endpoint. It does not receive an inner route to the home LAN, and other applications cannot accidentally use Waker's tunnel.

Real WireGuard configuration files contain private keys and must not be committed. The repository ignores `waker.local.conf` and `config.local.toml`; additional local profiles should be kept outside the repository or added to local ignore rules.
