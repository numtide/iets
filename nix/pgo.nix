# Optional experiments, deliberately outside packages/checks: training is a
# build input, whereas performance measurements must run outside derivations.
{
  lib,
  stdenv,
  commonArgs,
  iets,
  nixpkgs,
  rustc,
  runCommand,
  writeShellScript,
  symlinkJoin,
  coreutils,
  diffutils,
}:
let
  llvm = rustc.llvmPackages.llvm;
  bolt = rustc.llvmPackages.bolt;
  target = stdenv.hostPlatform.rust.rustcTarget;
  variant =
    suffix: extraFlags:
    (iets.override {
      commonArgs = commonArgs // {
        pname = "iets-${suffix}";
        cargoExtraArgs = "--locked --bin iets --target ${target}";
        RUSTFLAGS = "-C link-arg=-Wl,--emit-relocs ${extraFlags}";
      };
    }).overrideAttrs
      { dontStrip = true; };
  # Keep relocation settings identical across instrumented/use builds. The
  # explicit target prevents instrumenting build scripts and proc macros.
  baseline = variant "pgo-baseline" "";
  instrumented = variant "pgo-instrumented" "-Cprofile-generate=.";
  training = writeShellScript "iets-profile-training" ''
    set -euo pipefail
    binary=$1
    output=$2
    profilePrefix=''${3-}
    mkdir -p "$output"
    work=$(mktemp -d "$TMPDIR/iets-training.XXXXXX")
    export HOME="$work/home" XDG_CACHE_HOME="$work/cache"
    export NIX_CONF_DIR=/nonexistent NIX_USER_CONF_FILES= IETS_PROGRESS=false
    mkdir -p "$HOME" "$XDG_CACHE_HOME"
    run() {
      name=$1
      shift
      echo "training: $name" >&2
      "$binary" "$@" > "$output/$name.out"
      if [[ -n "$profilePrefix" ]]; then
        # A glob with no match stays literal, so a missing profile fails here.
        for raw in "$profilePrefix"*; do
          if ! test -s "$raw"; then
            echo "training: $name produced a missing or empty profile: $raw" >&2
            exit 1
          fi
          mv "$raw" "$out/raw/''${raw%.*}-$name.''${raw##*.}"
        done
      fi
    }
    for workers in 1 4; do
      run "server-w$workers" eval --impure --strict --no-cache --workers "$workers" \
        -I nixpkgs=${nixpkgs} ${../bench/workloads/nixos-server.nix}
    done
    # Populate IR/tracing caches, then train their distinct execution paths.
    run server-cache-fill eval --impure --strict --workers 1 \
      -I nixpkgs=${nixpkgs} ${../bench/workloads/nixos-server.nix}
    run server-cache-hit eval --impure --strict --workers 1 \
      -I nixpkgs=${nixpkgs} ${../bench/workloads/nixos-server.nix}
    run server-warm-ir eval --impure --strict --no-tracing-cache --workers 1 \
      -I nixpkgs=${nixpkgs} ${../bench/workloads/nixos-server.nix}
    ${lib.concatMapStringsSep "\n"
      (name: ''
        run "${name}" eval --strict --json --no-cache --workers 1 \
          ${../bench/workloads/synthetic + "/${name}.nix"}
      '')
      [
        "calls"
        "strings"
        "attrs"
        "derivations"
      ]
    }
  '';
  profile =
    runCommand "iets-pgo-profile"
      {
        nativeBuildInputs = [
          llvm
          coreutils
          diffutils
        ];
      }
      ''
        mkdir -p $out/raw
        # %m keeps counters from separate images distinct; %p avoids concurrent
        # writers. The directory is private to this derivation's build sandbox.
        export LLVM_PROFILE_FILE="$PWD/llvm-%m-%p.profraw"
        ${training} ${instrumented}/bin/iets $out/results llvm-
        unset LLVM_PROFILE_FILE
        ${training} ${iets}/bin/iets reference
        diff -r reference $out/results
        llvm-profdata merge -failure-mode=any -o $out/merged.profdata $out/raw/*.profraw
        llvm-profdata show $out/merged.profdata > $out/summary.txt
        sha256sum ${instrumented}/bin/iets > $out/instrumented.sha256
      '';
  pgo = variant "pgo" "-Cprofile-use=${profile}/merged.profdata -Cllvm-args=-pgo-warn-missing-function";
  boltInstrumented =
    runCommand "iets-pgo-bolt-instrumented"
      {
        nativeBuildInputs = [ bolt ];
      }
      ''
        mkdir -p $out/bin
        llvm-bolt ${pgo}/bin/iets -o $out/bin/iets -instrument \
          -instrumentation-file=bolt-profile -instrumentation-file-append-pid
      '';
  boltProfile =
    runCommand "iets-pgo-bolt-profile"
      {
        nativeBuildInputs = [
          bolt
          diffutils
        ];
      }
      ''
        mkdir -p $out/raw
        ${training} ${boltInstrumented}/bin/iets $out/results bolt-profile.
        diff -r ${profile}/results $out/results
        merge-fdata $out/raw/* > $out/merged.fdata
        test -s $out/merged.fdata
      '';
  pgoBolt =
    runCommand "iets-pgo-bolt"
      {
        nativeBuildInputs = [ bolt ];
        meta.license = lib.licenses.lgpl21Plus;
      }
      ''
        set -o pipefail
        cp -r ${pgo} $out
        chmod -R u+w $out
        llvm-bolt ${pgo}/bin/iets -o $out/bin/iets -data=${boltProfile}/merged.fdata \
          -reorder-blocks=ext-tsp -reorder-functions=cdsort \
          -split-functions -split-all-cold -split-eh -update-debug-sections -dyno-stats \
          2>&1 | tee $out/bolt.log
      '';
in
{
  inherit
    baseline
    instrumented
    profile
    pgo
    ;
  tools = symlinkJoin {
    name = "iets-pgo-tools";
    paths = [ llvm ] ++ lib.optional stdenv.hostPlatform.isx86_64 bolt;
  };
}
// lib.optionalAttrs stdenv.hostPlatform.isx86_64 {
  inherit boltInstrumented boltProfile pgoBolt;
}
