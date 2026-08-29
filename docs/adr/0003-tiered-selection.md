# Upstreams form ordered tiers; v1 implements sequential selection only

A flat ranked list cannot express the intended policies ("race my own caches in
parallel, fall back to cache.nixos.org only on miss"), and retrofitting grouping
into a flat schema is a breaking config change. So the config schema is tiered
from day one: an ordered list of tiers, each tier an ordered list of upstreams
plus a `strategy` field (`sequential` | `race`).

v1 implements only `sequential`: within a tier, upstreams are tried by current
rank, skipping upstreams the probe marks down; a hit wins, a miss moves on; when
every tier misses, sito answers 404 and Nix proceeds (next substituter or local
build). The NAR is fetched from the upstream whose narinfo answered — narinfo
URLs are relative to their cache, so cross-upstream NAR retry is not attempted
in v1.

`race` stays in the schema but unimplemented until wanted: parallel fan-out
against upstreams someone else pays for (cache.nixos.org) is rude, and rank
data makes it unnecessary for the common case.
