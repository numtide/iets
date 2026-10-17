# Crane builds the dependencies as a derivation of their own, from the
# manifests alone, so a source edit rebuilds only the iets crate.
# `extraArgs` is for variants, such as the instrumented build of nix/coverage.nix,
# that need their own dependency build.
# iets runs `git` to read a work tree (docs/explanation/flakes-and-sources.md).
# The wrapper puts a git at the end of PATH, for a caller with none, such as a
# systemd service. `wrapGit` is off for the static build, which is for hosts
# without a /nix/store, and for nix/coverage.nix, where llvm-cov reads bin/iets.
{ lib, stdenv, craneLib, installShellFiles, makeBinaryWrapper, gitMinimal
, commonArgs, sources, extraArgs ? { }, wrapGit ? !stdenv.hostPlatform.isStatic }:
let
  args = commonArgs // { src = sources.bin; } // extraArgs;
in
craneLib.buildPackage (args // {
  cargoArtifacts = craneLib.buildDepsOnly args;
  meta.license = lib.licenses.lgpl21Plus;
  # checks.test runs the suite; the package build stays a build.
  doCheck = false;
  nativeBuildInputs = (args.nativeBuildInputs or [ ]) ++ [ installShellFiles ]
    ++ lib.optional wrapGit makeBinaryWrapper;
  postInstall = ''
    installShellCompletion \
      --bash src/cli/complete/iets.bash \
      --zsh src/cli/complete/iets.zsh \
      --fish src/cli/complete/iets.fish \
      --nushell src/cli/complete/iets.nu
    install -Dm444 src/cli/complete/iets.elv $out/share/elvish/lib/iets.elv
    install -Dm444 src/cli/complete/iets.ps1 $out/share/powershell/completions/iets.ps1
  '' + lib.optionalString wrapGit ''
    wrapProgram $out/bin/iets --suffix PATH : ${lib.makeBinPath [ gitMinimal ]}
  '';
})
