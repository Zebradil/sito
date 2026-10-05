---
title: Configure tiers and upstreams
description: Write a sito.toml for a roaming machine, with every key, its default, and the startup checks.
---

This guide writes the config for the case sito is built for: a laptop that is sometimes on a LAN with a fast cache box,
and sometimes not. Running sito through the NixOS or nix-darwin module? The module writes this file from
`services.sito`; see [Run sito with the NixOS or nix-darwin module](../nix-modules/).

## Prerequisites

- sito on `PATH`, or `nix run github:Zebradil/sito --` in place of `sito` below.
- The URL and signing public key of every binary cache you want sito to use.

## 1. Put your own caches in the first tier

Tiers are tried strictly in order, so the first tier holds the caches you prefer: the LAN box, and the remote cache it
mirrors. Within a tier sito ranks upstreams by measured latency, so their order in the file only matters until the first
measurements arrive.

```toml
[[tier]]

  [[tier.upstream]]
  url = "http://box.lan:5000"
  public-keys = ["box-1:AAAA..."]

  [[tier.upstream]]
  url = "https://cache.example.com"
  public-keys = ["box-1:AAAA..."]
```

`url` is the cache root: sito appends request paths to it verbatim, and tolerates a trailing slash. `public-keys` lists
the keys the cache's narinfos are signed with. sito never checks them; they exist so that the Nix modules can put them
into `trusted-public-keys` from the same file.

## 2. Add the public cache as a fallback tier

```toml
[[tier]]

  [[tier.upstream]]
  url = "https://cache.nixos.org"
  public-keys = ["cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="]
```

`cache.nixos.org` is asked only after every healthy upstream in the first tier has missed or failed, however fast it
measures. Off the LAN, once a probe or a failed request has marked the box down, sito skips it without waiting on a
connect timeout.

## 3. Tune the probe, if you need to

Top-level keys go above the first `[[tier]]`. The defaults suit most machines:

```toml
listen = "127.0.0.1:5001"
probe-interval-secs = 15
probe-timeout-secs = 3
max-inflight = 64
```

Lower `probe-interval-secs` to notice a network change sooner, at one small request per upstream per interval. Raise
`probe-timeout-secs` if a legitimate upstream regularly takes longer than 3 s to answer `GET /nix-cache-info`.

## 4. Check the file

Start a throwaway instance on an ephemeral port, so it does not collide with a running sito:

```sh
sito --config ./sito.toml --listen 127.0.0.1:0
```

A valid file logs `sito serving`; stop it with Ctrl-C. An invalid one exits with status 1 and names the problem:

| Problem | Error |
| --- | --- |
| Unknown or misspelled key | ``unknown field `probe-interval`, expected one of `listen`, `probe-interval-secs`, …`` |
| No upstream in any tier | `no upstreams configured` |
| A tier asks for `race` | `tier 0: strategy 'race' is not implemented yet` |
| URL is not `http://` or `https://` | `tier 0: upstream url must be http(s): ftp://x` |
| File missing or unreadable | `read missing.toml` |

Tier numbers in errors count from 0. The file is read once at startup: restart sito after editing it.

## Keys

### Top level

| Key | Default | Meaning |
| --- | --- | --- |
| `listen` | `"127.0.0.1:5001"` | `host:port` to bind. Keep it on loopback: sito authenticates no one and serves whatever its upstreams serve it. |
| `probe-interval-secs` | `15` | Seconds between probe passes. Bounds how long a stale reachability verdict survives a network change. |
| `probe-timeout-secs` | `3` | Deadline for one probe, connect through response. An upstream that misses it counts as down for that pass. |
| `max-inflight` | `64` | Concurrent NAR transfers. Past the cap, further NAR requests wait for a slot; narinfo lookups and sito's own endpoints never wait. Values below 1 count as 1. |
| `[[tier]]` | none | Tiers in the order they are tried. At least one must hold an upstream. |

### `[[tier]]`

| Key | Default | Meaning |
| --- | --- | --- |
| `strategy` | `"sequential"` | `"sequential"`: try upstreams one at a time in rank order, first hit wins. `"race"` is accepted by the parser and rejected at startup: reserved, not implemented. |
| `[[tier.upstream]]` | none | Upstreams in this tier. |

### `[[tier.upstream]]`

| Key | Default | Meaning |
| --- | --- | --- |
| `url` | required | Base URL of an HTTP binary cache, `http://` or `https://`. |
| `public-keys` | `[]` | Signing keys of this cache's narinfos, for the Nix client's `trusted-public-keys`. Ignored by sito itself. |

## Command line

`--config` (or `SITO_CONFIG`) names the file; `--listen` (or `SITO_LISTEN`) overrides `listen` from it. The full list
is in the [CLI reference](../../reference/cli/sito/).

Logging is set by `RUST_LOG`, a
[`tracing_subscriber` filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
that defaults to `sito=info`; see [Troubleshoot sito](../troubleshooting/).
