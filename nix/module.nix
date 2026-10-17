self:
{ config, lib, pkgs, ... }:
let
  packages = self.packages.${pkgs.stdenv.hostPlatform.system};
  cfg = config.iets.classic;
in
{
  options.iets.classic.enable = lib.mkEnableOption
    "the iets classic command names in front of Nix";

  options.iets.classic.nix = lib.mkPackageOption pkgs "nix" {
    example = "nixVersions.latest";
    extraDescription = ''
      The real Nix behind the classic names. It provides the daemon, the
      store tools and the fallback. Set this instead of `nix.package`.
    '';
  };

  options.iets.direnv.enable = lib.mkEnableOption
    "iets's `use flake` (nix/direnvrc) in direnv, in place of nix-direnv's"
    // {
      default = config.programs.direnv.enable;
      defaultText = lib.literalExpression "config.programs.direnv.enable";
    };

  config = {
    environment.systemPackages = [ packages.default ];

    # nixos-rebuild only sees nix.package. NixOS reads pname and version, and
    # the system path takes the manual from extraOutputsToInstall.
    nix.package = lib.mkIf cfg.enable (pkgs.buildEnv {
      name = "iets-classic-nix-${cfg.nix.version}";
      paths = [
        (lib.hiPrio (self.lib.mkClassic pkgs packages.default cfg.nix))
        cfg.nix
      ];
      inherit (config.environment) extraOutputsToInstall;
      passthru = { inherit (cfg.nix) pname version; };
      meta.mainProgram = "nix";
    });

    # direnv itself stays the host's choice. direnvrcExtra is sourced after
    # nix-direnv, so iets's use_flake wins even where a host keeps nix-direnv.
    programs.direnv = lib.mkIf config.iets.direnv.enable {
      nix-direnv.enable = lib.mkDefault false;
      direnvrcExtra = builtins.readFile ./direnvrc;
    };

    warnings = lib.optional (config.iets.direnv.enable && !config.programs.direnv.enable)
     "iets.direnv.enable does nothing while programs.direnv.enable is false.";
  };
}
