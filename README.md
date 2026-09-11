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

`waker.local.conf` is ignored by Git and is the default desktop profile path. Waker can keep its application settings in the same private file using the extension keys `WakerFritzIP`, `WakerPcMac`, and `WakerProbeAddress`:

```ini
WakerFritzIP = 192.168.178.1
WakerPcMac = AA:BB:CC:DD:EE:FF
WakerProbeAddress = 192.0.2.42:22
```

Environment variables override these file values, and file values override built-in defaults. The WireGuard parser deliberately ignores the Waker extension keys as well as wg-quick-only fields such as `DNS`; Waker never changes system DNS or routes.

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

The Android application is pure Rust. `waker-app` builds as a `cdylib`, uses winit's `NativeActivity` backend through eframe, and requests only normal internet access. It does **not** request Android VPN permission.

The Cargo manifest uses display name **Waker** and package name `app.waker.android`. Android packaging requires a Rust Android toolchain and an APK packager compatible with the manifest metadata in `crates/waker-app/Cargo.toml`; host-specific SDK/NDK setup is intentionally kept outside this repository.

Android debug builds, NativeActivity launch, diagnostics, S3 wake, and a mobile-data wake from outside the home LAN have all been exercised during development. The current wake-completion check still uses the temporary TCP readiness probe while FRITZ!Box host-status polling is evaluated.

On Android, the default WireGuard profile path is `<internalDataPath>/waker.local.conf`. Development profiles should be provisioned there through app-private storage; they should not be copied to shared `/sdcard` storage.

## Diagnostics and reporting

Each wake attempt has an ID and elapsed time. The normal UI presents concise progress (`Connecting`, wake request, numbered PC probes), a friendly terminal success/failure, and a collapsible technical **Details** section for failures. Unexpected worker/runtime shutdowns are converted into terminal runtime failures rather than leaving the UI permanently busy.

Waker also writes a persistent diagnostic event stream. The default log level is `debug`; set `WAKER_LOG=trace` for packet-level Waker tracing, or `info`, `warn`, or `error` to reduce detail. Persistent logs contain only Waker's own tracing targets, not arbitrary dependency logs. The background writer is non-lossy: if its bounded queue is ever saturated, Waker applies backpressure rather than silently dropping diagnostic events.

Desktop logs are written under `$XDG_STATE_HOME/waker/logs`, or `~/.local/state/waker/logs` when `XDG_STATE_HOME` is unset, and are mirrored to stderr. The desktop diagnostics directory is forced to mode `0700`. Android logs are written under the app-private internal data directory and mirrored to Android logcat with tag `Waker`. Logs rotate daily with at most seven files retained.

The in-app **Diagnostics** panel shows the last attempt, persistent-log status, a recent log tail, and three network diagnostics: **Check PC via FRITZ!Box API**, **Ping PC through tunnel**, and **Wake + time ICMP**. The first reports `GetSpecificHostEntry`/`NewActive`; the second resolves the target IPv4 address from that same host entry and sends one ICMP echo through Waker's private userspace tunnel; the third is an experimental alternative wake path that resolves once, sends WOL, and measures the first ICMP reply without changing the production Wake button. **Copy diagnostics** includes these results and copies a sanitised bundle suitable for troubleshooting. The experiments and decision criteria are recorded in [`docs/WAKE-READINESS.md`](docs/WAKE-READINESS.md). WireGuard private keys and preshared keys are private implementation fields, never deliberately logged, and credential-assignment lines are redacted again when diagnostics are copied.

## Validation

Run the normal validation suite with:

```sh
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

For verbose networking diagnostics:

```sh
WAKER_LOG=trace cargo run -p waker-app --bin waker
```

## Security model

Waker's inner network exists only in process memory. The host operating system sees an ordinary UDP flow to the WireGuard endpoint. It does not receive an inner route to the home LAN, and other applications cannot accidentally use Waker's tunnel.

Real WireGuard configuration files contain private keys and must not be committed. The repository ignores `waker.local.conf` and `config.local.toml`; additional local profiles should be kept outside the repository or added to local ignore rules.

## Licence

Waker is available under either the Apache License 2.0 or the MIT licence, at your option. See `LICENSE-APACHE` and `LICENSE-MIT`.
