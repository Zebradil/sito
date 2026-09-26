# Shared services.sito option surface and config-generation logic for the
# NixOS and darwin modules. Platform-specific modules `imports` this for the
# options plus the substituter wiring, then add their own service/daemon
# definition on top (systemd vs launchd differ too much to share).
{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.sito;
  settingsFormat = pkgs.formats.toml { };

  # Named entries sorted into the binary's ordered lists: priority first, the
  # attribute name breaking ties so the order never depends on definition order.
  byPriority =
    attrs:
    map (name: attrs.${name}) (
      lib.sort
        (
          a: b:
          attrs.${a}.priority < attrs.${b}.priority
          || (attrs.${a}.priority == attrs.${b}.priority && a < b)
        )
        (lib.attrNames attrs)
    );

  renderedTiers = map
    (
      tier:
      lib.optionalAttrs (tier.strategy != null) { inherit (tier) strategy; }
      // {
        upstream = map (upstream: { inherit (upstream) url public-keys; }) (byPriority tier.upstreams);
      }
    )
    (byPriority cfg.tiers);

  effectiveSettings =
    cfg.settings // lib.optionalAttrs (cfg.tiers != { }) { tier = renderedTiers; };

  # Every upstream's public-keys, across every tier, flattened and deduped —
  # the file already carries these (pass-through trust, ADR "language"), so
  # the module reuses them instead of asking for a second copy.
  allPublicKeys = lib.unique (
    lib.flatten (
      map (tier: map (upstream: upstream.public-keys or [ ]) (tier.upstream or [ ])) (
        effectiveSettings.tier or [ ]
      )
    )
  );

  priorityOption = lib.mkOption {
    type = lib.types.int;
    default = 1000;
    description = "Sort key; lower comes first. Ties fall back to the attribute name.";
  };

  # Mirrors the binary's own serde default (src/config.rs): the module
  # doesn't need `settings` to carry `listen` for the daemon to work, only to
  # derive its own substituter URL when the user leaves it unset.
  listenAddr = cfg.settings.listen or "127.0.0.1:5001";
in
{
  options.services.sito = {
    enable = lib.mkEnableOption "sito, a local always-on Nix substituter proxy";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.system}.sito;
      defaultText = lib.literalExpression "sito.packages.<system>.sito";
      description = "The sito package to run.";
    };

    settings = lib.mkOption {
      type = settingsFormat.type;
      default = { };
      description = ''
        sito's config, as an attrset matching the TOML file's kebab-case
        shape directly: `listen`, `probe-interval-secs`,
        `probe-timeout-secs`, `max-inflight`, and repeated `tier` entries
        each with a `strategy` and repeated `upstream` (`url`,
        `public-keys`). See the sito README for the full shape — every field
        is expressible here, so there is no separate option per tier/upstream
        knob.
      '';
      example = {
        listen = "127.0.0.1:5001";
        tier = [
          {
            strategy = "sequential";
            upstream = [
              {
                url = "http://box.lan:5000";
                public-keys = [ "znix.zebradil.dev:AAAA..." ];
              }
              {
                url = "https://cache.nixos.org";
                public-keys = [ "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=" ];
              }
            ];
          }
        ];
      };
    };

    tiers = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.submodule {
          options = {
            priority = priorityOption;
            strategy = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "Tier strategy; null leaves it to the binary's default (`sequential`).";
            };
            upstreams = lib.mkOption {
              type = lib.types.attrsOf (
                lib.types.submodule {
                  options = {
                    priority = priorityOption;
                    url = lib.mkOption {
                      type = lib.types.str;
                      description = "Base URL of the binary cache.";
                    };
                    public-keys = lib.mkOption {
                      type = lib.types.listOf lib.types.str;
                      default = [ ];
                      description = "Signing keys this upstream's narinfos carry.";
                    };
                  };
                }
              );
              default = { };
              description = "Upstreams in this tier, keyed by name.";
            };
          };
        }
      );
      default = { };
      description = ''
        Tiers keyed by name, rendered into `settings.tier` sorted by
        `priority`. Unlike the raw list, named tiers and upstreams merge
        across modules, so a host can add an upstream to a tier another
        module defines. Mutually exclusive with `settings.tier`.
      '';
      example = {
        lan = {
          priority = 10;
          upstreams.box.url = "http://box.lan:5000";
        };
        public.upstreams.nixos = {
          url = "https://cache.nixos.org";
          public-keys = [ "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=" ];
        };
      };
    };

    manageSubstituters = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Point `nix.settings.substituters` at sito's own listen address and
        trust every `public-keys` entry configured across `settings.tier`.
        Enabling sito without wiring nix to it would be a surprising
        default; set this to false to curate substituters/trusted-public-keys
        by hand instead.
      '';
    };

    extraFallbackSubstituters = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = ''
        Extra substituters appended after sito's own endpoint, only while
        `manageSubstituters` is true. Not needed for the common case: sito
        already queries every configured upstream itself, and a dead sito
        answers connection-refused instantly so nix falls back to building
        without a timeout tax. For the cautious.
      '';
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "sito=info";
      description = "RUST_LOG filter passed to the daemon.";
    };

    configFile = lib.mkOption {
      type = lib.types.path;
      internal = true;
      readOnly = true;
      description = "Generated TOML config file, consumed by the platform service definition.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.tiers == { } || !(cfg.settings ? tier);
        message = "services.sito: set either `tiers` or `settings.tier`, not both.";
      }
    ];

    services.sito.configFile = settingsFormat.generate "sito.toml" effectiveSettings;

    nix.settings = lib.mkIf cfg.manageSubstituters {
      substituters = [ "http://${listenAddr}" ] ++ cfg.extraFallbackSubstituters;
      trusted-public-keys = allPublicKeys;
    };
  };
}
