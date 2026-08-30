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
      mkSito =
        rustPlatform:
        rustPlatform.buildRustPackage {
          pname = "sito";
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
        { system, ... }:
        {
          # cargo test runs in the package's checkPhase.
          build = self.packages.${system}.sito;
        }
      );

      formatter = forAllSystems ({ pkgs, ... }: pkgs.nixpkgs-fmt);
    };
}
