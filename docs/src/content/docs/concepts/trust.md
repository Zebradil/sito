---
title: Trust model
description: sito holds no keys and leaves signatures untouched; the Nix client verifies everything, as it would without sito.
---

sito never signs, re-signs, or strips a signature, and holds no key material. The Nix client verifies each narinfo's
signature against its own `trusted-public-keys`, and each NAR against the narinfo's `NarHash`, exactly as it would
talking to the upstream directly.

This keeps sito out of the trust chain. A broken or compromised sito can deny service or pick a slow upstream. It
cannot make Nix accept an unsigned or foreign store path.
Rationale: [ADR-0006](https://github.com/Zebradil/sito/blob/main/docs/adr/0006-pass-through-trust.md).

## What sito authors itself

Only two responses come from sito rather than an upstream:

| Route | Content |
| --- | --- |
| `/nix-cache-info` | `StoreDir: /nix/store`, `WantMassQuery: 1`, `Priority: 10`. Authored locally so that Nix sees a usable substituter even when every upstream is down. |
| `/status` | sito's own diagnostics, as JSON. Not part of the Nix protocol. |

Everything else is either relayed from an upstream (`*.narinfo`, `/nar/*`) or a status code with a short plain-text
body: 404 for an unknown path or an exhausted plan, 405 for any method other than `GET` and `HEAD`.

Relayed responses keep the upstream's status and `Content-Type`, and add one header of sito's own:
`X-Sito-Upstream`, the configured URL of the upstream that answered.

## What sito changes, and why that is safe

sito rewrites each narinfo it relays (see [NAR affinity and substitutes](../selection/#nar-affinity-and-substitutes)):

- `URL:` becomes a self-describing NAR URL under `nar/sito/v1/`.
- `Compression:` becomes `none`, and sito serves the NAR decompressed.
- `FileHash:` and `FileSize:`, which describe the compressed file, are dropped.

Every other line is relayed as received, `Sig:` included. A narinfo signature covers only the store path, `NarHash`,
`NarSize` and references, so none of the rewritten fields are signed, and Nix still checks every NAR it receives
against the signed `NarHash`. A NAR from a substitute upstream is used only when that upstream's narinfo carries the
same `NarHash`, and Nix checks the bytes anyway.

A narinfo sito cannot rewrite is relayed unmodified: one in a compression sito does not decode (only `xz`, `zstd` and
`none` are), one with an absolute `URL:`, or one without a `NarHash`.

A narinfo is read into memory, up to 1 MiB. A larger one goes out truncated and Nix rejects it as malformed; real ones
are well under a kilobyte.

## Where the keys come from

Each upstream in the config carries `public-keys`: the keys its narinfos are signed with. sito ignores the field. It
exists so that one file can drive both the proxy and the client's trust: the NixOS and nix-darwin modules collect every
`public-keys` entry across the tiers into `nix.settings.trusted-public-keys`. Running sito by hand, you keep
`trusted-public-keys` in step yourself.

A signature error from Nix therefore never comes from sito. Nix does not trust the key the upstream signs with.

## Who may use sito

sito authenticates no one and grants its clients whatever its upstreams grant it. Its `listen` address defaults to
`127.0.0.1:5001` for that reason: it is a proxy for the local machine.

It also sends no credentials upstream. A cache that requires authentication answers the probe with 401, and sito marks
it down. Support is deferred, with its shape recorded: read the machine's existing netrc through a `netrc-file` key,
never put secrets in the config, and keep credentials out of upstream URLs, which appear in `/status` and in logs.
Rationale: [ADR-0008](https://github.com/Zebradil/sito/blob/main/docs/adr/0008-auth-gated-upstreams.md).
