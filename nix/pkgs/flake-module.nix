{
  self,
  inputs,
  crane,
  ...
}:
{
  perSystem =
    { system, pkgs, ... }:
    let
      rustPackages = import ./rust.nix { inherit pkgs inputs crane; };
    in
    {
      packages = {
        inherit (rustPackages) rust;
        lndk = rustPackages.rust; # Alias for consistency
        default = rustPackages.rust;
      };
    };
}
