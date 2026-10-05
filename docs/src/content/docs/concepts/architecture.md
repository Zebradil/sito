---
title: Architecture
description: Why sito is a streaming proxy, and how a request travels through its threads, plan, and HTTP clients.
---

sito sits in the data path: Nix asks it for every narinfo and NAR, and sito fetches the bytes from an upstream and
streams them through. This page covers why, and what happens at runtime. How the upstream is chosen is
[How sito picks an upstream](../selection/).

## Why a streaming proxy

There are three ways to put a selector between Nix and its caches:

| Approach | Verdict |
| --- | --- |
| Rewrite the substituter list as networks change | Does not fit NixOS or nix-darwin, where `nix.conf` is generated and read-only. |
| Redirect: answer with a 302 to the chosen upstream | Works, since Nix follows redirects, and keeps sito out of the data path. But sito could never measure throughput. |
| Stream: fetch from the chosen upstream and relay the bytes | Measures everything ranking needs, at the cost of one localhost copy per NAR. |

sito streams. The localhost copy is noise next to the network transfer.
Rationale: [ADR-0002](https://github.com/Zebradil/sito/blob/main/docs/adr/0002-streaming-proxy.md).

## Routes

sito speaks the read side of the [Nix binary cache protocol](https://nix.dev/manual/nix/latest/protocols/http-binary-cache),
plus `/status`:

| Route | Methods | Answered by |
| --- | --- | --- |
| `/nix-cache-info` | `GET`, `HEAD` | sito |
| `/status` | `GET`, `HEAD` | sito |
| `*.narinfo` | `GET`, `HEAD` | an upstream, through the selection plan |
| `/nar/*` | `GET`, `HEAD` | an upstream, through the selection plan |
| anything else | `GET`, `HEAD` | sito: `404 not found` |
| anything | other methods | sito: `405 sito is read-only` |

## Threads

There is no async runtime. sito runs three kinds of threads:

- **The accept loop**, on the main thread, takes each request and spawns a worker for it. It waits on nothing else, so
  a stuck transfer never keeps another request from being dispatched.
- **One worker per request** does the whole job: plan, fetch, stream, respond. A NAR worker first takes one of the
  `max-inflight` slots, waiting if none is free, and holds it until the last byte is written. Narinfo requests and
  sito's own endpoints take no slot, so stuck NARs never block lookups or `/status`.
- **The probe thread** probes every upstream in turn, then sleeps for `probe-interval-secs`. A failed request cuts the
  sleep short.

All shared state sits in one registry: per-upstream health, moving averages and counters behind one lock, and the NAR
affinity map behind another. Readers take a copy, so no lock is held across a network call.

## A request, end to end

1. The worker copies the registry and assembles the selection input: request kind and path, the affinity upstream for
   a NAR, and every tier's upstream state.
2. The engine returns the plan, for example upstreams `[2, 0, 3]`.
3. The worker tries each in turn. A 404 counts a miss and moves on. A connection error, timeout, error status other
   than 404, or a body that fails before its first byte counts an error, marks the upstream down, starts a probe pass,
   and moves on.
4. The first upstream that answers with a body wins; no other upstream is asked. For a narinfo, sito records its
   latency and remembers the upstream for the NAR its `URL:` field names.
5. sito relays the response: the upstream's status code and `Content-Type`, and the body byte for byte.

Upstreams are numbered `0..n` in config order across all tiers: tier 0's upstreams first, then tier 1's. That number is
the upstream's identity in the plan, the affinity map, and `/status`.

### What is and is not relayed

- Only `Content-Type` is copied from the upstream's headers. A narinfo carries the `Content-Length` of what sito sends;
  a `HEAD` answer carries the upstream's `Content-Length` and no body.
- NARs go out chunked, without `Content-Length`. sito cannot close a client connection mid-response, so with a fixed
  length a broken transfer would leave Nix waiting until its own stall timeout. A chunked body just ends, and Nix
  rejects the short NAR at once.
- Client request headers are not forwarded. `Range` and `If-None-Match` are ignored, and sito sends no
  `Accept-Ranges`, so Nix cannot resume a broken NAR through it.

### Waiting for the first byte

sito sends nothing to Nix until a NAR's first body chunk has arrived. An upstream that sends headers and then stalls
therefore still falls through to the next one. Once bytes have gone out there is no switching: if the upstream breaks
off or goes 60 s without sending, sito records an error and ends the body.

## HTTP clients

sito uses separate HTTP clients with deliberately different timeouts:

| Client | Timeouts | Why |
| --- | --- | --- |
| Narinfo | 10 s for the whole request | Narinfos are tiny. A slow one means a broken upstream, not a big download. |
| NAR | 5 s DNS, 5 s connect, then 60 s idle on every wait for bytes; no overall deadline | A multi-gigabyte NAR over a slow link must not be killed by a clock, but a flow that went silent (a VPN drop leaves no FIN or RST) must not hold its thread forever. 60 s is well under Nix's 300 s `stalled-download-timeout`, so sito gives up first. |
| Probe | `probe-timeout-secs` for the whole request | Bounds how long one dead upstream can delay a probe pass. |

## What sito deliberately does not do

- **Cache.** The Nix client already caches narinfo lookups; a second layer would only hide staleness. The affinity map
  is metadata, not a response cache.
- **Write.** `nix copy --to` through sito is not proxied.
- **Handle signatures.** No keys, no verification, no re-signing; see [Trust model](../trust/).
- **Persist.** Every start is a cold start.
- **Retry once bytes flow.** A NAR that fails before its first byte falls through to the next upstream; one that fails
  mid-body surfaces to Nix.
