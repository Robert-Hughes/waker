# Local WireGuard lab

This lab exercises Waker's real `smoltcp -> GotaTun -> UDP -> WireGuard peer` path without contacting the production FRITZ!Box.

On GhostBSD/FreeBSD the peer is the independent kernel WireGuard implementation, which is useful because it verifies interoperability rather than testing GotaTun against itself.

## What it creates

`lab-up-freebsd.sh` creates an ephemeral interface named `wg-waker-lab`:

```text
outer UDP
Waker/GotaTun -> 127.0.0.1:51820 -> FreeBSD wg-waker-lab

inner IP
Waker          10.231.0.2
lab peer       10.231.0.1
```

It generates throwaway client/server keys and stores them below the ignored `.lab/` directory. It never reads or changes a production WireGuard profile.

If IPFW is enabled, the helper also installs temporary rule `90` allowing only `10.231.0.0/24` traffic via `wg-waker-lab`; the rule is required for the synthetic inbound TCP services on GhostBSD. The helper refuses to run if rule `90` or the `wg-waker-lab` interface already exists, and teardown removes only the marked lab rule/interface.

## Start the lab peer

From the repository root:

```sh
sudo ./scripts/lab-up-freebsd.sh
```

The command prints the generated client profile and the exact commands for the two unprivileged processes.

## Start fake FRITZ and PC services

In terminal 1:

```sh
cd ~/src/waker
cargo run -p waker-lab
```

Defaults:

- fake FRITZ: `0.0.0.0:49000`
- fake PC probe: `0.0.0.0:2222`
- fake PC wake delay: 2000 ms

The fake PC port is not opened until the fake FRITZ receives the WOL SOAP request. This lets the normal Waker retry state machine exercise the transition from asleep to reachable.

The values can be changed with:

```sh
WAKER_LAB_FRITZ_BIND=0.0.0.0:49000 \
WAKER_LAB_PC_BIND=0.0.0.0:2222 \
WAKER_LAB_WAKE_DELAY_MS=5000 \
cargo run -p waker-lab
```

## Run Waker against the lab

In terminal 2:

```sh
cd ~/src/waker
WAKER_WG_CONFIG="$PWD/.lab/waker-lab.conf" \
WAKER_FRITZ_IP="10.231.0.1" \
WAKER_PC_MAC="AA:BB:CC:DD:EE:FF" \
WAKER_PC_PROBE="10.231.0.1:2222" \
RUST_LOG=waker=trace \
cargo run -p waker-app --bin waker
```

Press **Wake**. A successful trace should show the private TCP connection to the fake FRITZ, the WOL request, one or more failed PC probes during the artificial delay, then a successful probe.

For an automated non-GUI end-to-end run against the same live lab:

```sh
WAKER_LAB_PROFILE="$PWD/.lab/waker-lab.conf" \
  cargo test -p waker-net --test local_lab -- --ignored --nocapture
```

This test is ignored during normal `cargo test --workspace` runs because it requires the temporary kernel WireGuard peer and fake services.

For packet-level diagnosis, the two useful capture points are:

```sh
sudo tcpdump -ni lo0 udp port 51820
sudo tcpdump -ni wg-waker-lab
```

`lo0` shows encrypted WireGuard traffic. `wg-waker-lab` shows the decrypted inner TCP/IP traffic.

## Tear down

```sh
sudo ./scripts/lab-down-freebsd.sh
```

The teardown helper removes only the marked lab IPFW rule (when one was installed), destroys the exact interface recorded by the Waker lab marker, and then removes the generated throwaway keys/configuration.

## Why this lab exists

It gives separate failure boundaries:

- no UDP/handshake traffic: endpoint/GotaTun problem;
- handshake but no inner SYN: GotaTun-to-smoltcp adapter problem;
- malformed inner TCP: smoltcp configuration/problem;
- good TCP but wrong SOAP: FRITZ protocol problem;
- WOL succeeds but no eventual probe: target/readiness problem.

The same `waker-net` code is used by this lab, the desktop application, and Android.
