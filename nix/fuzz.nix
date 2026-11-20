# The fuzz targets as packages, checks and campaigns (docs/how-to/fuzz.md).
{ pkgs, craneLib, sources, nixpkgs, nix-conformance, iets }:
let
  inherit (pkgs) lib;
  oracles = nix-conformance.packages.${pkgs.stdenv.hostPlatform.system}.oracles;
  # the cases and the grown corpora of nix-conformance, as libFuzzer seeds
  link = dirs: name: ''
    find ${toString dirs} -name '${name}' | while read -r f; do
      ln -sf "$f" "seeds/$(echo "$f" | sha1sum | cut -c1-40)"
    done
  '';
  # what each libFuzzer binary is run with; the seeds go to a directory `seeds`.
  # `runs` is the number of inputs of a check, seeds included; the comments of
  # the checks give the time this takes on an x86_64 builder.
  targets = {
    parse = {
      flags = "-max_len=4096 -dict=${../fuzz/parse.dict}";
      # an eighth of nixpkgs in a check
      seeds = every: ''
        find ${nixpkgs} -name '*.nix' -size -32k | awk 'NR % ${toString every} == 0' | while read -r f; do
          ln -s "$f" "seeds/$(echo "$f" | sha1sum | cut -c1-40)"
        done
        ${link [ "${nix-conformance}/parse" "${nix-conformance}/corpus/parse" ] "*.nix"}
      '';
      runs = 100000;
    };
    nar-read = {
      flags = "-max_len=65536 -dict=${../fuzz/nar.dict}";
      seeds = _: link [ "${nix-conformance}/nar" "${nix-conformance}/corpus/nar" ] "*.nar";
      runs = 30000;
    };
    nar-gen = { flags = "-max_len=4096"; seeds = _: ""; runs = 7500; };
    nar-tree = { flags = "-max_len=4096"; seeds = _: ""; runs = 2500; };
    wire = { flags = "-max_len=4096"; seeds = _: ""; runs = 4000; };
    # a hang is a bug of its own: the expressions end (fuzz/src/eval.rs)
    eval = { flags = "-max_len=4096 -timeout=10"; seeds = _: ""; runs = 150000; };
    drv-eval = { flags = "-max_len=4096 -timeout=10"; seeds = _: ""; runs = 20000; };
    drv-read = {
      flags = "-max_len=16384 -dict=${../fuzz/drv.dict}";
      # with the inputs that once diverged, so that each check runs them again
      seeds = _: link [ "${nix-conformance}/drv" "${../fuzz/regressions/drv-read}" ] "*.drv";
      runs = 150000;
    };
    drv-gen = { flags = "-max_len=4096"; seeds = _: ""; runs = 200000; };
  };
  env = ''
    export IETS_PARSE_ORACLE=${oracles}/bin/parse-oracle
    export IETS_NAR_ORACLE=${oracles}/bin/nar-oracle
    export IETS_WIRE_ORACLE=${oracles}/bin/wire-oracle
    export IETS_EVAL_ORACLE=${oracles}/bin/eval-oracle
    export IETS_DRV_ORACLE=${oracles}/bin/drv-oracle
    # scratch trees on tmpfs: it lists a directory in the order its entries
    # were made, where the build directory may not (nar-read, fuzz/src/nar.rs)
    export TMPDIR=/dev/shm
  '';
  # the replay tool, built plain: it is fast and needs no libFuzzer
  fuzz-diff =
    let
      args = {
        pname = "iets-fuzz-diff";
        version = "0.0.0";
        strictDeps = true;
        src = sources.fuzz;
        cargoLock = ../fuzz/Cargo.lock;
        cargoToml = ../fuzz/Cargo.toml;
        postUnpack = "cd $sourceRoot/fuzz; sourceRoot=.";
        cargoExtraArgs = "--locked --bin fuzz-diff --bin conformance-adapter";
        doCheck = false;
      };
    in
    craneLib.buildPackage (args // {
      cargoArtifacts = craneLib.buildDepsOnly args;
      meta.license = lib.licenses.lgpl21Plus;
    });
  # the libFuzzer binaries, built as cargo-fuzz 0.13 builds them (cargo fuzz
  # build -v), in one cargo call for every target and on cached deps
  fuzzerArgs = {
    pname = "iets-fuzzers";
    version = "0.0.0";
    strictDeps = true;
    src = sources.fuzz;
    cargoLock = ../fuzz/Cargo.lock;
    cargoToml = ../fuzz/Cargo.toml;
    postUnpack = "cd $sourceRoot/fuzz; sourceRoot=.";
    doCheck = false;
    RUSTFLAGS = toString [
      "-Cpasses=sancov-module"
      "-Cllvm-args=-sanitizer-coverage-level=4"
      "-Cllvm-args=-sanitizer-coverage-inline-8bit-counters"
      "-Cllvm-args=-sanitizer-coverage-pc-table"
      "-Cllvm-args=-sanitizer-coverage-trace-compares"
      "--cfg fuzzing"
      "-Cllvm-args=-simplifycfg-branch-fold-threshold=0"
      "-Cllvm-args=-sanitizer-coverage-stack-depth"
      "-Ccodegen-units=1"
    ];
  };
  fuzzerBuild = what: ''
    cargo build --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --release \
      --config 'profile.release.debug="line-tables-only"' ${what}
  '';
  fuzzers = craneLib.mkCargoDerivation (fuzzerArgs // {
    doInstallCargoArtifacts = false;
    # the dummy bins of the deps cannot link without libFuzzer
    cargoArtifacts = craneLib.buildDepsOnly (fuzzerArgs // { buildPhaseCargoCommand = fuzzerBuild "--lib"; });
    buildPhaseCargoCommand = fuzzerBuild (lib.concatMapStringsSep " " (t: "--bin ${t}") (builtins.attrNames targets));
    installPhaseCommand = ''
      for t in ${toString (builtins.attrNames targets)}; do
        install -Dm755 target/*/release/$t $out/bin/fuzz-$t
      done
    '';
    dontFixup = true;
    meta.license = lib.licenses.lgpl21Plus;
  });
  # one target for a fixed number of inputs; a divergence fails the build and
  # prints the input. A count, not a time: a slow builder must not stop before
  # the input that a fast one reaches. -reload=0, as libFuzzer reads the corpus
  # again each second. The campaigns fuzz for a time.
  # -timeout=30, as the default of 1200 s an input is the silence timeout of
  # CI; the -timeout of a target comes after it, and the last one counts.
  run = name: ''
    mkdir -p seeds new-${name}
    rm -f seeds/*
    ${targets.${name}.seeds 8}
    # the RNG seed from $out, which the drv hash names: a new seed at each
    # change, the same inputs again on a rebuild. libFuzzer takes 0 as random.
    seed=$(( 16#$(echo -n "$out ${name}" | sha256sum | cut -c1-8) % 4294967295 + 1 ))
    # libFuzzer runs the seeds by size, and two of one size in the order it
    # got them. A directory lists its files in an order of its own, a list
    # does not.
    printf %s "$(find seeds -mindepth 1 | sort | paste -sd, -)" > seeds.list
    # without address randomization: sets keyed on pointers (seen sets of the
    # evaluator) probe in another order at each address
    setarch "$(uname -m)" -R \
      fuzz-${name} -seed=$seed -runs=${toString targets.${name}.runs} -reload=0 -timeout=30 ${targets.${name}.flags} \
      $([ -s seeds.list ] && echo -seed_inputs=@seeds.list) \
      -artifact_prefix=art-${name}- new-${name} || {
        for a in art-${name}-*; do echo "the input, in base64: $(base64 -w0 "$a")" >&2; done
        exit 1
      }
    # the grown corpus, to compare two runs of one check
    echo "${name}: $(ls new-${name} | wc -l) inputs, sha256 $(cat new-${name}/* | sha256sum | cut -c1-16)"
  '';
  check = { name, script }: pkgs.runCommand "iets-fuzz-${name}-check" {
    nativeBuildInputs = [ fuzz-diff fuzzers oracles pkgs.util-linux ];
  } ''
    ${env}
    ${script}
    mkdir -p $out
  '';
  # a long run of one target on every core: the grown corpus, minimised, and
  # every input that diverged, replayed and classified in triage.jsonl
  campaign = { target ? "parse", seconds ? 3600, corpus ? null }: pkgs.runCommand "iets-fuzz-${target}-campaign" {
    nativeBuildInputs = [ fuzz-diff fuzzers oracles pkgs.jq ];
  } ''
    ${env}
    mkdir seeds grown $out $out/corpus $out/artifacts
    ${targets.${target}.seeds 1}
    fuzz-${target} -merge=1 ${targets.${target}.flags} grown seeds ${toString corpus} 2> $out/seed.log
    fuzz-${target} -fork=$NIX_BUILD_CORES -ignore_crashes=1 -max_total_time=${toString seconds} \
      ${targets.${target}.flags} -artifact_prefix=$out/artifacts/ grown 2> $out/fuzz.log || true
    fuzz-${target} -merge=1 ${targets.${target}.flags} $out/corpus grown 2> $out/merge.log
    # one process per artifact: iets's arena is never freed, and an artifact
    # can be one that ran out of memory; each writes its own file, as a
    # divergence line can be longer than a pipe writes at once
    mkdir triage
    find $out/artifacts -type f -printf '%f\n' | xargs -r -P $NIX_BUILD_CORES -I{} \
      sh -c 'fuzz-diff --json ${target} "$1/$2" > "triage/$2" 2> /dev/null || true' _ $out/artifacts {}
    find triage -type f | sort | xargs -r cat > triage.all
    { grep -v '^{"summary":' triage.all || true; } > $out/triage.jsonl
    { grep '^{"summary":' triage.all || true; } | jq -s -c -S '{summary: {
      inputs: (map(.summary.inputs) | add // 0),
      same: (map(.summary.same) | add // 0),
      bugs: (map(.summary.bugs) | add // 0),
      expected: (reduce (.[].summary.expected | to_entries[]) as $e ({}; .[$e.key] += $e.value)),
      seconds: (map(.summary.seconds) | add // 0)}}' >> $out/triage.jsonl
  '';
in
{
  inherit oracles fuzz-diff fuzzers campaign;
  checks = {
    # every .nix file of nixpkgs through both parsers, then about a minute of libFuzzer
    fuzz-parse = check {
      name = "parse";
      script = ''
        fuzz-diff --nix-only parse ${nixpkgs}
        ${run "parse"}
      '';
    };
    # every case of nix-conformance through iets, with the trees; the cases
    # in conformance-known fail on purpose. The runner prints only failures
    # and skips, and its quiet stretches outlast max-silent-time on a loaded
    # builder, so the adapter names each case on fd 3, the build log. A case
    # past the adapter's timeout (exit 124) is listed and fails the check.
    # The runner takes one case at a time; the cases go round-robin to
    # parallel runners, as one runner takes minutes.
    conformance = pkgs.runCommand "nix-conformance-iets" { } ''
      exec 3>&2
      cd ${nix-conformance}
      find lang parse nar drv flake wire -type f | sort > $NIX_BUILD_TOP/cases
      n=$(( NIX_BUILD_CORES < 16 ? NIX_BUILD_CORES : 16 ))
      split -n r/$n $NIX_BUILD_TOP/cases $NIX_BUILD_TOP/shard-
      for s in $NIX_BUILD_TOP/shard-*; do
        { xargs -a $s ${nix-conformance.packages.${pkgs.stdenv.hostPlatform.system}.run}/bin/nix-conformance \
            --tree --known ${../fuzz/conformance-known} ${pkgs.writeShellScript "iets-conformance-adapter" ''
              echo "case $*" 2>/dev/null >&3
              IETS=${iets}/bin/iets ${fuzz-diff}/bin/conformance-adapter "$@" 3>&-
              code=$?
              if [ $code = 124 ] && [ -n "''${NIX_BUILD_TOP:-}" ]; then
                echo "timed out: $*" | tee -a "$NIX_BUILD_TOP/timed-out" 2>/dev/null >&3
              fi
              exit $code
            ''} > $s.out 2>&1; echo $? > $s.code; } &
      done
      wait
      ok=1
      for s in $NIX_BUILD_TOP/shard-??; do
        cat $s.out
        [ "$(cat $s.code)" = 0 ] || ok=
      done
      if [ -s "$NIX_BUILD_TOP/timed-out" ]; then
        cat "$NIX_BUILD_TOP/timed-out" >&2
        ok=
      fi
      [ -n "$ok" ]
      touch $out
    '';
    # about half a minute of iets's daemon client against Nix's, on scripted daemons
    fuzz-wire = check {
      name = "wire";
      script = run "wire";
    };
    # about a minute of generated expressions through both evaluators
    fuzz-eval = check {
      name = "eval";
      script = run "eval";
    };
    # about twenty seconds of each derivation target
    fuzz-drv = check {
      name = "drv";
      script = lib.concatMapStrings run [ "drv-read" "drv-gen" ];
    };
    # about thirty seconds of derivation calls, their drvPaths and outPaths
    fuzz-drv-eval = check {
      name = "drv-eval";
      script = run "drv-eval";
    };
    # about twenty seconds of each NAR target
    fuzz-nar = check {
      name = "nar";
      script = lib.concatMapStrings run [ "nar-read" "nar-gen" "nar-tree" ];
    };
  };
}
