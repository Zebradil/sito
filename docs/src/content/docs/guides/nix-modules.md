---
title: Run sito with the NixOS or nix-darwin module
description: Enable services.sito so the daemon runs permanently and Nix uses it as its substituter.
---

The flake ships `nixosModules.default` (systemd, `x86_64-linux`) and `darwinModules.default` (launchd,
`aarch64-darwin`). Both expose the same `services.sito` options, run the daemon, and by default point
`nix.settings.substituters` at it.

## Prerequisites

- A flake-based NixOS or nix-darwin configuration.
- The URL and signing public key of each cache; [Configure tiers and upstreams](../configure-upstreams/) explains how
  to group them.

## 1. Add the flake input and the module

```nix
{
  inputs.sito.url = "github:Zebradil/sito";

  outputs = { nixpkgs, sito, ... }: {
    nixosConfigurations.laptop = nixpkgs.lib.nixosSystem {
      modules = [
        sito.nixosModules.default
        ./sito.nix
      ];
    };
  };
}
```

On nix-darwin, add `sito.darwinModules.default` to the `darwinSystem` modules instead.

## 2. Declare tiers and upstreams

In `sito.nix`, name each tier and each upstream. Lower `priority` comes first; equal priorities (default `1000`) sort
by name:

```nix
{
  services.sito = {
    enable = true;
    tiers = {
      lan = {
        priority = 10;
        upstreams = {
          box = { url = "http://box.lan:5000"; public-keys = [ "box-1:AAAA..." ]; };
          remote = { url = "https://cache.example.com"; public-keys = [ "box-1:AAAA..." ]; };
        };
      };
      public.upstreams.nixos = {
        url = "https://cache.nixos.org";
        public-keys = [ "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=" ];
      };
    };
  };
}
```

Because tiers and upstreams are named, any other module can add to them. A work profile can contribute one line:

```nix
{ services.sito.tiers.lan.upstreams.work.url = "http://work.example:5000"; }
```

With both modules, the generated config has `box`, `remote` and `work` in the first tier, in that order, and
`cache.nixos.org` in the second.

Set every other config key through `services.sito.settings`, an attrset with the TOML file's kebab-case keys:

```nix
{ services.sito.settings.probe-interval-secs = 10; }
```

`settings.tier` also works, as a raw list, but cannot be combined with `tiers`: evaluation fails with
``services.sito: set either `tiers` or `settings.tier`, not both.``

## 3. Rebuild

Run `nixos-rebuild switch` or `darwin-rebuild switch`. The module regenerates the TOML file and the service runs
`sito --config <generated file>`; the service restarts on every config change, since sito reads its config only at
startup.

With the default `manageSubstituters = true`, the module also sets:

- `nix.settings.substituters` to `http://<listen>` (`http://127.0.0.1:5001` unless `settings.listen` says otherwise),
  followed by `extraFallbackSubstituters`.
- `nix.settings.trusted-public-keys` to every `public-keys` entry across the tiers, deduplicated.

NixOS and nix-darwin append their own default, `https://cache.nixos.org/` and its key, to those lists, so the effective
list on the example above is `[ "http://127.0.0.1:5001" "https://cache.nixos.org/" ]`. sito advertises `Priority: 10`,
so Nix asks it first, and asks `cache.nixos.org` directly only after sito answers 404. To make sito the only
substituter, force the list:

```nix
{ lib, ... }: { nix.settings.substituters = lib.mkForce [ "http://127.0.0.1:5001" ]; }
```

To curate both lists by hand instead, set `manageSubstituters = false` and add sito's address yourself.

## 4. Check the service

On NixOS the unit is `sito`, logging to the journal:

```sh
systemctl status sito
journalctl -u sito -f
```

On nix-darwin the daemon is `org.nixos.sito`, logging to `/var/log/sito.log`:

```sh
tail -f /var/log/sito.log
sudo launchctl kickstart -k system/org.nixos.sito
```

