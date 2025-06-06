{ ... }:
{
  perSystem =
    { pkgs, lib, ... }:
    {
      treefmt = {
        projectRootFile = "flake.nix";
        programs.nixfmt.enable = true;
        programs.rustfmt.enable = true;
      };
    };
}
