# Local WireGuard lab

This lab exercises Waker's real `smoltcp -> GotaTun -> UDP -> WireGuard peer` path without contacting the production FRITZ!Box.

On GhostBSD/FreeBSD the peer is the independent kernel WireGuard implementation, which verifies interoperability rather than testing GotaTun against itself.

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

If IPFW is enabled, the helper installs temporary rule `90` allowing only `10.231.0.0/24` traffic via `wg-waker-lab`. This permits the synthetic FRITZ HTTP service and ICMP readiness traffic through the lab interface. The helper refuses to run if rule `90` or the `wg-waker-lab` interface already exists, and teardown removes only the marked lab rule/interface.

## Start the lab peer

From the repository root:

```sh
sudo ./scripts/lab-up-freebsd.sh
```

The command prints the generated client profile and the exact commands for the unprivileged processes.

## Start the fake FRITZ service

In terminal 1:

```sh
cd ~/src/waker
cargo run -p waker-lab
```

Defaults:

- fake FRITZ: `0.0.0.0:49000`
- target IPv4 returned by `GetSpecificHostEntry`: `10.231.0.1`

The fake service implements the two Hosts actions used by the production wake flow:

- `GetSpecificHostEntry` returns the configured lab target IPv4 address;
- `X_AVM-DE_WakeOnLANByMACAddress` accepts the WOL request.

The target is the FreeBSD WireGuard peer itself, so the production ICMP readiness probe is exercised over the real encrypted tunnel. Retry and timeout behaviour are covered separately by `waker-core` unit tests.

The returned target can be changed with:

```sh
WAKER_LAB_FRITZ_BIND=0.0.0.0:49000 \
WAKER_LAB_PC_IP=10.231.0.1 \
cargo run -p waker-lab
```

## Run Waker against the lab

In terminal 2:

```sh
cd ~/src/waker
WAKER_WG_CONFIG="$PWD/.lab/waker-lab.conf" \
WAKER_FRITZ_IP="10.231.0.1" \
WAKER_PC_MAC="AA:BB:CC:DD:EE:FF" \
RUST_LOG=waker=trace \
cargo run -p waker-app --bin waker
```

Press **Wake**. A successful trace should show:

1. the private TCP connection to the fake FRITZ;
2. `GetSpecificHostEntry` resolving the target IPv4 address;
3. the WOL request;
4. an ICMP readiness probe to the resolved lab peer;
5. the terminal **PC awake** state.

For an automated non-GUI end-to-end run against the same live lab:

```sh
WAKER_LAB_PROFILE="$PWD/.lab/waker-lab.conf" \
  cargo test -p waker-net --test local_lab -- --ignored --nocapture
```

This test is ignored during normal `cargo test --workspace` runs because it requires the temporary kernel WireGuard peer and fake service.

For packet-level diagnosis, the two useful capture points are:

```sh
sudo tcpdump -ni lo0 udp port 51820
sudo tcpdump -ni wg-waker-lab
```

`lo0` shows encrypted WireGuard traffic. `wg-waker-lab` shows the decrypted inner HTTP and ICMP traffic.

## Tear down

```sh
sudo ./scripts/lab-down-freebsd.sh
```

The teardown helper removes only the marked lab IPFW rule (when one was installed), destroys the exact interface recorded by the Waker lab marker, and then removes the generated throwaway keys/configuration.

## Why this lab exists

It gives separate failure boundaries:

- no UDP/handshake traffic: endpoint/GotaTun problem;
- handshake but no inner TCP: GotaTun-to-smoltcp adapter problem;
- good TCP but wrong SOAP: FRITZ protocol problem;
- host lookup does not resolve the expected address: Hosts parsing/problem;
- WOL succeeds but no inner ICMP: readiness/ICMP path problem.

The same `waker-net` code is used by this lab, the desktop application, and Android.
