# Eval-time check of the darwin module's vmagent daemon: the launchd command
# carries the flags, and the scrape config targets sito's listen address.
# Building the command's context also runs the module's -dryRun config check.
{ self, pkgs }:
let
  inherit (pkgs) lib;
  eval = lib.evalModules {
    modules = [
      (import ../modules/darwin.nix { inherit self; })
      {
        # Stubs for the nix-darwin options the module writes to.
        options.assertions = lib.mkOption { type = lib.types.listOf lib.types.attrs; };
        options.nix.settings = lib.mkOption { type = lib.types.attrs; };
        options.launchd.daemons = lib.mkOption { type = lib.types.attrsOf lib.types.attrs; };
        config._module.args = { inherit pkgs; };
      }
      {
        services.sito = {
          enable = true;
          settings.listen = "127.0.0.1:5009";
          tiers.public.upstreams.nixos.url = "https://cache.nixos.org";
          vmagent = {
            enable = true;
            remoteWriteUrl = "http://vm.example:8428/api/v1/write";
            extraArgs = [ "-remoteWrite.label=host=test" ];
          };
        };
      }
    ];
  };
  command = eval.config.launchd.daemons.sito-vmagent.command;
in
pkgs.runCommand "sito-darwin-module" { } ''
  for flag in \
    -remoteWrite.url=http://vm.example:8428/api/v1/write \
    -remoteWrite.tmpDataPath=/var/lib/sito-vmagent \
    -remoteWrite.maxDiskUsagePerURL=1GB \
    -httpListenAddr=127.0.0.1:8429 \
    -remoteWrite.label=host=test; do
    grep -qF -- "$flag" <<'CMD' || { echo "missing $flag"; exit 1; }
  ${command}
  CMD
  done
  config=$(grep -o "/nix/store/[^ ']*-sito-scrape-checked.yaml" <<'CMD'
  ${command}
  CMD
  )
  grep -qF 127.0.0.1:5009 "$config"
  touch $out
''
