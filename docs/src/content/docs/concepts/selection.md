---
title: How sito picks an upstream
description: Tiers set the policy, probes decide what is reachable, and measured latency orders the rest.
---

For every narinfo or NAR request, sito builds a **selection plan**: an ordered list of upstreams to try. It walks the
plan until one upstream answers, and answers 404 itself when none does. Three inputs shape the plan: the tiers you
configured, the probe's reachability verdicts, and latency measured on real traffic.

## Tiers are the policy

An **upstream** is one HTTP binary cache. A **tier** is an ordered group of upstreams sharing a selection **strategy**.
Tiers are tried strictly in config order: an upstream in tier 1 is asked only after every healthy upstream in tier 0
has missed or failed, however much faster tier 1 measures. "My own caches first, the public one only on a miss" is one
tier for each half.

A flat ranked list cannot express that, and adding grouping to a flat schema later would break every config. So the
schema is tiered from the start, and carries a `strategy` field per tier:

| Strategy | Behavior |
| --- | --- |
| `sequential` (default) | Try the tier's upstreams one at a time in rank order. A hit ends the walk; a 404 or a failure moves on. |
| `race` | Reserved: parallel fan-out, first positive answer wins. Parsed, then rejected at startup. |

`race` stays unimplemented until a real tier needs it: fanning out to caches someone else pays for, like
`cache.nixos.org`, is rude, and ranking makes it unnecessary in the common case.
Rationale: [ADR-0003](https://github.com/Zebradil/sito/blob/main/docs/adr/0003-tiered-selection.md).

## The quality signal

Ranking draws on two signals with different blind spots:

- **Passive**: narinfo latency and NAR throughput, measured on real requests. Free and honest, but stale the moment
  the machine changes networks while idle.
- **Active**: the **probe**, a timed `GET /nix-cache-info` to every upstream. It runs at startup, every
  `probe-interval-secs` (15 s by default), and immediately after any failed request. It answers the roaming question
  directly: is the LAN box reachable right now?

Probes decide reachability: an upstream whose probe failed, or whose last request failed, is skipped entirely until a
probe succeeds again. Passive measurements order the reachable ones.

Each measurement is an exponentially weighted moving average, weighing the newest sample at 0.3 and seeded with the
first sample. A few requests after a network change, the old numbers have faded.

Everything lives in memory. Each start is a cold start: until probes and traffic fill the numbers in, upstreams keep
their config order. The startup probe settles reachability before the first request, and yesterday's throughput on a
different network is as likely wrong as right.
Rationale: [ADR-0004](https://github.com/Zebradil/sito/blob/main/docs/adr/0004-quality-signal.md).

## Building the plan

The built-in engine applies four rules:

1. **Affinity first.** For a NAR, the upstream that served the narinfo naming it goes first, even when marked down.
2. **Tier order.** Tiers in config order, never reordered across tiers.
3. **Rank within a tier.** Ascending `narinfo_ms`, falling back to `probe_ms`. Upstreams with neither sort last and
   keep config order among themselves.
4. **Health gate.** Upstreams marked down are left out, except the affinity upstream.

An empty plan, when everything is down, is a 404 without asking anyone. Nix then tries its next substituter or builds
the path locally.

### NAR affinity

A narinfo's `URL:` field is relative to the cache that served it, so that cache is usually the only one that can serve
the NAR under that path. sito remembers which upstream answered each narinfo and tries it first for the NAR that
follows. It goes first even when a probe marks it down, because a probe verdict can be seconds stale during a flap.

Affinity is a preference, not a pin. If the remembered upstream misses, the walk continues through the tiers. If it
*fails* and the rest of the walk finds nothing, sito retries it 3 times, 5 s apart, before answering 404. It answers
404 rather than a 5xx on purpose: once Nix's own retries run out, a failed NAR download fails the whole build unless
`--fallback` is set, while a 404 lets Nix build that one path locally.

sito remembers up to 4096 paths, then forgets them all at once; a forgotten path costs one ordinary tier walk.

## The selection boundary

The engine sees one serializable input (the request kind and path, the affinity upstream, and every tier's strategy
and upstream state) and returns one serializable plan (upstream indices in order). Nothing in the proxy reaches around
that boundary.

The rules are plain Rust today; changing them is a rebuild, which is the normal `nixos-rebuild` loop anyway. The
boundary is where an embedded scripting engine could replace them later without touching the proxy.
Rationale: [ADR-0005](https://github.com/Zebradil/sito/blob/main/docs/adr/0005-selection-boundary.md).

To see the inputs the engine ranks on, read [`/status`](../../guides/troubleshooting/#3-read-status).
