{
  description = "LNDK: Standalone deamon that connects to LND that implements bolt12 funtionallities";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";

    flake-parts.url = "github:hercules-ci/flake-parts";

    crane.url = "github:ipetkov/crane";

    treefmt-nix.url = "github:numtide/treefmt-nix";

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      flake-parts,
      rust-overlay,
      crane,
      ...
    }:
    flake-parts.lib.mkFlake
      {
        inherit inputs;
        specialArgs = { inherit crane; };
      }
      {
        systems = nixpkgs.lib.systems.flakeExposed;
        imports = [
          inputs.treefmt-nix.flakeModule
          ./nix/pkgs/flake-module.nix
          ./nix/checks/flake-module.nix
          ./nix/shells.nix
          ./nix/treefmt.nix
        ];
        perSystem =
          {
            config,
            pkgs,
            self',
            system,
            ...
          }:
          {
            _module.args.pkgs = import inputs.nixpkgs {
              inherit system;
              overlays = [ (import rust-overlay) ];
            };
            apps = {
              lndk = {
                program = "${self'.packages.lndk}/bin/lndk";
              };
              lndk-cli = {
                program = "${self'.packages.lndk}/bin/lndk-cli";
              };
            };
          };
      };
}
