{
  pkgs,
  inputs,
  crane,
  ...
}:

let
  inherit (pkgs) lib stdenv;
  src = ../../.;
  rustToolchainFile = builtins.fromTOML (builtins.readFile ../../rust-toolchain.toml);
  rustChannel = rustToolchainFile.toolchain.channel;
  craneLib = (crane.mkLib pkgs).overrideToolchain (pkgs.rust-bin.stable.${rustChannel}.default);
  commonDeps = {
    nativeBuildInputs = with pkgs; [
      pkg-config
      protobuf
    ];

    buildInputs = with pkgs; [
      openssl
    ];
  };

  cargoArtifacts = craneLib.buildDepsOnly {
    inherit src;
    pname = "lndk-deps";
    inherit (commonDeps) nativeBuildInputs buildInputs;
  };

  basePkg = {
    inherit src cargoArtifacts;
    inherit (commonDeps) nativeBuildInputs buildInputs;

    meta = with lib; {
      description = "Standalone daemon that connects to LND to implement bolt12 functionalities";
      homepage = "https://github.com/lndk-org/lndk";
      license = licenses.mit;
      platforms = platforms.linux ++ platforms.darwin;
    };
  };

  lndkPkg = craneLib.buildPackage (
    basePkg
    // {
      pname = "lndk";

in
{
  rust = lndkPkg;
}
