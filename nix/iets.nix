# Crane builds the dependencies as a derivation of their own, from the
# manifests alone, so a source edit rebuilds only the iets crate.
# iets runs `git` to read a work tree (docs/explanation/flakes-and-sources.md).
# `IETS_FALLBACK_PATH` names a git for a caller with none on PATH, such as a
# systemd service (sys::program). It is off for the static build, which is for
# hosts without a /nix/store. Nixpkgs calls WebAssembly static too.
{ lib, stdenv, craneLib, installShellFiles, gitMinimal
, commonArgs, sources, fallbackGit ? !stdenv.hostPlatform.isStatic }:
let
  inherit (stdenv.hostPlatform) isWasi;
  args = commonArgs // { src = sources.bin; } // lib.optionalAttrs isWasi {
    # docs/explanation/webassembly.md explains the stack. Debug info is most of
    # the file, and a browser downloads the file.
    RUSTFLAGS = lib.concatStringsSep " " [
      "-Clinker-flavor=gcc"
      "-Clink-self-contained=no"
      "-Clink-arg=--target=${stdenv.hostPlatform.config}"
      "-Clink-arg=-Wl,-z,stack-size=8388608"
      "-Clink-arg=-Wl,--stack-first"
    ];
    CARGO_PROFILE_RELEASE_DEBUG = "0";
    CARGO_PROFILE_RELEASE_STRIP = "true";
  };
in
craneLib.buildPackage (args // {
  cargoArtifacts = craneLib.buildDepsOnly args;
  meta.license = lib.licenses.lgpl21Plus;
  meta.mainProgram = "iets";
  # checks.test runs the suite; the package build stays a build.
  doCheck = false;
  nativeBuildInputs = (args.nativeBuildInputs or [ ]) ++ [ installShellFiles ];
  postInstall = lib.optionalString (!isWasi) ''
    installShellCompletion \
      --bash src/cli/complete/iets.bash \
      --zsh src/cli/complete/iets.zsh \
      --fish src/cli/complete/iets.fish \
      --nushell src/cli/complete/iets.nu
    install -Dm444 src/cli/complete/iets.elv $out/share/elvish/lib/iets.elv
    install -Dm444 src/cli/complete/iets.ps1 $out/share/powershell/completions/iets.ps1
  '';
} // lib.optionalAttrs fallbackGit {
  IETS_FALLBACK_PATH = lib.makeBinPath [ gitMinimal ];
})
