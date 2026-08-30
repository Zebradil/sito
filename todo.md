# todo

Deferred by design (see `docs/adr/`):

- **`race` strategy** — schema field exists, implementation deferred until a
  real multi-upstream tier wants it (ADR-0003).
- **Scripting engine** for selection rules (rhai/Lua/WASM) — slot in at the
  serializable selection boundary once built-in rules prove insufficient
  (ADR-0005).
- **Idle-burst re-probe** — re-probe all upstreams on the first request after
  an idle gap; decide during implementation (ADR-0004).
- **Rank-state persistence** — only if cold start measurably misroutes
  (ADR-0004).
- **Prometheus metrics** — `/status` JSON suffices until a dashboard exists to
  consume more.
- **OS network-change events** (SCNetworkReachability/netlink) — only if the
  probe interval provably annoys (ADR-0004).
- **Push proxying** (`nix copy --to` through sito) — writing is not sito's
  concern; roaming push (queue when off-LAN?) is a separate design if ever.
- **Redirect mode** — 302 instead of streaming as a per-request optimization,
  once an upstream is chosen on other evidence (ADR-0002).
- **mDNS discovery** — an early rough idea, likely never needed with a stable
  box URL plus reachability probing.
- **Per-request log line at info level** — today request routing is only
  visible at `sito=debug`; `/var/log/sito.log` at the default `sito=info`
  shows startup and probe transitions but no requests. One compact info
  line per request (path, chosen upstream, status, ms) would make the
  production log useful without debug noise.
- **Auth-gated upstreams** — private caches (Cachix, Attic, FlakeHub,
  nix-serve behind Basic) are read as down today, since sito sends no
  credentials. Read the machine's `netrc` via a `netrc-file` key when built
  (ADR-0008).
