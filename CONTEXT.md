# sito

A local always-on Nix substituter proxy for roaming clients. Nix talks to one
substituter (`http://localhost:<port>`); sito routes each request to the best
reachable upstream binary cache, so a laptop that moves between the home LAN and
the outside world never pays a connect-timeout tax and never needs its
substituter list edited.

Named after the sieve (Russian «сито»): everything pours through, sito decides
which mesh it lands on. A sibling of [kasha](https://github.com/Zebradil/kasha)
(the LAN cache box) in spirit, but strictly independent — neither project
depends on the other, and sito works with any HTTP binary caches.

Status: design phase. Decisions live in `docs/adr/`; deferred work in `todo.md`.

## Language

**Upstream**:
One HTTP binary cache sito can fetch from (a kasha box, a private remote cache,
`https://cache.nixos.org`, …). Configured with its URL and the signing public
key(s) its narinfos carry.
_Avoid_: backend, mirror

**Tier**:
An ordered group of upstreams sharing a selection strategy. Tiers are tried in
order; the first tier that produces a hit wins. The tier is the unit of policy
("race my caches, then fall back to the official cache").

**Strategy**:
How a tier queries its upstreams for one request: `sequential` (by current rank,
first hit wins) or `race` (parallel, first positive answer wins). v1 implements
`sequential` only; the config schema carries the field from day one.

**Selection plan**:
The ordered list of upstream fetch attempts the selection engine emits for one
request. The engine's input (request context, upstream states and metrics) and
output (the plan) are serializable — the seam where an embedded scripting engine
could later replace the built-in Rust rules.
_Avoid_: routing table

**Quality signal**:
What ranking is based on: passive measurements of real traffic (latency,
throughput) plus active reachability probes (timed `GET /nix-cache-info`).
In-memory only; every start is a cold start.

**Probe**:
The active half of the quality signal. Runs on start, on a short interval, and
immediately after a request failure. Answers "is this upstream reachable right
now", which is the whole roaming problem.

**Pass-through**:
sito's trust model: narinfo and NAR bytes are relayed unmodified, signature
verification stays in the Nix client, sito holds no signing keys. The only
response sito authors itself is its own `/nix-cache-info`.
