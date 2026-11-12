# Optional experiments, deliberately outside packages/checks: training is a
# build input, whereas performance measurements must run outside derivations.
{ lib, stdenv, commonArgs, iets, benchmarks, nixpkgs, rustc, binutils
, runCommand, writeShellScript, symlinkJoin, coreutils, diffutils }:
let
  llvm = rustc.llvmPackages.llvm;
  bolt = rustc.llvmPackages.bolt;
  compilerRt = rustc.llvmPackages.compiler-rt;
  target = stdenv.hostPlatform.rust.rustcTarget;
  # nixpkgs' external LLVM can differ from Rust's vendored compiler-rt. The
  # instrumented code and runtime must agree on the InstrProf ABI. Keep the
  # exact Rust crate metadata/shim and replace only its native C runtime.
  profilingSysroot = runCommand "iets-profiling-sysroot" {
    nativeBuildInputs = [ binutils ];
  } ''
    mkdir -p $out
    cp -rs ${rustc.unwrapped}/lib $out/lib
    find $out -type d -exec chmod u+w '{}' +
    original=(${rustc.unwrapped}/lib/rustlib/${target}/lib/libprofiler_builtins-*.rlib)
    test "''${#original[@]}" -eq 1
    name=$(basename "''${original[0]}")
    mkdir rust native
    (cd rust; ar x "''${original[0]}")
    find rust -type f ! -name 'lib.rmeta' ! -name 'lib.rmeta-link' ! -name '*.rcgu.o' -delete
    (cd native; ar x ${compilerRt}/lib/linux/libclang_rt.profile-${stdenv.hostPlatform.parsed.cpu.name}.a)
    destination=$out/lib/rustlib/${target}/lib/$name
    rm "$destination"
    ar crsD "$destination" rust/* native/*
  '';
  flags = "--sysroot=${profilingSysroot} -C link-arg=-Wl,--emit-relocs";
  profilerCheck = runCommand "iets-profiler-check" {
    nativeBuildInputs = [ rustc stdenv.cc llvm ];
  } ''
    mkdir -p $out
    cat > minimal.rs <<'EOF'
    fn main() {
        let call: fn() = || { println!("ok"); };
        std::hint::black_box(call)();
    }
    EOF
    rustc ${flags} -O -Clto=thin -Cpanic=abort -Cprofile-generate=. minimal.rs -o minimal
    export LLVM_PROFILE_FILE="$TMPDIR/%m-%p.profraw"
    ./minimal > $out/stdout
    grep -Fx ok $out/stdout
    llvm-profdata merge -failure-mode=any -o $out/merged.profdata ./*.profraw
    llvm-profdata show $out/merged.profdata > $out/summary.txt
  '';
  variant = suffix: extraFlags: (iets.override {
    commonArgs = commonArgs // {
      pname = "iets-${suffix}";
      cargoExtraArgs = "--locked --bin iets --target ${target}";
      RUSTFLAGS = "${flags} ${extraFlags}";
      preBuild = "test -s ${profilerCheck}/merged.profdata";
    };
  }).overrideAttrs { dontStrip = true; };
  # Keep relocation settings identical across instrumented/use builds. The
  # explicit target prevents instrumenting build scripts and proc macros.
  baseline = variant "pgo-baseline" "";
  instrumented = variant "pgo-instrumented" "-Cprofile-generate=.";
  training = writeShellScript "iets-profile-training" ''
    set -euo pipefail
    binary=$1
    output=$2
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
    }
    for workers in 1 4; do
      run "server-w$workers" eval --impure --strict --no-cache --workers "$workers" \
        -I nixpkgs=${nixpkgs} ${benchmarks.workloads}/nixos-server.nix
    done
    # Populate IR/tracing caches, then train their distinct execution paths.
    run server-cache-fill eval --impure --strict --workers 1 \
      -I nixpkgs=${nixpkgs} ${benchmarks.workloads}/nixos-server.nix
    run server-cache-hit eval --impure --strict --workers 1 \
      -I nixpkgs=${nixpkgs} ${benchmarks.workloads}/nixos-server.nix
    run server-warm-ir eval --impure --strict --no-tracing-cache --workers 1 \
      -I nixpkgs=${nixpkgs} ${benchmarks.workloads}/nixos-server.nix
    for workload in calls strings attrs derivations; do
      run "$workload" eval --strict --json --no-cache --workers 1 \
        ${benchmarks.workloads}/synthetic/"$workload".nix
    done
  '';
  profile = runCommand "iets-pgo-profile" {
    nativeBuildInputs = [ llvm coreutils diffutils ];
  } ''
    mkdir -p $out/raw
    # %m keeps counters from separate images distinct; %p avoids concurrent
    # writers. The directory is private to this derivation's build sandbox.
    export LLVM_PROFILE_FILE="$out/raw/%m-%p.profraw"
    ${training} ${instrumented}/bin/iets $out/results
    unset LLVM_PROFILE_FILE
    ${training} ${iets}/bin/iets reference
    diff -r reference $out/results
    # Every one of the nine invocations must flush a profile, and no corrupt
    # input may be silently discarded by the merge.
    test "$(find $out/raw -name '*.profraw' -type f | wc -l)" -ge 9
    for raw in $out/raw/*.profraw; do test -s "$raw"; done
    llvm-profdata merge -failure-mode=any -o $out/merged.profdata $out/raw/*.profraw
    llvm-profdata show $out/merged.profdata > $out/summary.txt
    sha256sum ${instrumented}/bin/iets > $out/instrumented.sha256
  '';
  pgo = variant "pgo" "-Cprofile-use=${profile}/merged.profdata -Cllvm-args=-pgo-warn-missing-function";
  boltInstrumented = runCommand "iets-pgo-bolt-instrumented" {
    nativeBuildInputs = [ bolt ];
  } ''
    mkdir -p $out/bin
    llvm-bolt ${pgo}/bin/iets -o $out/bin/iets -instrument \
      -instrumentation-file=bolt-profile -instrumentation-file-append-pid
  '';
  boltProfile = runCommand "iets-pgo-bolt-profile" {
    nativeBuildInputs = [ bolt diffutils ];
  } ''
    mkdir -p $out/raw
    ${training} ${boltInstrumented}/bin/iets $out/results
    diff -r ${profile}/results $out/results
    find . -name 'bolt-profile*' -type f -exec cp '{}' $out/raw/ \;
    test "$(find $out/raw -type f | wc -l)" -ge 9
    for raw in $out/raw/*; do test -s "$raw"; done
    merge-fdata $out/raw/* > $out/merged.fdata
    test -s $out/merged.fdata
  '';
  pgoBolt = runCommand "iets-pgo-bolt" {
    nativeBuildInputs = [ bolt ];
    meta.license = lib.licenses.lgpl21Plus;
  } ''
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
  inherit profilingSysroot profilerCheck baseline instrumented profile pgo boltInstrumented boltProfile pgoBolt;
  tools = symlinkJoin { name = "iets-pgo-tools"; paths = [ llvm bolt ]; };
}
