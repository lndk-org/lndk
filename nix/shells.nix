{ self, crane, ... }:
{
  perSystem =
    {
      config,
      pkgs,
      system,
      ...
    }:
    let
      rustToolchainFile = builtins.fromTOML (builtins.readFile ../rust-toolchain.toml);
      rustChannel = rustToolchainFile.toolchain.channel;
      bitcoinPkgs = import ./bitcoin.nix { inherit system pkgs; };
    in
    {
      devShells = {
        default = pkgs.mkShell {
          packages = [
            pkgs.rust-bin.stable.${rustChannel}.rust-analyzer
            bitcoinPkgs.bitcoind
            pkgs.docker
          ];

          nativeBuildInputs = with pkgs; [
            pkg-config
            protobuf
            openssl
            openssl.dev
            rust-bin.stable.${rustChannel}.default
            go
            git
          ];

          shellHook = ''
            echo "LNDK development environment loaded with Rust toolchain $(rustc --version)"
            export BITCOIND_EXE="${bitcoinPkgs.bitcoind}/bin/bitcoind"
            export BITCOIND_SKIP_DOWNLOAD="TRUE"
          '';
        };
      };
    };
}
