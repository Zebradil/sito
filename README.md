# sito

A local always-on Nix substituter proxy for roaming clients.

Nix is configured with a single substituter — `http://localhost:5001` — and
sito routes every narinfo/NAR request to the best reachable upstream binary
cache. A laptop that moves between the home LAN (fast local cache box) and the
outside world (remote cache, cache.nixos.org) gets the fastest reachable
source, instantly, with no substituter-list editing and no connect-timeout tax.

Status: core v1 (`sequential` strategy, probing, `/status`) and the nixosModule
and darwinModule are implemented.

## Documentation

- [Configuration](docs/configuration.md) — every TOML key, CLI flag, and
  `services.sito` module option, with defaults.
- [HTTP API](docs/http-api.md) — the routes sito answers and every field
  `/status` reports.
- [Architecture](docs/architecture.md) — threads, request lifecycle, where the
  numbers come from.
- [Running and troubleshooting](docs/operations.md) — service management, logs,
  symptom-to-cause table, dev loop.
- [`CONTEXT.md`](CONTEXT.md) — the project's vocabulary.
- [`docs/adr/`](docs/adr/) — why it is built this way; [`todo.md`](todo.md) —
  what was deliberately deferred.

## Shape

- Single static Rust binary, streaming proxy, read-only, no cache of its own.
- Upstreams are configured in ordered **tiers**, each with a selection
  strategy (`sequential` now, `race` reserved).
- Ranking from passive traffic metrics plus active reachability probes.
- Trust stays in the Nix client: narinfos pass through unmodified, sito holds
  no keys.
- `/status` JSON endpoint exposes what the ranker sees
  ([field reference](docs/http-api.md#get-status)).
- Ships a nixosModule (`x86_64-linux`) and darwinModule (`aarch64-darwin`)
  that run the daemon and manage `substituters` / `trusted-public-keys` by
  default.

## Config sketch

```toml
listen = "127.0.0.1:5001"

[[tier]]
strategy = "sequential"

  [[tier.upstream]]
  url = "http://box.lan:5000"
  public-keys = ["znix.zebradil.dev:AAAA..."]

  [[tier.upstream]]
  url = "https://znix.zebradil.dev"
  public-keys = ["znix.zebradil.dev:AAAA..."]

[[tier]]
strategy = "sequential"

  [[tier.upstream]]
  url = "https://cache.nixos.org"
  public-keys = ["cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="]
```

## NixOS / nix-darwin module

```nix
{
  inputs.sito.url = "github:Zebradil/sito";

  outputs = { self, nixpkgs, sito, ... }: {
    nixosConfigurations.laptop = nixpkgs.lib.nixosSystem {
      modules = [
        sito.nixosModules.default
        {
          services.sito = {
            enable = true;
            settings = {
              listen = "127.0.0.1:5001";
              tier = [
                {
                  strategy = "sequential";
                  upstream = [
                    { url = "http://box.lan:5000"; public-keys = [ "znix.zebradil.dev:AAAA..." ]; }
                    { url = "https://znix.zebradil.dev"; public-keys = [ "znix.zebradil.dev:AAAA..." ]; }
                  ];
                }
                {
                  strategy = "sequential";
                  upstream = [
                    { url = "https://cache.nixos.org"; public-keys = [ "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=" ]; }
                  ];
                }
              ];
            };
          };
        }
      ];
    };
  };
}
```

`darwinModules.default` mirrors this for nix-darwin. `services.sito.settings` is
the TOML config above as a Nix attrset — every field the binary understands is
expressible, with no separate option per knob. With the default
`manageSubstituters = true`, the module points `nix.settings.substituters` at
sito's own `listen` address and trusts every `public-keys` entry found across
`settings.tier` (see [ADR 0007](docs/adr/0007-nix-modules.md)). Full option
reference: [docs/configuration.md](docs/configuration.md#nix-module-options).

## CI

[zebradil/nix-ci](https://github.com/zebradil/nix-ci) provides the workflow:
`nix flake check --no-build` up front, then `checks.<system>.*` — `build`
(the package, which runs `cargo test` in its own checkPhase), `fmt`
(`cargo fmt --check`), `clippy` — built and pushed to kasha's remote cache on
every push to `main`. Pull requests read that cache but never write to it, so
a client that trusts the key substitutes sito instead of compiling it:

```nix
nix.settings = {
  substituters = [ "https://znix.zebradil.dev" ];
  trusted-public-keys = [ "kasha-ci-1:KNW/sz+Zz800U/IFZ38vH5rvlHtM3Fb0Q/wmDJquG+U=" ];
};
```

Retention is kasha's: after the push, CI calls kasha's `emit-manifest` action
to file a generation manifest under `roots/sito/`, one per system — a push
with no manifest is invisible to kasha's retention and gets garbage
collected on the next sweep. See `.github/workflows/ci.yml`.

## Relation to kasha

sito is a sibling of [kasha](https://github.com/Zebradil/kasha) — the LAN
cache box it was designed around — but neither runtime depends on the other:
sito proxies any HTTP binary caches, kasha serves fine without sito. The only
link is at build time, above: sito's CI publishes into kasha's cache and
manifests it with kasha's own actions.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
