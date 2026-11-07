{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.crane.url = "github:ipetkov/crane";
  outputs = { self, nixpkgs, crane }:
    let
      # CI-only inputs live in a private flake with its own lock, so they stay
      # out of this flake.lock and out of the lock of anyone using iets.
      ci = (import ./nix/flake-compat.nix { src = ./nix/ci; }).defaultNix;
      inherit (ci) nix-conformance hercules-ci-effects;
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
        commonArgs = { pname = "iets"; inherit version; strictDeps = true; };
        sources = final.callPackage ./nix/sources.nix { };
        iets = final.callPackage ./nix/iets.nix { };
        ietsStatic = (scopeFor pkgs.pkgsStatic).iets;
        # only a check builds it: the dev profile, as a missing cfg fails in any
        # profile and LTO is the slowest compile of the suite
        ietsRiscv64 = (scopeFor pkgs.pkgsCross.riscv64).iets.override (o: {
          commonArgs = o.commonArgs // { CARGO_PROFILE = "dev"; };
        });
        # runs in a browser (docs/explanation/webassembly.md)
        ietsWasi = (scopeFor pkgs.pkgsCross.wasi32).iets;
        classic = final.callPackage ./nix/classic.nix { };
        devShell = final.callPackage
          ({ lib, stdenv, mkShell, cargo, rustc, rustPlatform, clippy, rustfmt, hyperfine, nix, perf, jq, nushell, direnv, ast-grep }:
            mkShell {
              packages = [ cargo rustc clippy rustfmt hyperfine nix jq nushell direnv ast-grep ]
                ++ lib.optional stdenv.hostPlatform.isLinux perf;
              RUST_SRC_PATH = "${rustPlatform.rustLibSrc}";
            })
          { };
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
      # every package and dev shell, so that CI pushes them to the cache; the
      # fuzz campaigns run for an hour each
      checks = lib.mapAttrs (system: scope:
        lib.mapAttrs' (n: lib.nameValuePair "package-${n}")
          (lib.filterAttrs (n: _: !lib.hasSuffix "-campaign" n) self.packages.${system})
        // lib.mapAttrs' (n: lib.nameValuePair "devShell-${n}") self.devShells.${system}
        // onLinux scope (scope.callPackages ./nix/checks.nix { }) { })
        (forAll lib.id);
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
