# Upstreams form ordered tiers; v1 implements sequential selection only

A flat ranked list cannot express the intended policies ("race my own caches in
parallel, fall back to cache.nixos.org only on miss"), and retrofitting grouping
into a flat schema is a breaking config change. So the config schema is tiered
from day one: an ordered list of tiers, each tier an ordered list of upstreams
plus a `strategy` field (`sequential` | `race`).

v1 implements only `sequential`: within a tier, upstreams are tried by current
rank, skipping upstreams the probe marks down; a hit wins, a miss moves on; when
every tier misses, sito answers 404 and Nix proceeds (next substituter or local
build). A NAR is tried first at the upstream whose narinfo answered, because
narinfo `URL:` fields are relative to their own cache; that affinity is a
preference, not a pin, so if the remembered upstream 404s or fails the walk
carries on through the tiers. Caches that share the content-addressed
`nar/<filehash>` layout will have the path or 404 cheaply, and a body from the
wrong file fails the client's own NAR hash check, so the extra attempts cost
nothing but a round trip.

`race` stays in the schema but unimplemented until wanted: parallel fan-out
against upstreams someone else pays for (cache.nixos.org) is rude, and rank
data makes it unnecessary for the common case.
