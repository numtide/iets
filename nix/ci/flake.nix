{
  # Private to this repo: keeps CI-only inputs out of the root flake.lock, so
  # consumers of iets do not lock or fetch them. Loaded via flake-compat.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  # the reference stays on its own nixpkgs, where nix is 2.34.8
  inputs.nix-conformance.url = "github:numtide/nix-conformance";
  # property-based differential tests of Nix evaluators (nix/pbt.nix)
  inputs.nix-pbt.url = "github:yorickvP/nix-pbt";
  inputs.nix-pbt.flake = false;
  inputs.hercules-ci-effects.url = "github:hercules-ci/hercules-ci-effects";
  inputs.hercules-ci-effects.inputs.nixpkgs.follows = "nixpkgs";
  outputs =
    { nix-conformance, nix-pbt, hercules-ci-effects, ... }:
    { inherit nix-conformance nix-pbt hercules-ci-effects; };
}
