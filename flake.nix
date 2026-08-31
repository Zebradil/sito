{
  description = "sito — local Nix substituter proxy";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  # Unused by the outputs: CI reads the locked rev out of flake.lock and runs
  # kasha's `kasha-cache-push` and `kasha emit` from it. Pinning here is what
  # keeps the push script and the manifest format it writes in step
  # (kasha ADR-0009), and lets renovate bump both at once.
  inputs.kasha.url = "github:zebradil/kasha";
  inputs.kasha.inputs.nixpkgs.follows = "nixpkgs";

  outputs =
    { self, nixpkgs, ... }:
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
