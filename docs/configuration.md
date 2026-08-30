# Configuration

sito is configured by one TOML file, with two overrides available as CLI flags
or environment variables. Config is read once at startup — there is no
live-reload, by design ([ADR-0005](adr/0005-selection-boundary.md)): the Nix
module regenerates the file and restarts the service on rebuild.

## Command line

```
sito --config <PATH> [--listen <ADDR>]
```

| Flag              | Environment    | Required | Meaning                                              |
| ----------------- | -------------- | -------- | ---------------------------------------------------- |
| `--config <PATH>` | `SITO_CONFIG`  | yes      | Path to the TOML file below.                         |
| `--listen <ADDR>` | `SITO_LISTEN`  | no       | Overrides `listen` from the file. Handy for testing. |

`--version` prints the crate version.

Logging is controlled by `RUST_LOG` (a
[`tracing_subscriber` filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html));
the default is `sito=info`. See [operations.md](operations.md).

## The TOML file

All keys are **kebab-case**, and unknown keys are a hard error rather than a
silent typo. At least one upstream must be configured.

```toml
# Where sito listens. Nix points at exactly this address.
listen = "127.0.0.1:5001"

# How the quality signal is gathered (ADR-0004).
probe-interval-secs = 15
probe-timeout-secs = 3

# Hard cap on concurrent in-flight requests.
max-inflight = 64

# Tier 0: my own caches, LAN first, its public mirror second.
[[tier]]
strategy = "sequential"

  [[tier.upstream]]
  url = "http://box.lan:5000"
  public-keys = ["znix.zebradil.dev:AAAA..."]

  [[tier.upstream]]
  url = "https://znix.zebradil.dev"
  public-keys = ["znix.zebradil.dev:AAAA..."]

# Tier 1: only consulted when tier 0 has nothing.
[[tier]]
strategy = "sequential"

  [[tier.upstream]]
  url = "https://cache.nixos.org"
  public-keys = ["cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="]
```

### Top-level keys

| Key                   | Type   | Default            | Meaning                                                                                                                                                                      |
| --------------------- | ------ | ------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `listen`              | string | `"127.0.0.1:5001"` | `host:port` to bind. Keep it on loopback: sito is an unauthenticated read-only proxy for the local machine. Port `0` binds an ephemeral port (used by the tests).                |
| `probe-interval-secs` | int    | `15`               | Seconds between probe passes. This bounds how long stale reachability data can survive a network change. Lower costs one tiny request per upstream per interval.                |
| `probe-timeout-secs`  | int    | `3`                | Global per-probe timeout. An upstream that does not answer `GET /nix-cache-info` within this is marked down. Must comfortably exceed the worst legitimate round-trip.            |
| `max-inflight`        | int    | `64`               | Maximum requests being served at once. A slot is held for the *entire* transfer, NAR body included; past the cap the accept loop blocks, which is the backpressure. Values below 1 are treated as 1. |
| `[[tier]]`            | array  | —                  | Ordered tiers. Required in practice: a config with no upstreams anywhere is rejected.                                                                                          |

### `[[tier]]`

| Key                | Type   | Default        | Meaning                                                     |
| ------------------ | ------ | -------------- | ----------------------------------------------------------- |
| `strategy`         | string | `"sequential"` | `"sequential"` or `"race"` — see below.                      |
| `[[tier.upstream]]` | array | `[]`           | Upstreams in this tier, in config order.                     |

Tiers are tried **strictly in order**. Every upstream in tier 0 is attempted
before tier 1 is consulted, no matter how much faster tier 1 measures — that is
the point of a tier ([ADR-0003](adr/0003-tiered-selection.md)). Ordering
*within* a tier is what the measurements decide.

**Strategies:**

- `sequential` — try the tier's upstreams one at a time, in current rank order;
  the first hit wins, a 404 moves to the next.
- `race` — parallel fan-out, first positive answer wins. **Parsed but rejected
  at startup**: the schema carries the field from day one so adding it later is
  not a breaking config change, but v1 does not implement it. A config using it
  fails with `tier N: strategy 'race' is not implemented yet`.

### `[[tier.upstream]]`

| Key           | Type     | Default | Meaning                                                                                                                            |
| ------------- | -------- | ------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `url`         | string   | —       | Required. Base URL of an HTTP binary cache; must start with `http://` or `https://`. A trailing slash is tolerated. Paths are appended verbatim, so the URL must be the cache root. |
| `public-keys` | [string] | `[]`    | The signing keys this upstream's narinfos carry. **sito never verifies signatures** ([ADR-0006](adr/0006-pass-through-trust.md)); the field exists so one file can drive both the proxy and the Nix module's `trusted-public-keys`. Running sito standalone, it is documentation only. |

### Validation

`sito` refuses to start (with a message naming the tier index) when:

- the file has a key sito does not know, or the wrong type for one;
- no tier has any upstream;
- a tier uses `strategy = "race"`;
- an upstream `url` is not `http(s)`.

Check a file without touching the running daemon by pointing a throwaway
instance at it:

```console
$ sito --config ./sito.toml --listen 127.0.0.1:0
```

It exits nonzero with the error, or logs `sito serving` and can be killed.

## Nix module options

`services.sito` is provided by both `nixosModules.default` and
`darwinModules.default`, sharing one option surface
([ADR-0007](adr/0007-nix-modules.md)).

| Option                            | Type      | Default                        | Meaning                                                                                                                                  |
| --------------------------------- | --------- | ------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `enable`                          | bool      | `false`                        | Run the daemon (systemd on NixOS, launchd on darwin).                                                                                     |
| `package`                         | package   | `sito.packages.<system>.sito`  | Which build to run.                                                                                                                       |
| `settings`                        | attrs     | `{}`                           | The TOML file above, as a Nix attrset with the same kebab-case keys: `listen`, `probe-interval-secs`, `probe-timeout-secs`, `max-inflight`, and a `tier` list whose entries have `strategy` and an `upstream` list. Deliberately one option instead of one option per knob. |
| `manageSubstituters`              | bool      | `true`                         | Point `nix.settings.substituters` at `http://<listen>` and set `nix.settings.trusted-public-keys` to every `public-keys` entry found across `settings.tier` (flattened and deduped). Set false to curate both by hand. |
| `extraFallbackSubstituters`       | [string]  | `[]`                           | Extra substituters appended after sito's own endpoint, only while `manageSubstituters` is true. Usually unnecessary — sito already queries every upstream, and a dead sito answers connection-refused instantly. |
| `logLevel`                        | string    | `"sito=info"`                  | `RUST_LOG` filter for the daemon.                                                                                                         |

`settings.listen` is optional even with `manageSubstituters = true`: the module
falls back to the same `127.0.0.1:5001` default the binary uses when deriving
the substituter URL.

A worked example lives in the [README](../README.md#nixos--nix-darwin-module).
