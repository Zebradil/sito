# Self-describing NAR URLs: rewrite the narinfo, serve NARs decompressed

Supersedes the in-memory NAR affinity of ADR-0003 and amends ADR-0006.

A narinfo's `URL:` is relative to the cache that served it. Nix keeps
narinfos in its own disk cache for `narinfo-cache-positive-ttl` (30 days by
default), keyed by substituter URL, and to Nix sito is one substituter. So a
NAR request can arrive weeks after its narinfo: after sito restarted and lost
its in-memory affinity map, or while the upstream that served the narinfo is
unreachable (a work cache off VPN). sito then had no way to serve the NAR and
answered 404. Nix moved on to its next substituter or built the path locally,
though another upstream usually held the very same NAR under its own URL.

Options considered:

- **Lower `narinfo-cache-positive-ttl`.** Global, explicit, and it costs more
  narinfo requests for every substituter, not only sito.
- **Persist the affinity map.** Survives restarts, but still has nowhere to go
  when the upstream is down.
- **Rewrite the `URL:` to carry the upstream, keep the compression.** Stateless
  routing, but a substitute upstream's NAR is usable only in the same
  compression, and transcoding into the original one means compressing (xz at
  a few MB/s), which can be slower than building.
- **Rewrite the narinfo and serve every NAR decompressed.** Chosen.

sito rewrites each narinfo it can:

```text
URL: nar/sito/v1/<upstream key>/<store hash>/<NarHash>/<codec>/<original URL>
Compression: none
```

and drops `FileHash` and `FileSize`, which describe the compressed file. The
upstream key is FNV-1a over the upstream's URL, so it survives restarts and
config reorders. A NAR request at such a URL goes to that upstream first (the
affinity upstream, still tried even when marked down). If that upstream
misses or fails, sito asks the others for the narinfo of the same store path,
and the first whose `NarHash` matches serves the NAR from its own URL in its
own compression. Every NAR reaches Nix decompressed, so any upstream holding
the same NAR, by `NarHash`, can stand in for any other. Nix's narinfo cache
becomes sito's persistent affinity store, and sito itself stays stateless.

Why this is safe: a narinfo signature covers only the store path, `NarHash`,
`NarSize` and references, so the rewritten fields are unsigned, and Nix still
checks the NAR it receives against `NarHash`. A buggy sito can still only deny
service; it cannot make Nix accept content the narinfo does not vouch for.

Consequences:

- sito no longer relays narinfo bytes unmodified (amending ADR-0006). Signatures
  are untouched and verification stays in Nix.
- sito decompresses every NAR (xz and zstd). Machine-wide CPU is unchanged,
  since Nix no longer does it; loopback carries the larger raw NAR, which is
  free on localhost and would not be for a sito serving a LAN.
- A narinfo sito cannot rewrite (bzip2 or another compression, an absolute
  `URL:`, no `NarHash`) is relayed unmodified and its NAR verbatim, without a
  fallback.
- A NAR built non-reproducibly, whose `NarHash` only the unreachable upstream
  holds, still 404s: Nix holds a signed narinfo for that content alone.
- Narinfos Nix cached before the rewrite keep their raw URLs until they expire.
  Such a NAR request walks the tiers like any other path; it is served by
  whichever upstream has that path.
- `/status` `affinity_entries` and the `sito_affinity_entries` metric are gone
  with the map.
- Because sito now serves raw NARs identified by `NarHash`, a NAR that breaks
  mid-transfer could later be resumed from another upstream by skipping the
  bytes already sent. Not built.
