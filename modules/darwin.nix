# nix-darwin module: runs sito under launchd, on top of the shared
# services.sito options and substituter wiring in common.nix.
{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.sito;
  vm = cfg.vmagent;

  # Same fallback as common.nix: the binary's own default listen address.
  listenAddr = cfg.settings.listen or "127.0.0.1:5001";

  vmagentArgs = [
    (lib.getExe vm.package)
    "-promscrape.config=${scrapeConfig}"
    "-remoteWrite.url=${vm.remoteWriteUrl}"
    "-remoteWrite.tmpDataPath=${vm.dataDir}"
    "-remoteWrite.maxDiskUsagePerURL=${vm.maxDiskUsage}"
    # vmagent listens on every interface by default; its own metrics and
    # target pages are for this machine only.
    "-httpListenAddr=127.0.0.1:8429"
  ]
  ++ vm.extraArgs;

  # Checked with vmagent's -dryRun at build time, so a broken scrape config
  # fails darwin-rebuild instead of crash-looping under launchd.
  scrapeConfig =
    let
      file = (pkgs.formats.yaml { }).generate "sito-scrape.yaml" {
        scrape_configs = [
          {
            job_name = "sito";
            scrape_interval = vm.scrapeInterval;
            static_configs = [{ targets = [ listenAddr ]; }];
          }
        ];
      };
    in
    pkgs.runCommand "sito-scrape-checked.yaml" { } ''
      ${lib.getExe vm.package} -promscrape.config=${file} -dryRun
      ln -s ${file} $out
    '';
in
{
  imports = [ (import ./common.nix { inherit self; }) ];

  options.services.sito.vmagent = {
    enable = lib.mkEnableOption ''
      a vmagent daemon that scrapes sito's /metrics and remote-writes them to
      a Prometheus-compatible store. Samples queue on disk while the store is
      unreachable and are sent with their original timestamps once it is back,
      which suits a laptop that is often away from the network the store is on
    '';

    package = lib.mkPackageOption pkgs "vmagent" { };

    remoteWriteUrl = lib.mkOption {
      type = lib.types.str;
      example = "http://metrics.example.ts.net:8428/api/v1/write";
      description = "Remote write endpoint, such as VictoriaMetrics' `/api/v1/write`.";
    };

    scrapeInterval = lib.mkOption {
      type = lib.types.str;
      default = "30s";
      description = "How often vmagent scrapes sito.";
    };

    maxDiskUsage = lib.mkOption {
      type = lib.types.str;
      default = "1GB";
      description = ''
        Cap on the on-disk queue (`-remoteWrite.maxDiskUsagePerURL`); past it
        the oldest samples are dropped. sito's few dozen series take a few MB
        a day at the default interval, so 1GB covers months offline.
      '';
    };

    dataDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/sito-vmagent";
      description = "Where vmagent keeps samples not yet sent.";
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [
        "-remoteWrite.label=host=laptop"
        "-remoteWrite.basicAuth.username=sito"
        "-remoteWrite.basicAuth.passwordFile=/etc/sito/remote-write-password"
      ];
      description = "Extra vmagent flags, for labels and remote-write credentials among others.";
    };
  };

  config = lib.mkIf cfg.enable {
    launchd.daemons.sito = {
      # `command` (not ProgramArguments) makes nix-darwin prefix
      # `wait4path /nix/store`: at boot launchd can spawn the daemon before
      # the Nix volume is mounted, fail with EX_CONFIG, and never retry.
      command = lib.escapeShellArgs [
        (lib.getExe' cfg.package "sito")
        "--config"
        "${cfg.configFile}"
      ];
      serviceConfig = {
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

    launchd.daemons.sito-vmagent = lib.mkIf vm.enable {
      command = lib.escapeShellArgs vmagentArgs;
      serviceConfig = {
        KeepAlive = true;
        RunAtLoad = true;
        StandardOutPath = "/var/log/sito-vmagent.log";
        StandardErrorPath = "/var/log/sito-vmagent.log";
      };
    };
  };
}
