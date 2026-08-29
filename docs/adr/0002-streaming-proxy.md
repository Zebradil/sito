# Streaming proxy in the data path, not redirects or config rewriting

Three ways to put a selector between Nix and its caches:

- **Config rewriting** — mutate the substituter list as networks change. Does
  not fit NixOS/nix-darwin, where `nix.conf` is generated and read-only.
- **Redirect proxy** — answer narinfo/NAR requests with 302 to the chosen
  upstream; Nix's libcurl follows redirects, so this works and keeps sito out
  of the data path.
- **Streaming proxy** — sito fetches from the chosen upstream and streams the
  bytes through.

Streaming wins because ranking needs data only the data path can provide:
per-upstream throughput is unmeasurable from a redirect. The cost is a
localhost copy per NAR, which is noise next to network transfer. Redirects
remain possible later as a per-request optimization once an upstream is chosen
on other evidence.

sito is read-only (`nix copy --to` pushes are not proxied) and caches nothing:
the Nix client already caches narinfo lookups, and a second cache layer would
only hide staleness. All-upstream misses return 404.
