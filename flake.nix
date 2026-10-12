{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.crane.url = "github:ipetkov/crane";
  # the reference stays on its own nixpkgs, where nix is 2.34.8
  inputs.nix-conformance.url = "github:numtide/nix-conformance";
  inputs.hercules-ci-effects.url = "github:hercules-ci/hercules-ci-effects";
  inputs.hercules-ci-effects.inputs.nixpkgs.follows = "nixpkgs";
  outputs = { self, nixpkgs, crane, nix-conformance, hercules-ci-effects }:
    let
      lib = nixpkgs.lib;
      # Cargo.toml must agree: checks.version fails when it does not
      version = lib.fileContents ./VERSION;
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      # Every file in nix/ is a function of what it needs; callPackage fills
      # it from this scope or from nixpkgs.
      scopeFor = pkgs: pkgs.lib.makeScope pkgs.newScope (final: {
        inherit pkgs nixpkgs nix-conformance;
        craneSrc = crane;
        craneLib = crane.mkLib pkgs;
        commonArgs = {
          pname = "iets";
          inherit version;
          strictDeps = true;
          # a fixed-output fetch: the farm evaluator refuses builtins.fetchGit
          outputHashes."git+https://github.com/nix-community/harmonia?rev=c1be8eba70b3d06cd502056719d119c95e36c9d8#c1be8eba70b3d06cd502056719d119c95e36c9d8" =
            "sha256-N30ulmYkeIoFrVmyeioItVFCuI78VW/jL9lD0LCJvYo=";
        };
        sources = final.callPackage ./nix/sources.nix { };
        iets = final.callPackage ./nix/iets.nix { };
        ietsStatic = (scopeFor pkgs.pkgsStatic).iets;
        ietsRiscv64 = (scopeFor pkgs.pkgsCross.riscv64).iets;
        classic = final.callPackage ./nix/classic.nix { };
        devShell = final.callPackage
          ({ lib, stdenv, mkShell, cargo, rustc, clippy, rustfmt, hyperfine, nix, perf, jq, nushell, direnv, ast-grep }:
            mkShell {
              packages = [ cargo rustc clippy rustfmt hyperfine nix jq nushell direnv ast-grep ]
                ++ lib.optional stdenv.hostPlatform.isLinux perf;
            })
          { };
        e2eCases = import ./nix/e2e-cases.nix { inherit pkgs; };
        e2eTest = final.callPackage ./nix/e2e-test.nix { };
        coverage = final.callPackage ./nix/coverage.nix { };
        # the differential fuzzer against Nix (docs/how-to/fuzz.md)
        fuzz = final.callPackage ./nix/fuzz.nix { };
        # for files that return a set of packages
        callPackages = pkgs.lib.callPackagesWith (pkgs // final);
      });
      # macOS gets the dev shell and the package, not the fuzzers or the suite
      onLinux = scope: linux: other:
        if scope.pkgs.stdenv.hostPlatform.isLinux then linux else other;
      forAll = f: nixpkgs.lib.genAttrs systems (s: f (scopeFor nixpkgs.legacyPackages.${s}));
      x86 = nixpkgs.legacyPackages.x86_64-linux;
      release = x86.callPackage ./nix/release.nix {
        inherit version;
        static = lib.genAttrs [ "x86_64-linux" "aarch64-linux" ]
          (s: (scopeFor nixpkgs.legacyPackages.${s}).ietsStatic);
      };
      notes = ./release-notes + "/v${version}.md";
    in {
      devShells = forAll (scope: { default = scope.devShell; });
      nixosModules.default = import ./nix/module.nix self;
      lib.mkClassic = pkgs: iets: nix: pkgs.callPackage ./nix/classic.nix { inherit iets nix; };
      packages = lib.recursiveUpdate
        (forAll (scope: onLinux scope
          (scope.callPackages ./nix/packages.nix { })
          { default = scope.iets; }))
        { x86_64-linux = { inherit release; }; };
      checks = forAll (scope: onLinux scope
        (scope.callPackages ./nix/checks.nix { })
        { build = scope.iets; devShell = scope.devShell; });
      herculesCI = { primaryRepo, ... }: {
        onPush.default.outputs = {
          inherit release;
          effects.release = import ./nix/release-effect.nix {
            pkgs = x86;
            hci-effects = hercules-ci-effects.lib.withPkgs x86;
            inherit version release;
            notes = if builtins.pathExists notes then notes else null;
            inherit (primaryRepo) rev;
            repo = "numtide/iets";
          };
        };
      };
    };
}