:::caution[Unverified]
The service commands in this step were not run for this page: the systemd ones need a NixOS host, and the launchd ones
would touch a live daemon. The unit name, label, and log path come from evaluating the modules.
:::

Then confirm sito answers: `curl -s http://127.0.0.1:5001/status`. [Troubleshoot sito](../troubleshooting/) explains
the output.

## 5. Ship metrics off a roaming Mac (nix-darwin, optional)

sito keeps no history. On nix-darwin, `services.sito.vmagent` runs [vmagent](https://docs.victoriametrics.com/victoriametrics/vmagent/)
next to it: vmagent scrapes sito's [`/metrics`](../troubleshooting/#4-graph-it-over-time) locally and remote-writes them
to VictoriaMetrics, Prometheus or any store that accepts remote write. While the store is unreachable, samples queue
on disk and are sent with their original timestamps once it is back, so a laptop away from home leaves no gaps.

```nix
{
  services.sito.vmagent = {
    enable = true;
    remoteWriteUrl = "http://metrics.example.ts.net:8428/api/v1/write";
    extraArgs = [ "-remoteWrite.label=host=laptop" ];
  };
}
```

The daemon is `org.nixos.sito-vmagent`, logging to `/var/log/sito-vmagent.log`. Its own status pages listen on
`127.0.0.1:8429` only. The scrape config is checked with `vmagent -dryRun` when the system is built.

sito's series take about 6 MB a day at the default 30 s interval with four upstreams (an estimate, measured on a smaller
setup and scaled), so the default 1GB cap holds months offline. The store must keep data at least as long as the
longest offline stretch: VictoriaMetrics drops samples older than its `-retentionPeriod` on arrival.

NixOS has its own `services.vmagent` module; point a scrape job at sito's listen address there instead.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `enable` | `false` | Run the daemon. |
| `package` | `sito.packages.<system>.sito` | Build to run. |
| `tiers` | `{}` | Named tiers: `{ priority ? 1000; strategy ? null; upstreams.<name> = { priority ? 1000; url; public-keys ? []; }; }`. A `null` strategy leaves it to sito's default, `sequential`. |
| `settings` | `{}` | The TOML config as an attrset: `listen`, `probe-interval-secs`, `probe-timeout-secs`, `max-inflight`, or a raw `tier` list. |
| `manageSubstituters` | `true` | Point `nix.settings.substituters` at sito and trust every configured `public-keys` entry. |
| `extraFallbackSubstituters` | `[]` | Substituters added after sito's own address, only while `manageSubstituters` is on. |
| `logLevel` | `"sito=info"` | `RUST_LOG` filter for the daemon. |
| `vmagent.enable` | `false` | nix-darwin only. Run vmagent to remote-write sito's metrics, buffered on disk while offline. |
| `vmagent.remoteWriteUrl` | none | Remote write endpoint, for example VictoriaMetrics' `/api/v1/write`. |
| `vmagent.scrapeInterval` | `"30s"` | How often vmagent scrapes sito. |
| `vmagent.maxDiskUsage` | `"1GB"` | Cap on the on-disk queue; past it the oldest samples are dropped. |
| `vmagent.dataDir` | `"/var/lib/sito-vmagent"` | Where unsent samples wait. |
| `vmagent.extraArgs` | `[]` | Extra vmagent flags: labels, remote-write credentials. |
| `vmagent.package` | `pkgs.vmagent` | vmagent build to run. |

The NixOS service runs with `DynamicUser`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp` and `NoNewPrivileges`,
starts after `network-online.target`, and restarts 2 s after any exit. The nix-darwin daemon runs with `KeepAlive` and
`RunAtLoad`, and waits for `/nix/store` to be mounted before starting.

Why the module manages the substituter list by default:
[ADR-0007](https://github.com/Zebradil/sito/blob/main/docs/adr/0007-nix-modules.md).
