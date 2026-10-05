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

The docs site, [zebradil.github.io/sito](https://zebradil.github.io/sito/), has:

- [Getting started](https://zebradil.github.io/sito/getting-started/) — run sito and fetch a path through
  it in a few minutes.
- [Configure tiers and upstreams](https://zebradil.github.io/sito/guides/configure-upstreams/) — every
  TOML key and CLI flag, with defaults.
- [Run sito with the NixOS or nix-darwin module](https://zebradil.github.io/sito/guides/nix-modules/) —
  every `services.sito` option.
- [Troubleshoot sito](https://zebradil.github.io/sito/guides/troubleshooting/) — logs, every `/status`
  field, symptom-to-cause list.
- Concepts: [how sito picks an upstream](https://zebradil.github.io/sito/concepts/selection/),
  [trust model](https://zebradil.github.io/sito/concepts/trust/), [architecture](https://zebradil.github.io/sito/concepts/architecture/).

In this repository:

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
  ([field reference](https://zebradil.github.io/sito/guides/troubleshooting/#3-read-status)).
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
            tiers = {
              lan = {
                priority = 10;
                upstreams = {
                  box = { url = "http://box.lan:5000"; public-keys = [ "znix.zebradil.dev:AAAA..." ]; };
                  remote = { url = "https://znix.zebradil.dev"; public-keys = [ "znix.zebradil.dev:AAAA..." ]; };
                };
              };
              public.upstreams.nixos = {
                url = "https://cache.nixos.org";
                public-keys = [ "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=" ];
              };
            };
          };
        }
      ];
    };
  };
}
```

`darwinModules.default` mirrors this for nix-darwin. `services.sito.tiers`
renders into the config's `[[tier]]` list, sorted by `priority` (default 1000,
ties broken by name; upstreams within a tier likewise). Because tiers and
upstreams are named, another module can add to an existing tier —
`services.sito.tiers.lan.upstreams.work.url = "http://work.example:5000";` —
which a raw list cannot do. Every other field goes in `services.sito.settings`,
the TOML config above as a Nix attrset; `settings.tier` still works but is
mutually exclusive with `tiers`. With the default
`manageSubstituters = true`, the module points `nix.settings.substituters` at
sito's own `listen` address and trusts every `public-keys` entry found across
the tiers (see [ADR 0007](docs/adr/0007-nix-modules.md)). Full option
reference: [the module guide](https://zebradil.github.io/sito/guides/nix-modules/#options).

## Development

```console
$ nix develop                     # cargo, clippy, rustfmt, rust-analyzer
$ cargo test                      # unit tests plus the end-to-end suite
$ cargo clippy --all-targets --all-features -- -D warnings
$ cargo fmt --all
$ nix flake check -L              # what CI's build job builds: fmt, clippy,
                                  # and the package (cargo test again, in checkPhase)
```

`tests/e2e.rs` starts real mock upstreams and a real sito on ephemeral ports —
tier fallback, affinity, dead upstreams and the read-only surface are covered
there rather than with mocks. The docs site lives in `docs/` (docs-kit); build
it with `npm ci && npm run build` from there.

## CI

[zebradil/nix-ci](https://github.com/zebradil/nix-ci) provides the workflow:
`nix flake check --no-build` up front, then `checks.<system>.*` — `build`
(the package, which runs `cargo test` in its own checkPhase), `fmt`
(`cargo fmt --check`), `clippy` — built and pushed to kasha's remote cache on
every push to `main` and every pull request from this repository. Fork pull
requests get no secrets, so they build without publishing. A client that
trusts the key substitutes sito instead of compiling it:

```nix
nix.settings = {
  substituters = [ "https://znix.zebradil.dev" ];
  trusted-public-keys = [ "kasha-ci-1:KNW/sz+Zz800U/IFZ38vH5rvlHtM3Fb0Q/wmDJquG+U=" ];
};
```

Retention is kasha's: after each push, nix-ci's build action files a kasha
generation manifest under `roots/sito/`, in retention group
`checks-<system>` — a push with no manifest is invisible to kasha's retention
and gets garbage collected on the next sweep. See `.github/workflows/ci.yml`.

## Relation to kasha

sito is a sibling of [kasha](https://github.com/Zebradil/kasha) — the LAN
cache box it was designed around — but neither runtime depends on the other:
sito proxies any HTTP binary caches, kasha serves fine without sito. The only
link is at build time, above: sito's CI publishes into kasha's cache and
manifests it with kasha's own actions.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
