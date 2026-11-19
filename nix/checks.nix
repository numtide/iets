{ lib, pkgs, stdenv, runCommand, git, jq, nix, openssh, util-linux, nix-eval-jobs, path, ast-grep, craneLib, commonArgs, sources, ietsRiscv64, fuzz }:
let
  # the dev profile, as `cargo test` in the dev shell: the release profile is
  # LTO, minutes for no extra coverage
  dev = commonArgs // { src = sources.test; CARGO_PROFILE = "dev"; };
  withDev = dev // {
    cargoArtifacts = craneLib.buildDepsOnly (dev // { cargoCheckExtraArgs = "--all-targets"; });
    doInstallCargoArtifacts = false;
  };
in
{
  test = craneLib.cargoNextest (withDev // {
    # nix runs the private daemon of tests/e2e
    # ssh-keygen checks the signed commits of the fetchGit tests
    nativeCheckInputs = [ git nix openssh ] ++ lib.optional stdenv.hostPlatform.isLinux util-linux;
    # no e2e test may skip for want of nix-daemon
    IETS_TEST_E2E = "1";
    # nextest runs no example's tests by default, and examples/ab.rs has some
    cargoNextestExtraArgs = "--lib --bins --tests --examples";
    # cargo would create /homeless-shelter, and then that daemon refuses to
    # build without a sandbox
    postUnpack = "export HOME=$(mktemp -d)";
    IETS_TEST_NIXPKGS = "${path}";
    IETS_TEST_NIX_EVAL_JOBS = lib.getExe nix-eval-jobs;
  } // lib.optionalAttrs stdenv.hostPlatform.isLinux {
    IETS_TEST_NIX_GRPC_DAEMON = "${import ./nix-grpc-store.nix { inherit pkgs; }}/bin/nix-grpc-daemon";
  });
  # every nixpkgs drv against nix-eval-jobs (tests/nixpkgs_eval.rs), with
  # optimized code to keep it short. Nix's side depends on nixpkgs alone, so
  # it is a derivation of its own that a change to iets does not rebuild.
  nixpkgs-eval =
    let
      args = commonArgs // { src = sources.test; };
    in
    craneLib.cargoNextest (args // {
      cargoArtifacts = craneLib.buildDepsOnly (args // { cargoCheckExtraArgs = "--all-targets"; });
      doInstallCargoArtifacts = false;
      cargoNextestExtraArgs = "--test nixpkgs_eval --run-ignored only";
      IETS_TEST_NIXPKGS = "${path}";
      IETS_TEST_NIX_EVAL_JOBS = lib.getExe nix-eval-jobs;
      # the test reads attr, drvPath and whether there is an error: the rest
      # is hundreds of MB
      IETS_TEST_NIXPKGS_NIX = runCommand "nixpkgs-eval-nix" { nativeBuildInputs = [ nix-eval-jobs jq ]; } ''
        export HOME=$(mktemp -d) NIX_CONF_DIR=/nonexistent NIX_USER_CONF_FILES= XDG_CACHE_HOME=$(mktemp -d)
        nix-eval-jobs --store 'dummy://?read-only=false' --impure --no-instantiate --workers 4 \
          --arg nixpkgs ${path} --arg limit null ${../tests/nixpkgs_eval.nix} \
          | jq -c 'if has("error") then {attr, error} else {attr, drvPath} end' > $out
      '';
    });
  # nextest runs no doctests
  doctest = craneLib.cargoDocTest withDev;
  doc = craneLib.cargoDoc (withDev // { RUSTDOCFLAGS = "-D warnings"; });
  clippy = craneLib.cargoClippy (withDev // { cargoClippyExtraArgs = "--all-targets -- -D warnings"; });
  # the library facade: every example links the library only (docs/reference/library.md)
  examples = craneLib.mkCargoDerivation (withDev // {
    pnameSuffix = "-examples";
    buildPhaseCargoCommand = "cargoWithProfile build --examples";
    installPhaseCommand = "touch $out";
  });
  fmt = craneLib.cargoFmt (commonArgs // { src = sources.test; });
  # nix/ast-grep, through sgconfig.yml
  ast-grep = runCommand "iets-ast-grep" { nativeBuildInputs = [ ast-grep ]; } "cd ${sources.astGrep} && ast-grep scan && touch $out";
  # VERSION is what the release effect publishes; cargo cannot read it
  version = let cargo = (lib.importTOML ../Cargo.toml).package.version; in
    runCommand "iets-version" { } (if cargo == commonArgs.version then "touch $out" else ''
      echo "VERSION is ${commonArgs.version} but Cargo.toml is ${cargo}" >&2
      exit 1
    '');
}
# catches a missing per-arch cfg
// lib.optionalAttrs (stdenv.hostPlatform.system == "x86_64-linux") {
  cross-riscv64 = ietsRiscv64;
}
# the differential fuzzers against Nix (nix/fuzz.nix)
// fuzz.checks
