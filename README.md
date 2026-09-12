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
FRITZ TR-064 + ICMP readiness (waker-net)
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
4. Call `Hosts:1#GetSpecificHostEntry` for the configured MAC address and obtain the PC's current IPv4 address.
5. Send `Hosts:1#X_AVM-DE_WakeOnLANByMACAddress` for the same MAC address.
6. Poll ICMP echo through the same private tunnel to the resolved PC address.
7. Report success on the first echo reply, or fail after the wake timeout.
8. Destroy the WireGuard device and private network state.

## Workspace

- `crates/waker-core`: platform-independent wake state machine and core types.
- `crates/waker-net`: WireGuard profile parsing, GotaTun/smoltcp bridge, private TCP/ICMP transport, FRITZ TR-064 host lookup/WOL, and readiness probing.
- `crates/waker-app`: Waker UI and desktop/Android entry points.
- `crates/waker-lab`: fake FRITZ!Box service and independent WireGuard/ICMP lab path for end-to-end testing.

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
cargo run -p waker-app --bin waker
```

`waker.local.conf` is ignored by Git and is the default desktop profile path. Waker can keep its application settings in the same private file using the extension keys `WakerFritzIP` and `WakerPcMac`:

```ini
WakerFritzIP = 192.168.178.1
WakerPcMac = AA:BB:CC:DD:EE:FF
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
  -> ICMP to the resolved lab peer address
```

The fake FRITZ service implements the Hosts actions Waker uses for target lookup and WOL. The FreeBSD WireGuard peer itself answers the ICMP readiness probe, while core unit tests cover retry and timeout transitions.

On GhostBSD/FreeBSD, use the helper scripts described in `docs/LAB.md`. They create only an ephemeral `wg-waker-lab` interface and a generated test profile; no production keys are involved.

The fake services themselves run unprivileged:

```sh
cargo run -p waker-lab
```

## Android

The Android application is Rust-first: `waker-app` builds as a `cdylib` and drives AndroidX GameActivity directly through `android-activity`, with egui rendered by `egui-wgpu`/wgpu. eframe/winit remain desktop-only integration dependencies. A tiny `WakerActivity` subclass supplies the Android host type, while the existing Java log helper hands sanitised logs to Android's MediaStore/viewer intent flow. Waker requests only normal internet access and does **not** request Android VPN permission.

The Android package name is `app.waker.android` and display name is **Waker**. Cargo builds the native Rust library; a small Gradle packaging project resolves GameActivity/AppCompat and produces the final AndroidX-aware APK. Host-specific SDK/NDK setup remains outside the repository, while the packaging definition itself is versioned under `android-gradle/`.
Release signing, the local ignored keystore, the repeatable release-build command, and the one-time debug-to-release migration are documented in [`docs/RELEASE.md`](docs/RELEASE.md). The direct GameActivity architecture, the reasons Android no longer uses winit, and the lifecycle/input validation requirements are documented in [`docs/ANDROID-LIFECYCLE.md`](docs/ANDROID-LIFECYCLE.md).

Android diagnostics, S3 wake, and a mobile-data wake from outside the home LAN have all been exercised during development. Production wake readiness resolves the target's current IPv4 address from the FRITZ!Box Hosts service and polls ICMP through Waker's private userspace tunnel; this was selected after repeated S3 timing tests against both the former TCP probe and FRITZ!Box `NewActive`.

On Android, the default WireGuard profile path is `<internalDataPath>/waker.local.conf`. Development profiles should be provisioned there through app-private storage; they should not be copied to shared `/sdcard` storage.

## Diagnostics and reporting

Each wake attempt has an ID and elapsed time. The normal UI presents concise progress (`Connecting`, `Finding PC`, wake request, numbered readiness probes), a friendly terminal success/failure, and a collapsible technical **Details** section for failures. Unexpected worker/runtime shutdowns are converted into terminal runtime failures rather than leaving the UI permanently busy.

Waker also writes a persistent diagnostic event stream. The default log level is `debug`; set `WAKER_LOG=trace` for packet-level Waker tracing, or `info`, `warn`, or `error` to reduce detail. Persistent logs contain only Waker's own tracing targets, not arbitrary dependency logs. The background writer is non-lossy: if its bounded queue is ever saturated, Waker applies backpressure rather than silently dropping diagnostic events.

Desktop logs are written under `$XDG_STATE_HOME/waker/logs`, or `~/.local/state/waker/logs` when `XDG_STATE_HOME` is unset, and are mirrored to stderr. The desktop diagnostics directory is forced to mode `0700`. Android logs are written under the app-private internal data directory and mirrored to Android logcat with tag `Waker`. Logs rotate daily with at most seven files retained.

The in-app **Diagnostics** panel shows the last attempt and persistent-log status, plus two network diagnostics: **Check PC via FRITZ!Box API** and **Ping PC through tunnel**. The first reports `GetSpecificHostEntry`/`NewActive`; the second resolves the target IPv4 address from that same host entry and sends one ICMP echo through Waker's private userspace tunnel. On Android, **Open log** writes or refreshes one sanitised `Download/Waker/waker-log.txt` MediaStore entry and launches an `ACTION_VIEW` chooser for its `content://` URI; **Copy log** copies the wake/check summaries plus the sanitised recent log tail without embedding a scrollable log viewer in Waker itself. The experiments that led to the production ICMP readiness design are recorded in [`docs/WAKE-READINESS.md`](docs/WAKE-READINESS.md). WireGuard private keys and preshared keys are private implementation fields, never deliberately logged, and credential-assignment lines are redacted again when diagnostics are copied.

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
