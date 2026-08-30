# sito

A local always-on Nix substituter proxy for roaming clients.

Nix is configured with a single substituter — `http://localhost:5001` — and
sito routes every narinfo/NAR request to the best reachable upstream binary
cache. A laptop that moves between the home LAN (fast local cache box) and the
outside world (remote cache, cache.nixos.org) gets the fastest reachable
source, instantly, with no substituter-list editing and no connect-timeout tax.

Status: core v1 (`sequential` strategy, probing, `/status`) and the nixosModule
and darwinModule are implemented. Design background lives in
[`CONTEXT.md`](CONTEXT.md) and [`docs/adr/`](docs/adr/); deferred ideas in
[`todo.md`](todo.md).

## Shape

- Single static Rust binary, streaming proxy, read-only, no cache of its own.
- Upstreams are configured in ordered **tiers**, each with a selection
  strategy (`sequential` now, `race` reserved).
- Ranking from passive traffic metrics plus active reachability probes.
- Trust stays in the Nix client: narinfos pass through unmodified, sito holds
  no keys.
- `/status` JSON endpoint exposes what the ranker sees.
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
`settings.tier` (see [ADR 0007](docs/adr/0007-nix-modules.md)).

## Relation to kasha

sito is a sibling of [kasha](https://github.com/Zebradil/kasha) — the LAN
cache box it was designed around — but neither depends on the other: sito
proxies any HTTP binary caches, kasha serves fine without sito.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
