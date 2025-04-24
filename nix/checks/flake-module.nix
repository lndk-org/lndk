{
  inputs,
  self,
  crane,
  ...
}:
{
  perSystem =
    {
      pkgs,
      config,
      system,
      ...
    }:
    let
      rustToolchainFile = builtins.fromTOML (builtins.readFile ../../rust-toolchain.toml);
      rustChannel = rustToolchainFile.toolchain.channel;
      craneLib = (crane.mkLib pkgs).overrideToolchain (pkgs.rust-bin.stable.${rustChannel}.default);
      advisory-db = inputs.advisory-db;
    in
    {
      checks = {
        lndk = config.packages.rust;
        cargo-audit = craneLib.cargoAudit {
          src = ../../.;
          inherit advisory-db;
        };
        formatting = config.treefmt.build.check self;
      };
    };
}
