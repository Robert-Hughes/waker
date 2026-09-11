# Wake readiness experiments

This document records the experiments used to choose how Waker decides that a PC has finished waking. The production wake path should only change when the replacement has been measured on a real suspend/resume cycle.

## Constraints

Waker's networking remains private to the process:

- WireGuard runs in process and uses an ordinary UDP socket.
- The inner IP stack is smoltcp.
- Android does not install a VPN service, TUN interface, route, or DNS configuration.
- Readiness checking should not require a dedicated service on the target PC if that can be avoided.
- Target addressing should preferably come from the FRITZ!Box host entry for the configured MAC rather than another separately configured address.

## Experiment 1: temporary TCP readiness service

The first end-to-end wake tests used a temporary TCP listener on the target PC and a narrow temporary firewall rule. Waker repeatedly attempted a TCP connection through its private WireGuard/smoltcp stack.

On 10 September 2026, one measured S3 wake produced this sequence:

| Event | Time |
| --- | --- |
| FRITZ!Box accepted WOL | 22:46:06.223 |
| Kernel resumed from S3 | about 22:46:11 |
| Ethernet link became up | about 22:46:14 |
| TCP readiness probe first succeeded | 22:46:16.190 |

The TCP probe therefore detected readiness about **9.97 seconds after WOL was accepted**.

### What this proved

- Waker's private tunnel remains usable while the PC is asleep and resumes.
- A direct target probe can detect readiness significantly earlier than the FRITZ!Box host-active flag.
- The wake path works without exposing Waker's tunnel to other Android applications.

### Why it is not an ideal production mechanism

It requires a listener on the PC, a configured target IP/port, and a firewall allowance. Those are extra moving parts whose only purpose is readiness detection.

The production Wake button still uses this mechanism while alternatives are being evaluated.

## Experiment 2: FRITZ!Box `NewActive`

A diagnostic action calls the Hosts service `GetSpecificHostEntry` for the target MAC and reports `NewActive`.

During the same S3 cycle:

| Event | Time |
| --- | --- |
| Suspend requested | 22:45:51 |
| FRITZ still reported active | 22:45:55.704 |
| FRITZ first reported inactive | 22:45:58.924 |
| WOL accepted | 22:46:06.223 |
| TCP readiness succeeded | 22:46:16.190 |
| FRITZ still reported inactive | 22:46:24.032 |
| FRITZ first reported active again | 22:46:25.930 |

### What this proved

The inactive transition is reasonably prompt, but the active transition can lag real target readiness. In this sample, `NewActive=1` appeared about **19.71 seconds after WOL acceptance**, roughly **9.74 seconds after the direct TCP probe already worked**.

### Consequence

`NewActive` is useful for diagnostics and may be an acceptable fallback, but using it as the only post-WOL readiness signal adds avoidable latency.

## Experiment 3: ICMP while already awake

Waker gained a diagnostic that:

1. opens the normal private WireGuard tunnel;
2. resolves the PC's current IPv4 address with `GetSpecificHostEntry`;
3. sends an ICMP echo through smoltcp;
4. reports whether an echo reply is received;
5. disconnects the tunnel.

An Android test against an already-awake PC succeeded. The whole diagnostic completed in about **1.43 seconds**.

### What this proved

ICMP works through Waker's in-process WireGuard/smoltcp path and the target responds to echo requests while awake.

### What this did not prove

It did not establish how soon ICMP becomes usable after an S3 wake. That timing is the important criterion for replacing the TCP readiness probe.

## Experiment 4: Wake + ICMP timing

A temporary diagnostic alternative wake action, **Wake + time ICMP**, was added specifically to measure the missing S3 behaviour without changing the production Wake button.

Its sequence was:

1. connect one private WireGuard tunnel;
2. resolve the target IPv4 address once from the FRITZ!Box host entry;
3. send the normal FRITZ!Box Wake-on-LAN request;
4. start timing when the FRITZ!Box accepts the WOL request;
5. send ICMP echo attempts to the resolved address through the same tunnel;
6. report the first reply time and attempt count;
7. fail after 45 seconds if no reply arrives;
8. disconnect the tunnel.

