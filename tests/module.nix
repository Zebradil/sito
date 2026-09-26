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
  # Compared as files at build time: reading configFile during eval would be
  # import-from-derivation, which `nix flake check --no-build` rejects.
  expected = (pkgs.formats.toml { }).generate "expected.toml" {
    tier = [
      {
        upstream = [
          { url = "http://box.lan:5000"; public-keys = [ ]; }
          { url = "http://work.example:5000"; public-keys = [ ]; }
          { url = "https://remote.example"; public-keys = [ "remote-1:AAAA" ]; }
        ];
      }
      { upstream = [{ url = "https://cache.nixos.org"; public-keys = [ ]; }]; }
    ];
  };
in
assert eval.config.nix.settings.trusted-public-keys == [ "remote-1:AAAA" ];
pkgs.runCommand "sito-module-tiers" { } ''
  diff -u ${expected} ${eval.config.services.sito.configFile}
  touch $out
''
