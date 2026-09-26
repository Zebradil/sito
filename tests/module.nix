# Eval-time check of the module's `tiers` rendering: two modules contribute to
# one named tier, and the result must come out as the binary's ordered lists.
{ self, pkgs }:
let
  inherit (pkgs) lib;
  eval = lib.evalModules {
    modules = [
      (import ../modules/common.nix { inherit self; })
      {
        # Stubs for the NixOS/nix-darwin options common.nix writes to.
        options.assertions = lib.mkOption { type = lib.types.listOf lib.types.attrs; };
        options.nix.settings = lib.mkOption { type = lib.types.attrs; };
        config._module.args = { inherit pkgs; };
      }
      {
        services.sito = {
          enable = true;
          tiers.lan = {
            priority = 10;
            upstreams.box.url = "http://box.lan:5000";
            upstreams.remote = {
              priority = 2000;
              url = "https://remote.example";
              public-keys = [ "remote-1:AAAA" ];
            };
          };
          tiers.public.upstreams.nixos.url = "https://cache.nixos.org";
        };
      }
      { services.sito.tiers.lan.upstreams.work.url = "http://work.example:5000"; }
    ];
  };
  toml = lib.importTOML eval.config.services.sito.configFile;
  urls = map (tier: map (upstream: upstream.url) tier.upstream) toml.tier;
  expected = [
    [
      "http://box.lan:5000"
      "http://work.example:5000"
      "https://remote.example"
    ]
    [ "https://cache.nixos.org" ]
  ];
in
assert lib.assertMsg (urls == expected) "tiers rendered as ${builtins.toJSON urls}";
assert eval.config.nix.settings.trusted-public-keys == [ "remote-1:AAAA" ];
pkgs.runCommand "sito-module-tiers" { } "touch $out"
