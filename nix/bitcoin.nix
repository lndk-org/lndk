{ system, pkgs }:
let
  pkgs_bitcoind = import (builtins.fetchTarball {
    url = "https://github.com/NixOS/nixpkgs/archive/21808d22b1cda1898b71cf1a1beb524a97add2c4.tar.gz";
    # Bitcoin Core 28.1
    sha256 = "0v2z6jphhbk1ik7fqhlfnihcyff5np9wb3pv19j9qb9mpildx0cg";
  }) { inherit system; };
in
{
  bitcoind = pkgs_bitcoind.bitcoind;
}
