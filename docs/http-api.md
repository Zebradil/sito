# HTTP API

sito speaks a small subset of the [Nix binary cache HTTP
protocol](https://nix.dev/manual/nix/latest/protocols/http-binary-cache), plus
one endpoint of its own (`/status`). It is **read-only**: only `GET` and `HEAD`
are answered, everything else gets `405`.

Two of the four routes sito answers itself; the other two are forwarded to
upstreams according to the selection plan (see
[architecture.md](architecture.md)).

| Route             | Method       | Answered by | Notes                                          |
| ----------------- | ------------ | ----------- | ---------------------------------------------- |
| `/nix-cache-info` | `GET`/`HEAD` | sito        | The only response sito authors (ADR-0006)      |
| `/status`         | `GET`/`HEAD` | sito        | JSON, sito-specific, not part of the Nix protocol |
| `/*.narinfo`      | `GET`/`HEAD` | upstream    | Body buffered (≤1 MiB) to seed NAR affinity     |
| `/nar/*`          | `GET`/`HEAD` | upstream    | Streamed through, never buffered                |
| anything else     | `GET`/`HEAD` | sito        | `404 not found` — never forwarded               |
| anything          | other        | sito        | `405 sito is read-only`                         |

## `GET /nix-cache-info`

Static, authored by sito rather than proxied — a client must be able to learn
sito is a usable substituter even when every upstream is down.

```
StoreDir: /nix/store
WantMassQuery: 1
Priority: 10
```

Lower `Priority` means "prefer me": at 10, sito outranks a directly-configured
`cache.nixos.org` (which advertises 40) should both end up in a client's
substituter list. Ordering *among the real caches* is sito's tiering, not the
client's priority list.

## `GET /<hash>.narinfo` and `GET /nar/<...>`

Forwarded to upstreams in selection-plan order. Per attempt:

| Upstream result | sito's reaction                                              |
| --------------- | ------------------------------------------------------------ |
| 2xx             | stream/relay it to the client, record a hit, stop             |
| 404             | record a miss, try the next upstream in the plan              |
| transport error | record an error, mark the upstream down, kick a probe pass, try the next |

If the plan is exhausted (or was empty because every upstream is marked down),
sito answers `404 no upstream has this path` and Nix falls through to its next
substituter or a local build.

Response headers are **not** passed through wholesale: only `Content-Type` and
`Content-Length` are copied. The body is byte-identical to the upstream's
(pass-through trust, [ADR-0006](adr/0006-pass-through-trust.md)) — signature
verification stays in the Nix client.

`HEAD` requests are forwarded as `HEAD` and answered `200` with the copied
headers and no body.

Client request headers are **not** forwarded upstream, and sito always answers
`200` on a hit rather than mirroring the upstream's status. `Range`,
`If-None-Match` and friends are therefore ignored — the Nix daemon uses none of
them, but anything else pointed at sito should know.

Narinfo bodies are read into memory with a 1 MiB cap so the `URL:` field can be
parsed and remembered as **NAR affinity** — the next request for that NAR path
goes straight to the upstream whose narinfo named it. A narinfo larger than
1 MiB would be truncated while the upstream's `Content-Length` is still
forwarded, so the client would wait for bytes that never arrive; real narinfos
are well under a kilobyte. NAR bodies
are never buffered: they stream through a metering reader that records
throughput once the transfer completes.

A failure while reading a narinfo body from the upstream (after the response
headers already arrived) answers `502 upstream read failed` — no other upstream
is tried, because the client has already been told a response is coming.

## `GET /status`

sito's introspection endpoint: the exact numbers the selection engine ranks on.
`Content-Type: application/json`.

```console
$ curl -s localhost:5001/status | jq
```

```json
{
  "uptime_secs": 3812,
  "affinity_entries": 214,
  "upstreams": [
    {
      "index": 0,
      "url": "http://box.lan:5000",
      "tier": 0,
      "healthy": true,
      "probe_ms": 1.8,
      "narinfo_ms": 3.2,
      "nar_mbps": 84.5,
      "hits": 191,
      "misses": 12,
      "errors": 0
    },
    {
      "index": 1,
      "url": "https://cache.nixos.org",
      "tier": 1,
      "healthy": true,
      "probe_ms": 41.6,
      "narinfo_ms": null,
      "nar_mbps": null,
      "hits": 0,
      "misses": 0,
      "errors": 0
    }
  ]
}
```

### Top level

| Field              | Type   | Meaning                                                                                                              |
| ------------------ | ------ | -------------------------------------------------------------------------------------------------------------------- |
| `uptime_secs`      | int    | Whole seconds since this process built its registry, i.e. process uptime. All counters and averages below are scoped to it — nothing survives a restart ([ADR-0004](adr/0004-quality-signal.md)). |
| `affinity_entries` | int    | NAR paths currently remembered in the narinfo→upstream affinity map. Bounded at 4096; when full the whole map is dropped, so this number sawtooths rather than plateaus. It is metadata, not a response cache. |
| `upstreams`        | array  | One object per configured upstream, in flat config order (tier 0's upstreams first, then tier 1's, …).                 |

### Per upstream

| Field        | Type          | Meaning                                                                                                                                   |
| ------------ | ------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `index`      | int           | Position in the flat config-order list. This is the id the selection plan speaks in and the array index into `upstreams`.                     |
| `url`        | string        | Base URL exactly as configured.                                                                                                              |
| `tier`       | int           | 0-based index of the `[[tier]]` this upstream belongs to. Lower tiers are always tried first, regardless of measured speed.                   |
| `healthy`    | bool \| null  | `null` until the first probe answers (cold start). `true` after a successful probe, `false` after a failed probe **or** any transport error on a real request. `false` means the upstream is skipped entirely by the selection engine until a probe brings it back. |
| `probe_ms`   | float \| null | EWMA of `GET /nix-cache-info` round-trip time, milliseconds. `null` until the first successful probe. Failed probes do not contribute a sample — they only flip `healthy`. |
| `narinfo_ms` | float \| null | EWMA of narinfo request latency measured on real traffic, milliseconds. `null` until this upstream has served a narinfo. This is the **primary ranking key** within a tier; `probe_ms` is the fallback. |
| `nar_mbps`   | float \| null | EWMA of NAR download throughput. Despite the name this is **megabytes per second** (`bytes / 1e6 / seconds`), not megabits. `null` until a NAR transfer completes; aborted transfers contribute nothing. Currently informational — the built-in engine does not rank on it. |
| `hits`       | int           | Successful (2xx) upstream responses served through this upstream, narinfo and NAR alike.                                                      |
| `misses`     | int           | 404s from this upstream — it simply does not have the path. Normal and expected for a small LAN cache in front of a big one.                  |
| `errors`     | int           | Transport failures (connection refused, timeout, TLS, …). Each one also sets `healthy` to `false` and kicks an immediate probe pass. A rising `errors` count with `healthy: true` means the upstream is flapping. |

All EWMAs use α = 0.3 on the newest sample, seeded with the first sample, so
roughly the last handful of measurements dominate and a network change is
reflected within a few requests.

### Reading it

- **Everything `healthy: null` and all counters zero** — sito has just started
  and the first probe pass has not finished. Fewer than ~2 seconds old.
- **`healthy: false` on the LAN box, `true` elsewhere** — you are off the LAN.
  This is the design working, not a fault.
- **`narinfo_ms` null on a lower tier while a higher tier has traffic** — the
  lower tier is being tried first and missing (check `misses`) or is down
  (check `healthy`).
- **`hits` climbing on tier 1 while tier 0 is healthy** — tier 0 genuinely does
  not have those paths; sito falls through per request, not per session.
