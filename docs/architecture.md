# Architecture

How the pieces fit at runtime. *Why* they are shaped this way lives in
[`docs/adr/`](adr/); the vocabulary (upstream, tier, strategy, selection plan,
quality signal, probe, pass-through) is defined in
[`CONTEXT.md`](../CONTEXT.md).

## Modules

| File            | Responsibility                                                                                     |
| --------------- | -------------------------------------------------------------------------------------------------- |
| `src/main.rs`   | CLI parsing, log setup, `--listen` override. Nothing else.                                          |
| `src/lib.rs`    | `build()` assembles everything and binds the listener; `serve()` runs it. Split so tests can bind port 0 and learn the real port. |
| `src/config.rs` | TOML schema, defaults, validation.                                                                  |
| `src/state.rs`  | `Registry`: the single piece of shared mutable state — per-upstream health, EWMA metrics, counters, and the NAR affinity map. |
| `src/probe.rs`  | The probe thread: the active half of the quality signal.                                            |
| `src/select.rs` | The selection boundary: serializable input in, ordered fetch plan out.                              |
| `src/proxy.rs`  | The HTTP surface: routing, forwarding, streaming, concurrency cap.                                  |

## Threads

Three kinds, no async runtime:

- **The accept loop** (`proxy::serve`, on the main thread) — takes one request
  at a time off `tiny_http`, acquires a concurrency slot, spawns a worker.
  Acquiring blocks when `max-inflight` slots are already out, so the cap is
  applied by backpressure rather than by queueing or rejection.
- **One worker thread per request** (512 KiB stack) — does the whole thing:
  plan, fetch, stream, respond. It holds its slot until the last byte is
  written, so a slow NAR really does occupy a slot for its whole transfer.
- **The probe thread** — one, detached, never exits. Probes all upstreams, then
  waits `probe-interval-secs` on a channel that a failure can cut short.

Everything shared is behind `Mutex`es inside `Registry`. There is exactly one
lock over all upstream state and one over the affinity map; on a localhost
proxy that is not a contention story worth complicating.

## The upstream index space

`build()` flattens the configured tiers into one list of upstreams and numbers
them `0..n` in config order. That number is the upstream's identity everywhere:
it is `UpstreamSnapshot::index`, the entry in `TierShape::indices`, what a
`Plan` contains, the affinity map's value, and the array position in `/status`.
Tier membership is carried alongside (`TierShape` for the shape,
`UpstreamSnapshot::tier` for display) rather than being encoded in the number.

## A request, end to end

```
client (nix)
   │  GET /<hash>.narinfo
   ▼
accept loop ── slot ──▶ worker thread
                          │
                          │ 1. snapshot Registry, build SelectionInput
                          ▼
                       SelectionEngine::plan  ──▶  Plan { attempts: [2, 0, 3] }
                          │
                          │ 2. try each index in order
                          ▼
                       upstream 2 ── 404 ──▶ record_miss, next
                       upstream 0 ── 200 ──▶ record_narinfo_hit
                          │
                          │ 3. relay bytes unmodified
                          ▼
                       parse `URL:` ──▶ set_affinity(nar path → 0)
                          │
                          ▼
                       client
```

The follow-up `GET /nar/<...>` finds that affinity entry and its plan puts
upstream 0 first, so the NAR comes from the same cache that answered the
narinfo — which matters because narinfo `URL:` fields are relative to their own
cache ([ADR-0003](adr/0003-tiered-selection.md)).

### Building the plan

`DefaultEngine` (`src/select.rs`) applies, in order:

1. **Affinity** — for a NAR request with a remembered upstream, that upstream
   goes first, unless it is marked down.
2. **Tier order** — tiers strictly in config order, no cross-tier reordering.
3. **Rank within a tier** — ascending `narinfo_ms`, falling back to `probe_ms`.
   Upstreams with neither sort to the back of their tier and keep config order
   among themselves (the sort is stable).
4. **Health gate** — anything with `healthy == false` is dropped entirely. It
   comes back only when a probe says so.

An empty plan (everything down) means a `404` without asking anyone.

The engine's input and output are plain `serde` types, and the core never
reaches around them. That is the seam a scripting engine would slot into later
([ADR-0005](adr/0005-selection-boundary.md)).

### Measurement points

The data `/status` reports, and where it comes from:

| Signal            | Recorded when                                                                       |
| ----------------- | ----------------------------------------------------------------------------------- |
| `healthy`         | Every probe pass; also set `false` immediately on any transport error in a request.  |
| `probe_ms`        | Each successful probe (`GET /nix-cache-info`), as an EWMA.                           |
| `narinfo_ms`      | Each successful narinfo fetch — measured from request start to response headers.     |
| `nar_mbytes_per_sec` | When a NAR body reaches EOF, from bytes and elapsed time. Aborted transfers record nothing, so a cancelled build cannot poison the number. |
| `hits`/`misses`/`errors` | Per attempt: 2xx / upstream 404 / transport failure.                         |

A transport error additionally kicks the probe thread, so recovery is measured
in the seconds it takes one probe pass, not in `probe-interval-secs`.

## HTTP client policy

Two `ureq` agents with deliberately different timeouts:

- **`info_agent`** — narinfo requests, 10 s *global* timeout. Narinfos are tiny;
  anything slow is a broken upstream, not a big download.
- **`nar_agent`** — NAR downloads, 5 s *connect* timeout and no body deadline. A
  multi-gigabyte closure over a slow link must not be killed by a clock.

The probe thread has its own agent with `probe-timeout-secs` as a global
timeout.

## What sito deliberately does not do

- **No cache of its own** — the Nix client already caches narinfo lookups, and a
  second layer would only hide staleness ([ADR-0002](adr/0002-streaming-proxy.md)).
- **No writes** — `nix copy --to` through sito is not proxied; non-`GET`/`HEAD`
  gets `405`.
- **No signature handling** — no keys, no verification, no re-signing
  ([ADR-0006](adr/0006-pass-through-trust.md)).
- **No persistence** — every start is a cold start, ranking falls back to config
  order until probes and traffic fill it in ([ADR-0004](adr/0004-quality-signal.md)).
- **No retry once bytes are flowing** — a NAR request that fails *before* a
  response arrives falls through to the next upstream in the plan like any
  other, but a failure mid-body surfaces to the client. Note that a NAR path
  fetched from an upstream other than the one that served the narinfo only
  works because both caches happen to use the same relative layout; affinity
  exists to make that the rare case.
