# NixOS module: runs sito under systemd, on top of the shared
# services.sito options and substituter wiring in common.nix.
{ self }:
{ config, lib, ... }:
let
  cfg = config.services.sito;
in
{
  imports = [ (import ./common.nix { inherit self; }) ];

  config = lib.mkIf cfg.enable {
    systemd.services.sito = {
      description = "sito — local Nix substituter proxy";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      environment.RUST_LOG = cfg.logLevel;

      serviceConfig = {
        ExecStart = "${lib.getExe' cfg.package "sito"} --config ${cfg.configFile}";
        DynamicUser = true;
        Restart = "always";
        RestartSec = 2;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
      };
    };
  };
}
