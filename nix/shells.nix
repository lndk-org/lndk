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
    in
    {
      devShells = {
        default = pkgs.mkShell {
          packages = [
            pkgs.rust-bin.stable.${rustChannel}.rust-analyzer
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
          '';
        };
      };
    };
}
