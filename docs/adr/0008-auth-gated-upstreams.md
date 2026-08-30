# Auth-gated upstreams: netrc when built, never secrets in config

sito sends no credentials today, so a private cache answers `401` and the
probe correctly marks it down. Support is deferred, not rejected — this
records the shape it takes when built, so the question is not re-researched.

Nix's only credential mechanism for HTTP substituters is `netrc`, in curl
syntax, which means HTTP Basic. Every auth-gated cache Nix can already use
therefore accepts Basic: Cachix private caches (per-cache read token), Attic
(JWT, whose token endpoint accepts `Basic` with the token after the colon just
as it accepts `Bearer`), FlakeHub Cache, harmonia or nix-serve behind nginx.
No Bearer path is needed. `s3://` (SigV4) and nixbuild.net (SSH transport) are
different protocols and out of scope for an HTTP proxy.

So sito should read the netrc the machine already has, via a `netrc-file`
config key mirroring nix.conf's own, rather than growing per-upstream token
fields. On NixOS `services.sito.settings` renders into `/nix/store`, which is
world-readable: a path to a secret is fine there, a secret is not.

Two consequences that are easy to get wrong:

- Credentials must never reach `registry.url()`. That string is served from
  `/status` and appears in every log line about an upstream. ureq does send
  Basic from URI userinfo, so `url = "https://:token@cache.example.com"`
  authenticates today with no sito change — and leaks the token into the store
  path, `/status`, and the logs. `Config::parse` should reject userinfo in an
  upstream URL and point at `netrc-file` instead.
- Both call sites need the credentials, not just the obvious one. Wiring only
  `proxy::forward` leaves `probe::probe_all` unauthenticated, so a cache that
  serves authenticated requests still reads `healthy: false` and is never
  selected.

The nix modules need a wrinkle: `DynamicUser = true` cannot read a root-owned
`/etc/nix/netrc`, so systemd should pass it through the credential store
(`LoadCredential`) rather than the service user being granted access to the
file. darwin's launchd job runs as root and can read the path directly.
