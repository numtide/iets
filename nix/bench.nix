# All tools and inputs belong to the flake closure. Measurements run outside
# the build sandbox; only correctness smoke checks run as derivations.
{ lib, craneLib, commonArgs, sources
, iets, nixpkgs, runCommand, writeShellApplication, coreutils, nix, valgrind, perf, util-linux, jq }:
let
  lock = builtins.fromTOML (builtins.readFile ../bench/Cargo.lock);
  crate = name: lib.findSingle (p: p.name == name)
    (throw "missing ${name} in Cargo.lock") (throw "multiple ${name} versions") lock.package;
  library = crate "gungraun";
  runnerCrate = crate "gungraun-runner";
  # Build the registry executable from the benchmark's own resolved graph.
  # Dummy sources keep edits to eval.rs out of the runner derivation.
  runnerArgs = args // {
    pname = "gungraun-runner";
    version = runnerCrate.version;
    src = craneLib.mkDummySrc args;
    cargoVendorDir = craneLib.vendorCargoDeps args;
    cargoExtraArgs = "--locked --package iets-bench --package gungraun-runner --bin gungraun-runner --features iets-bench/runner";
    doCheck = false; # upstream's integration tests require its repository
  };
  runner = assert library.version == runnerCrate.version;
    craneLib.buildPackage (runnerArgs // {
      cargoArtifacts = craneLib.buildDepsOnly (runnerArgs // { src = sources.bench; });
      # Cargo may reuse the registry executable from the dependency artifact;
      # install it explicitly instead of relying on fresh compiler messages.
      installPhaseCommand = "install -Dm755 target/release/gungraun-runner $out/bin/gungraun-runner";
      meta = {
        mainProgram = "gungraun-runner";
        license = with lib.licenses; [ mit asl20 ];
      };
    });
  workloads = runCommand "iets-bench-workloads" { } ''
    mkdir -p $out
    cp -r ${../bench/workloads}/. $out/
  '';
  args = {
    pname = "iets-bench";
    version = "0.0.0";
    strictDeps = true;
    src = sources.bench;
    cargoLock = ../bench/Cargo.lock;
    cargoToml = ../bench/Cargo.toml;
    postUnpack = "cd $sourceRoot/bench; sourceRoot=.";
  };
  abArgs = commonArgs // { src = sources.ab; };
  nativeHarness = craneLib.mkCargoDerivation (abArgs // {
    pnameSuffix = "-bench-ab";
    cargoArtifacts = craneLib.buildDepsOnly abArgs;
    buildPhaseCargoCommand = "cargo build --release --locked --example ab";
    installPhaseCommand = "install -Dm755 target/release/examples/ab $out/bin/iets-ab";
    doCheck = false;
    doInstallCargoArtifacts = false;
    meta.license = lib.licenses.lgpl21Plus;
  });
  # Linux >= 6.4 otherwise exposes the tool's vDSO to rustix and crashes.
  # Remove when nixpkgs includes upstream de9c4730516522bbc02b55caf4c4df376dd0f169.
  profilingValgrind = valgrind.overrideAttrs (old: {
    patches = (old.patches or [ ]) ++ [ ./patches/valgrind-client-auxv.patch ];
  });
  profiled = iets.overrideAttrs { dontStrip = true; };
  harness = craneLib.mkCargoDerivation (args // {
    cargoArtifacts = craneLib.buildDepsOnly (args // {
      buildPhaseCargoCommand = "cargo bench --locked --no-run --bench eval";
      doCheck = false;
    });
    nativeBuildInputs = [ jq ];
    buildPhaseCargoCommand = ''
      cargo bench --locked --no-run --bench eval --message-format=json-render-diagnostics > bench-build.json
    '';
    doCheck = false;
    doInstallCargoArtifacts = false;
    # Keep symbols for callgrind_annotate and differential flamegraphs.
    dontStrip = true;
    meta.license = lib.licenses.lgpl21Plus;
    installPhaseCommand = ''
      executable=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "eval" and (.target.kind | index("bench")) and .executable != null) | .executable' bench-build.json)
      install -Dm755 "$executable" $out/bin/iets-callgrind
    '';
  });
  ab = writeShellApplication {
    name = "iets-bench-ab";
    runtimeInputs = [ coreutils nix ];
    text = ''
      export NIXPKGS="''${NIXPKGS:-${nixpkgs}}"
      export NIX_CONF_DIR=/nonexistent NIX_USER_CONF_FILES=
      export IETS_BENCH_WORKLOADS="''${IETS_BENCH_WORKLOADS:-${workloads}}"
      exec ${nativeHarness}/bin/iets-ab "$@"
    '';
  };
  callgrind = writeShellApplication {
    name = "iets-bench-callgrind";
    runtimeInputs = [ runner profilingValgrind perf util-linux ];
    text = ''
      export GUNGRAUN_RUNNER=${lib.getExe runner}
      export GUNGRAUN_VALGRIND_BIN=${lib.getExe profilingValgrind}
      export GUNGRAUN_PERF_BIN=${perf}/bin/perf
      export GUNGRAUN_WORKSPACE_ROOT=${sources.bench}/bench
      export GUNGRAUN_SEPARATE_TARGETS="''${GUNGRAUN_SEPARATE_TARGETS:-true}"
      export GUNGRAUN_HOME="''${GUNGRAUN_HOME:-$PWD/target/gungraun}"
      export IETS_BENCH_BIN="''${IETS_BENCH_BIN:-${lib.getExe' profiled "iets"}}"
      export IETS_BENCH_WORKLOADS="''${IETS_BENCH_WORKLOADS:-${workloads}}"
      exec ${harness}/bin/iets-callgrind --bench "$@"
    '';
  };
in
{
  inherit ab callgrind workloads runner profilingValgrind;
  # Exercise installed paths and Valgrind compatibility. These build-time
  # counts are discarded; comparative measurements run outside derivations.
  smoke = runCommand "iets-bench-smoke" { nativeBuildInputs = [ callgrind ]; } ''
    export HOME=$TMPDIR XDG_CACHE_HOME=$TMPDIR/cache
    iets-bench-callgrind --test
    iets-bench-callgrind --save-summary=json
    touch $out
  '';
}
