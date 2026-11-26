let
  pkgs = <nixpkgs>;
  system = builtins.currentSystem;
  jobs = import (pkgs + "/nixos/release.nix") {
    nixpkgs = {
      outPath = pkgs;
      revCount = 1;
      shortRev = "abcdefg";
    };
    supportedSystems = [ system ];
  };
in
jobs.initialRamdisk.${system}.drvPath
