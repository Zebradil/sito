{
  description = "sito — local Nix substituter proxy";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
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
      # Shared by the package build and the lint checks below, so both draw the
      # same vendored, network-free cargo registry from one place.
      commonCargoArgs = {
        version = (lib.importTOML ./Cargo.toml).package.version;
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./src
          ];
        };
        cargoLock.lockFile = ./Cargo.lock;
      };
      mkSito = rustPlatform: rustPlatform.buildRustPackage (commonCargoArgs // { pname = "sito"; });
      # A lint check as a buildRustPackage derivation, not a plain runCommand: that's
      # what gets the vendored offline registry cargoSetupHook already sets up for the
      # package build, for free, instead of duplicating the vendoring by hand.
      mkCargoLintCheck =
        pkgs: name: command:
        pkgs.rustPlatform.buildRustPackage (
          commonCargoArgs
          // {
            pname = "sito-${name}";
            nativeBuildInputs = [
              pkgs.clippy
              pkgs.rustfmt
            ];
            buildPhase = command;
            doCheck = false;
            installPhase = "mkdir -p $out";
          }
        );
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
          sito = mkSito pkgs.rustPlatform;
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
        {
          # cargo test runs in the package's checkPhase.
          build = self.packages.${system}.sito;
          fmt = mkCargoLintCheck pkgs "fmt" "cargo fmt --all -- --check";
          clippy = mkCargoLintCheck pkgs "clippy" "cargo clippy --all-targets --all-features -- -D warnings";
        }
      );

      formatter = forAllSystems ({ pkgs, ... }: pkgs.nixpkgs-fmt);
    };
}
