{
  description = "sito — local Nix substituter proxy";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.crane.url = "github:ipetkov/crane";

  outputs =
    { self
    , nixpkgs
    , crane
    ,
    }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-darwin"
      ];
      forAllSystems =
        f:
        lib.genAttrs systems (
          system:
          f {
            inherit system;
            pkgs = nixpkgs.legacyPackages.${system};
          }
        );
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./src
          ./tests/e2e.rs
        ];
      };
      # Dependencies build in their own derivation from crane's dummy source.
      # crane keeps sito's own version in that source, so a release bump would
      # still rebuild every dependency; it is stripped from the dummy Cargo.toml
      # and Cargo.lock (both then read 0.0.0) so only sito recompiles. Cargo.lock
      # is left out of `depsSrc` because crane copies it from there verbatim.
      version = (lib.importTOML ./Cargo.toml).package.version;
      lockVersionLine = v: "name = \"sito\"\nversion = \"${v}\"\n";
      lock = builtins.readFile ./Cargo.lock;
      depsLock = builtins.replaceStrings [ (lockVersionLine version) ] [ (lockVersionLine "0.0.0") ] lock;
      depsSrc = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./src
          ./tests/e2e.rs
        ];
      };
      mkCrane =
        pkgs:
        let
          craneLib = crane.mkLib pkgs;
          cargoVendorDir = craneLib.vendorCargoDeps { inherit src; };
          cargoArtifacts =
            assert lib.assertMsg (depsLock != lock) "sito's version not found in Cargo.lock";
            craneLib.buildDepsOnly {
              pname = "sito";
              version = "0.0.0";
              inherit cargoVendorDir;
              dummySrc = craneLib.mkDummySrc {
                src = depsSrc;
                cargoLock = builtins.toFile "Cargo.lock" depsLock;
                cleanCargoTomlFilter =
                  path: craneLib.filters.cargoTomlDefault path && path != [ "package" "version" ];
              };
            };
        in
        {
          inherit craneLib cargoArtifacts;
          sito = craneLib.buildPackage { inherit src cargoArtifacts cargoVendorDir; };
        };
    in
    {
      nixosModules = {
        sito = import ./modules/nixos.nix { inherit self; };
        default = self.nixosModules.sito;
      };

      darwinModules = {
        sito = import ./modules/darwin.nix { inherit self; };
        default = self.darwinModules.sito;
      };

      packages = forAllSystems (
        { system, pkgs }:
        {
          sito = (mkCrane pkgs).sito;
          default = self.packages.${system}.sito;
        }
      );

      devShells = forAllSystems (
        { pkgs, ... }:
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.rust-analyzer
            ];
          };
        }
      );

      checks = forAllSystems (
        { system, pkgs }:
        let
          inherit (mkCrane pkgs) craneLib cargoArtifacts;
        in
        {
          # cargo test runs in the package's checkPhase.
          build = self.packages.${system}.sito;
          fmt = craneLib.cargoFmt { inherit src; };
          clippy = craneLib.cargoClippy {
            inherit src cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets --all-features -- -D warnings";
          };
          # CI pushes only the output closures of these checks, and the dependency
          # build is a build-time input of them, so without this it never reaches
          # the cache and every version bump recompiles it.
          build-inputs = pkgs.linkFarm "sito-build-inputs" {
            deps = cargoArtifacts;
            crane-utils = craneLib.craneUtils;
          };
          module-tiers = import ./tests/module.nix { inherit self pkgs; };
          module-darwin = import ./tests/darwin-module.nix { inherit self pkgs; };
        }
      );

      formatter = forAllSystems ({ pkgs, ... }: pkgs.nixpkgs-fmt);
    };
}