Each ICMP attempt used a 750 ms timeout followed by a 250 ms interval, giving approximately one-second sampling while the machine was unavailable.

The experiment deliberately did **not** use the configured TCP probe address, did not alter the production Wake state machine, and did not repeatedly query the FRITZ!Box between pings.

### S3 results

On 11 September 2026 the diagnostic was run three times from real S3 sleep. These tests were performed before the later reboot back into the default boot environment; the machine was in the temporary USB/S3 test environment during the measurements.

| Trial | WOL accepted | Kernel resume | Ethernet link up | First ICMP reply | WOL → ICMP | Attempts |
| --- | --- | --- | --- | --- | ---: | ---: |
| 1 | 08:14:34.968 | about 08:14:39 | 08:14:42 | 08:14:44.006 | **9.037 s** | 10 |
| 2 | 08:15:44.089 | about 08:15:48 | 08:15:51 | 08:15:55.148 | **11.058 s** | 12 |
| 3 | 08:16:54.098 | about 08:16:58 | 08:17:01 | 08:17:04.855 | **10.756 s** | 11 |

All three NIC wake records showed a magic-packet wake. ICMP became available roughly 2–4 seconds after Ethernet link-up and, in all three trials, before the later DHCP "New IP Address" log.

Across the three trials, WOL-to-ICMP time ranged from **9.037 to 11.058 seconds**, with an average of about **10.28 seconds** and a median of **10.756 seconds**.

For comparison, the earlier temporary TCP readiness test succeeded about **9.97 seconds after WOL acceptance**, while FRITZ!Box `NewActive=1` appeared about **19.71 seconds after WOL acceptance** in the measured cycle.

### Decision and production implementation

The experiment satisfied the decision criterion: ICMP readiness was reliable across repeated S3 wakes and effectively as prompt as the temporary TCP listener, while avoiding the dedicated target service, configured probe port, and firewall exception.

Production Wake now uses:

```text
connect private WireGuard
        ↓
GetSpecificHostEntry(MAC)
        ↓
obtain current IPv4 address
        ↓
send FRITZ!Box WOL
        ↓
poll ICMP through the same tunnel
        ↓
first echo reply = awake
        ↓
disconnect
```

The production probe uses the same 750 ms ICMP timeout and 250 ms retry interval validated by the experiment. The temporary **Wake + time ICMP** action was then removed because it duplicated the production path.

The obsolete `WakerProbeAddress` / `WAKER_PC_PROBE` configuration, PC-probe UI field, fake lab TCP listener, and TCP-specific wake-readiness code were removed. Generic TCP remains part of the private stack because FRITZ!Box TR-064 uses HTTP over TCP.

The temporary target listener and firewall rule were runtime-only and were already absent after the subsequent reboot.

## Production acceptance

The migrated normal **Wake** button was then tested from real S3 sleep on 11 September 2026 with the old TCP listener and firewall rule absent.

The production attempt:

- resolved the target through `GetSpecificHostEntry`;
- received HTTP 200 for the FRITZ!Box WOL request;
- required 11 ICMP readiness attempts;
- received the first ICMP reply **10.689 seconds after WOL acceptance**;
- completed the whole Waker attempt in **11.000 seconds**;
- disconnected the private WireGuard tunnel normally.

The host recorded a magic-packet wake, resumed roughly four seconds after WOL acceptance, brought Ethernet link up about three seconds later, and replied to ICMP about 3.6 seconds after link-up. The ICMP reply again preceded the later DHCP address-refresh log.

This accepts the FRITZ host lookup + ICMP design as the production wake-readiness mechanism.

## Why the experiments were staged

Changing production readiness before measuring the replacement would have conflated two questions: whether ICMP worked at all, and whether it became available early enough during resume. Keeping the existing Wake button unchanged provided a known baseline while the diagnostic path gathered evidence.

After the repeated S3 results met the decision criterion, the production path was migrated and the obsolete alternatives were removed together.