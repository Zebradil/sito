# nix-darwin module: runs sito under launchd, on top of the shared
# services.sito options and substituter wiring in common.nix.
{ self }:
{ config, lib, ... }:
let
  cfg = config.services.sito;
in
{
  imports = [ (import ./common.nix { inherit self; }) ];

  config = lib.mkIf cfg.enable {
    launchd.daemons.sito = {
      serviceConfig = {
        ProgramArguments = [
          (lib.getExe' cfg.package "sito")
          "--config"
          "${cfg.configFile}"
        ];
        KeepAlive = true;
        RunAtLoad = true;
        EnvironmentVariables = {
          RUST_LOG = cfg.logLevel;
        };
        # Without these launchd drops stderr, and every tracing line —
        # including the upstream-failure warnings sito exists to surface —
        # is lost. journald covers the NixOS module; only darwin needs this.
        StandardOutPath = "/var/log/sito.log";
        StandardErrorPath = "/var/log/sito.log";
      };
    };
  };
}
