self:
{ config, lib, pkgs, ... }:
let
  packages = self.packages.${pkgs.stdenv.hostPlatform.system};
in
{
  options.iets.classic.enable = lib.mkEnableOption
    "the iets classic command names ahead of nix in the system path";

  options.iets.direnv.enable = lib.mkEnableOption
    "direnv with iets's `use flake` (nix/direnvrc) in place of nix-direnv's";

  config = {
    environment.systemPackages = [ packages.default ]
      # buildEnv resolves a collision with nix's own bin/nix-instantiate by
      # meta.priority, lower wins, so hiPrio (-10) is what puts the farm's
      # symlinks in system-path instead. The farm links the one iets this
      # flake ships, never a rebuild against the host's pkgs, and is built for this host's
      # nix: the loser of the collision leaves the system path, and the
      # fallback finds it through the farm's own marker, with no environment.
      ++ lib.optional config.iets.classic.enable
        (lib.hiPrio (self.lib.mkClassic pkgs packages.default config.nix.package));

    programs.direnv = lib.mkIf config.iets.direnv.enable {
      enable = true;
      nix-direnv.enable = false;
      direnvrcExtra = builtins.readFile ./direnvrc;
    };
  };
}
