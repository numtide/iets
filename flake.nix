{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.crane.url = "github:ipetkov/crane";
  # the reference stays on its own nixpkgs, where nix is 2.34.8
  inputs.nix-conformance.url = "github:numtide/nix-conformance";
  outputs = { self, nixpkgs, crane, nix-conformance }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      # Every file in nix/ is a function of what it needs; callPackage fills
      # it from this scope or from nixpkgs.
      scopeFor = pkgs: pkgs.lib.makeScope pkgs.newScope (final: {
        inherit pkgs nixpkgs nix-conformance;
        craneSrc = crane;
        craneLib = crane.mkLib pkgs;
        commonArgs = { pname = "iets"; version = "0.1.0"; strictDeps = true; };
        sources = final.callPackage ./nix/sources.nix { };
        iets = final.callPackage ./nix/iets.nix { };
        ietsStatic = (scopeFor pkgs.pkgsStatic).iets;
        classic = final.callPackage ./nix/classic.nix { };
        devShell = final.callPackage
          ({ mkShell, cargo, rustc, clippy, rustfmt, hyperfine, nix, perf, jq, nushell, direnv, ast-grep }:
            mkShell { packages = [ cargo rustc clippy rustfmt hyperfine nix perf jq nushell direnv ast-grep ]; })
          { };
        e2eCases = import ./nix/e2e-cases.nix { inherit pkgs; };
        e2eTest = final.callPackage ./nix/e2e-test.nix { };
        coverage = final.callPackage ./nix/coverage.nix { };
        # the differential fuzzer against Nix (docs/how-to/fuzz.md)
        fuzz = final.callPackage ./nix/fuzz.nix { };
        # for files that return a set of packages
        callPackages = pkgs.lib.callPackagesWith (pkgs // final);
      });
      forAll = f: nixpkgs.lib.genAttrs systems (s: f (scopeFor nixpkgs.legacyPackages.${s}));
    in {
      devShells = forAll (scope: { default = scope.devShell; });
      nixosModules.default = import ./nix/module.nix self;
      lib.mkClassic = pkgs: iets: nix: pkgs.callPackage ./nix/classic.nix { inherit iets nix; };
      packages = forAll (scope: scope.callPackages ./nix/packages.nix { });
      checks = forAll (scope: scope.callPackages ./nix/checks.nix { });
    };
}
