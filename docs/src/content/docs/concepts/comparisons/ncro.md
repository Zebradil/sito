---
title: sito vs ncro
description: How sito differs from ncro, another Nix binary cache router, and which machine each one is built for.
---

sito and [ncro](https://github.com/manic-systems/ncro) both put one substituter in front of several Nix binary caches.
They aim at different machines. sito is a small routing daemon for a laptop that moves between networks. ncro is a
bigger routing layer for a server or fleet: it stores routing decisions on disk, checks signatures itself and can share
routes between machines.

This comparison reads ncro at commit `bb4fb83` (2026-09-22): its README, architecture doc and router source. Nothing
about ncro here was measured by running it.

| Aspect | sito | ncro |
| --- | --- | --- |
| Size | Under 2k lines of Rust in one crate | ~7.7k lines across 11 crates |
| Runtime | Plain threads, `tiny_http` + `ureq`, no async runtime | tokio + axum + reqwest; also sqlx, moka, mdns-sd, AWS SDK |
| Target | Roaming laptop (`aarch64-darwin` + `x86_64-linux`, NixOS and nix-darwin modules) | Server or fleet. The flake builds only `x86_64-linux` and `aarch64-linux`; NixOS module only, no darwin |
| Upstream model | Ordered tiers, tried one after another; `race` parses in the config but is rejected until built | Flat list plus `priority` to break ties within 10% latency, plus a separate `fallback_cache` |
| narinfo selection | Tries upstreams in rank order, one request at a time | On a route-cache miss, sends `HEAD` to **every** healthy upstream at once; the fastest wins |
| Ranking signal | Measured narinfo latency first, probe latency as fallback, NAR throughput tracked. Probes every ~15 s, and at once after a failure | EMA of probe latency, probes every 30 s. 15 s cooldown on transient errors |
| NAR routing | Remembers, in memory, which upstream served the narinfo and asks it first | Route stored in SQLite; if the first upstream sends no byte within 1 s, a second request starts in parallel (hedging) |
| Persistence | None; every start is cold | SQLite: routes (default TTL 1 h, LRU limit 100k), misses (10 min), health snapshots |
| narinfo caching | None; bytes relayed unmodified | Caches the **narinfo body** and serves warm hits without asking any upstream |
| Trust | Pass-through, holds no keys | Checks narinfo signatures against each upstream's keys and **rewrites the `URL:` line** (modes `Keep`, `ToSelf`, `ToUpstream`) |
| Auth | None yet; `netrc` planned | `netrc`, per-upstream username and password in config, `s3://` upstreams |
| Extras | `/status` JSON, `/metrics` (Prometheus), `X-Sito-Upstream` response header | `/metrics` (Prometheus), `/health`, `/status`, `X-Ncro-Upstream` and `X-Ncro-Route` response headers, path filters per upstream, mDNS discovery, signed UDP gossip between nodes, a Grafana dashboard |

## Where the designs differ

**Fanning out to every cache.** On every narinfo miss, ncro asks cache.nixos.org and every other healthy upstream at
the same time. sito rejects that as rude to public caches
([ADR-0003](https://github.com/Zebradil/sito/blob/main/docs/adr/0003-tiered-selection.md)). ncro limits it with
concurrency caps and a memory of recent misses, but still sends the requests.

**Stale answers.** ncro keeps narinfo bodies for up to an hour: the second cache layer sito leaves out
([ADR-0002](https://github.com/Zebradil/sito/blob/main/docs/adr/0002-streaming-proxy.md)). Because the body is
cached, ncro's warm requests never touch the network, which is where its advertised speed-up comes from. That benchmark
has not been checked here.

**Where trust lives.** ncro sits inside the trust chain: it verifies and rewrites narinfos. Nix still checks the
signature, since the `URL:` line is not part of what is signed (from Nix's signing format; not checked in ncro's code).
A compromised ncro can still point downloads at a NAR of its choosing, which Nix's hash check should reject (not
verified). sito can only delay or deny a download; see [Trust model](../../trust/).

**Roaming.** ncro assumes the network does not change: it probes every 30 s, keeps routes for an hour, and keeps them
across restarts. After a laptop wakes on a new network, it could keep sending requests to an unreachable LAN cache
until those routes are rejected. This is read from the code, not tested. sito's short probe interval, immediate
re-probe on failure and cold start are built for exactly this case
([ADR-0004](https://github.com/Zebradil/sito/blob/main/docs/adr/0004-quality-signal.md)).

## Which one to pick

Pick sito for a laptop or any machine that changes networks, on macOS or Linux, when you want Nix to keep verifying
everything itself. Pick ncro for a fixed server or fleet that benefits from persistent routes, shared routing between
nodes, or private `s3://` and password-protected caches today.
