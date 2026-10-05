---
title: Troubleshoot sito
description: Read sito's logs and /status to find out why Nix fetched from the wrong place, slowly, or not at all.
---

sito exposes two things for diagnosis: its log, and a `/status` endpoint with the numbers it ranks upstreams on. This
guide shows how to read both, then goes through common symptoms.

## Prerequisites

- A running sito, by hand or through the [NixOS or nix-darwin module](../nix-modules/). Commands below assume the
  default `127.0.0.1:5001`.
- `curl` and `jq`.

## 1. Check that sito answers

sito answers `/nix-cache-info` itself, even with every upstream down:

```sh
curl -s http://127.0.0.1:5001/nix-cache-info
```

```text
StoreDir: /nix/store
WantMassQuery: 1
Priority: 10
```

Connection refused means sito is not running or listens elsewhere: check the service and its `listen` address.

## 2. Read the log

On NixOS: `journalctl -u sito -f`. On nix-darwin: `tail -f /var/log/sito.log`. By hand: standard error.

The filter is `RUST_LOG`, set by the module's `logLevel` option:

| Level | What you get |
| --- | --- |
| `sito=info` (default) | The startup line and one line per upstream state change, `upstream state changed url=… up=… reason=…`. Nothing per request. |
| `sito=debug` | Adds the selection plan and the upstream that answered for every request, plus each response code. Use it when routing looks wrong. |

These warnings appear at the default level:

| Message | Meaning |
| --- | --- |
| `upstream failed` | A request to an upstream failed: connection, TLS, timeout, or an error status other than 404. The upstream is marked down. |
| `narinfo body read failed` | The narinfo headers arrived but the body did not. Treated like `upstream failed`. |
| `NAR upstream read failed` | A NAR body broke off or sent nothing for 60 s. The upstream is marked down. Before the first byte, sito tries the next upstream; after it, Nix gets a short body and fails that download. |
| `slow NAR transfer` | A NAR completed but took 30 s or more at under 1 MB/s. |
| `all NAR slots busy, waiting` | A NAR request waited 5 s for one of the `max-inflight` slots. |
| `accept failed` | The listener could not accept a connection. After 100 failures in a row sito exits. |

`NAR transfer abandoned by client`, at info, means Nix hung up first. Nothing is recorded against the upstream.

## 3. Read `/status`

```sh
curl -s http://127.0.0.1:5001/status | jq '.upstreams[] | {url, tier, healthy, probe_ms, narinfo_ms, hits, misses, errors}'
```

Every number is in memory since the process started; a restart resets them all.

| Field | Meaning |
| --- | --- |
| `uptime_secs` | Seconds since sito started. |
| `affinity_entries` | NAR paths remembered with the upstream whose narinfo named them. Cleared whenever it reaches 4096, so it rises and drops. |
| `nar_slots` | `used`: NAR transfers in flight; `max`: the `max-inflight` cap. |
| `upstreams[]` | One entry per upstream, in config order: the first tier's upstreams, then the second's. |

Per upstream:

| Field | Meaning |
| --- | --- |
| `index` | Position in that flat config-order list. |
| `url` | Base URL as configured. |
| `tier` | Tier number, from 0. A lower tier is always tried first. |
| `healthy` | `null` until the first probe answers; `true` after a successful probe; `false` after a failed probe or any failed request. A `false` upstream is skipped until a probe succeeds. |
| `probe_ms` | Moving average of the probe round trip (`GET /nix-cache-info`), in ms. Failed probes add no sample. |
| `narinfo_ms` | Moving average of narinfo latency on real traffic, in ms, from request to response headers. The main ranking key within a tier; `probe_ms` stands in until it exists. |
| `nar_mbytes_per_sec` | Moving average of NAR throughput, in MB/s, from completed transfers only. Shown, not ranked on. |
| `hits` | Requests this upstream answered, narinfo and NAR alike. |
| `misses` | 404s: the upstream does not have the path. Routine for a small cache in front of a big one. |
| `errors` | Failed requests, including NAR bodies that broke off. Each also sets `healthy` to `false` and starts a probe pass at once. |

The moving averages weigh the newest sample at 0.3, so a handful of requests is enough to reflect a network change.

The shape of `/status` is diagnostics for people, not a stable API.

## Symptoms

**Nix fetches from the public cache while the LAN box is up.** Look at the box's entry. `healthy: false` means the probe
cannot reach it: a network or upstream problem, not routing. `healthy: true` with `misses` rising means the box does not
have those paths, and sito falls through tier by tier on every request. If the public cache is in the *same* tier and
has a lower `narinfo_ms`, sito prefers it on measured evidence; move the box to its own, earlier tier to make the
preference absolute.

**An upstream that is back up is still skipped.** A down upstream returns after the next successful probe, at most
`probe-interval-secs` (default 15 s) later. Watch for `upstream state changed … up=true`.

**An upstream that needs credentials is always `healthy: false`.** The probe counts any answer other than 2xx as down,
401 and 403 included. The state-change line carries the reason, for example `reason="http status: 401"`. sito sends no
credentials, so authenticated caches are not supported yet
([ADR-0008](https://github.com/Zebradil/sito/blob/main/docs/adr/0008-auth-gated-upstreams.md)).

**Everything 404s.** Compare `misses` and `errors`. Rising `misses`: sito reaches the upstreams and they do not have the
paths; Nix builds locally. Rising `errors`: connectivity. With every upstream `healthy: false`, sito answers 404 without
asking anyone, except that a NAR is still tried at the upstream whose narinfo named it.

**Nix reports a signature error.** sito does not touch signatures. Nix does not trust the key the upstream signs with:
add it to that upstream's `public-keys` (with `manageSubstituters` on) or to `trusted-public-keys`, then rebuild.

**sito does not start.** The error names the problem; see the table in
[Configure tiers and upstreams](../configure-upstreams/#4-check-the-file). `bind 127.0.0.1:5001: Address already in use`
means another process, often an older sito, holds the port.

**Nix is slow while sito is down.** It should not be: a stopped sito refuses connections on loopback at once, and Nix
moves on. Slowness points at sito being up with an upstream that accepts connections and then stalls. Look for
`upstream failed` warnings and consider a lower `probe-timeout-secs`.

**A NAR download fails partway.** `NAR upstream read failed` names the upstream and how many bytes arrived. Once bytes
have reached Nix, sito cannot switch upstreams or resume, so Nix fails that download.

**Large builds stall.** Check `nar_slots`. `used` at `max` while bytes flow: raise `max-inflight`. `used` at `max` with
nothing moving, plus `all NAR slots busy, waiting`: transfers are stuck. Each gives up after 60 s without bytes, so look
for `NAR upstream read failed` naming the upstream.
