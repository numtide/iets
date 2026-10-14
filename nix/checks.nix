{ lib, stdenv, runCommand, nushell, ast-grep, craneLib, commonArgs, sources, iets, ietsStatic, ietsRiscv64, fuzz, e2eTest, e2eCases }:
let
  # the dev profile, as `cargo test` in the dev shell: the release profile is
  # fat LTO, minutes for no extra coverage
  dev = commonArgs // { src = sources.test; CARGO_PROFILE = "dev"; };
  withDev = dev // {
    cargoArtifacts = craneLib.buildDepsOnly (dev // { cargoCheckExtraArgs = "--all-targets"; });
    doInstallCargoArtifacts = false;
  };
  # scripts/<name>.nu against the built binary, in the build sandbox: the
  # scripts that need no nix-daemon and no live /nix/store.
  nuScript = name: runCommand "iets-${name}" { nativeBuildInputs = [ nushell ]; } ''
    cp -r ${sources.scripts}/scripts ${sources.scripts}/tests .
    chmod -R u+w scripts tests
    export IETS=${iets}/bin/iets
    nu scripts/${name}.nu
    touch $out
  '';
in
{
  # the tests that need a live /nix/store are #[ignore]d; run them
  # with `cargo test -- --ignored` in the dev shell.
  test = craneLib.cargoNextest withDev;
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
  linttest = nuScript "linttest";
  # VERSION is what the release effect publishes; cargo cannot read it
  version = let cargo = (lib.importTOML ../Cargo.toml).package.version; in
    runCommand "iets-version" { } (if cargo == commonArgs.version then "touch $out" else ''
      echo "VERSION is ${commonArgs.version} but Cargo.toml is ${cargo}" >&2
      exit 1
    '');
  static = ietsStatic;
}
# catches a missing per-arch cfg
// lib.optionalAttrs (stdenv.hostPlatform.system == "x86_64-linux") {
  cross-riscv64 = ietsRiscv64;
}
# the differential fuzzers against Nix (nix/fuzz.nix)
// fuzz.checks
# The scripts that need a nix-daemon and a writable store, in a NixOS test
# container (nix/e2e-test.nix). Their derivations name x86_64-linux.
// lib.optionalAttrs (stdenv.hostPlatform.system == "x86_64-linux")
  (lib.mapAttrs (name: args: e2eTest ({ inherit name iets; } // args)) e2eCases)
