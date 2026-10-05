---
title: Getting started
description: Run sito on your machine, fetch a store path through it, and watch it rank the upstream.
---

In this tutorial you run sito in front of `cache.nixos.org`, copy `hello` through it into a scratch store, and read
what sito measured along the way. It takes a few minutes and changes nothing in your system configuration.

You need Nix with the `nix-command` and `flakes` experimental features enabled, `curl`, and `jq`. The sito flake
builds for `x86_64-linux` and `aarch64-darwin`. Everything happens in a scratch directory:

```sh
mkdir sito-tutorial && cd sito-tutorial
```

## 1. Write a config

sito reads one TOML file. The smallest useful one has a single tier with a single upstream:

```sh
cat > sito.toml <<'EOF'
[[tier]]

  [[tier.upstream]]
  url = "https://cache.nixos.org"
  public-keys = ["cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="]
EOF
```

Every other key takes its default; sito listens on `127.0.0.1:5001`.

## 2. Start sito

In a second terminal, in the same directory:

```sh
nix run github:Zebradil/sito -- --config ./sito.toml
```

sito logs one line when it starts serving and one when the first probe reaches the upstream:

```text
INFO sito: sito serving listen="127.0.0.1:5001" upstreams=1
INFO sito::probe: upstream state changed url="https://cache.nixos.org" up=true reason=""
```

Back in the first terminal, ask sito what it sees:

```sh
curl -s http://127.0.0.1:5001/status | jq '.upstreams[0]'
```

```json
{
  "errors": 0,
  "healthy": true,
  "hits": 0,
  "index": 0,
  "misses": 0,
  "nar_mbytes_per_sec": null,
  "narinfo_ms": null,
  "probe_ms": 21.67925,
  "tier": 0,
  "url": "https://cache.nixos.org"
}
```

The probe has timed `GET /nix-cache-info` once (`probe_ms`) and marked the upstream `healthy`. No traffic has gone
through yet, so `narinfo_ms` is still `null`. Your numbers differ.

## 3. Fetch a path through sito

Copy `hello` and its runtime dependencies into `./client`, a scratch store, with sito as the only source:

```sh
p=$(nix eval --raw nixpkgs#hello.outPath)
nix copy --from http://127.0.0.1:5001 --to ./client "$p"
```

```text
copying 2 paths...
copying path '/nix/store/ls125wfdax9gk2ryq7fgzrncpi6x5v2s-libiconv-115.100.1' from 'http://127.0.0.1:5001'...
copying path '/nix/store/mgc3m5ad39b84vkdrca6zd9jan5a28c2-hello-2.12.3' from 'http://127.0.0.1:5001'...
```

The store paths and the number of paths depend on your platform and nixpkgs revision. Nix accepted them because they
carry `cache.nixos.org`'s signature and Nix trusts that key by default. sito passed the bytes through unmodified and
holds no keys of its own.

## 4. Read what sito measured

```sh
curl -s http://127.0.0.1:5001/status | jq '.upstreams[0] | {url, healthy, narinfo_ms, hits, misses}'
```

```json
{
  "url": "https://cache.nixos.org",
  "healthy": true,
  "narinfo_ms": 40.16627509999999,
  "hits": 4,
  "misses": 0
}
```

Each path took one narinfo and one NAR request, all answered by the upstream (`hits`). `narinfo_ms` is now the real
latency Nix waited on. With several upstreams in a tier, it is the number sito ranks them by.

## 5. Clean up

Stop sito with Ctrl-C. Store paths are written read-only, so make them writable before removing the scratch store:

```sh
chmod -R u+w client && cd .. && rm -rf sito-tutorial
```

## Next steps

- [Configure tiers and upstreams](../guides/configure-upstreams/) for a LAN cache with a public fallback.
- [Run sito with the NixOS or nix-darwin module](../guides/nix-modules/) so every Nix command goes through it.
- [How sito picks an upstream](../concepts/selection/) explains tiers, probes and ranking.
