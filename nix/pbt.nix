# The properties of nix-pbt, compiled, and `nix-pbt` to run them
# with the reference Nix first and iets compared against it:
#
#   nix run .#nix-pbt                  # every suite
#   nix run .#nix-pbt -- arith lists   # some of them
#   HEGEL_TEST_CASES=2000 nix run .#nix-pbt -- strings -- substring
#
# What follows `--` goes to the test binaries. Nothing runs the properties
# in a check yet: the suites that touch the store, the fetchers and flakes
# still report differences nobody has sorted.
{
  lib,
  craneLib,
  writeShellApplication,
  coreutils,
  findutils,
  git,
  gnutar,
  gzip,
  src,
  iets,
  nix,
}:
let
  args = {
    pname = "nix-pbt";
    version = "0.1.0";
    inherit src;
    strictDeps = true;
  };
  tests = craneLib.mkCargoDerivation (
    args
    // {
      cargoArtifacts = craneLib.buildDepsOnly args;
      doInstallCargoArtifacts = false;
      buildPhaseCargoCommand = "cargo test --release --locked --no-run";
      installPhaseCommand = ''
        mkdir -p $out/libexec/nix-pbt
        for t in tests/*.rs; do
          t=$(basename $t .rs)
          install -m755 "$(find target/release/deps -maxdepth 1 -name "$t-*" -type f -executable)" \
            $out/libexec/nix-pbt/$t
        done
      '';
    }
  );
in
writeShellApplication {
  name = "nix-pbt";
  runtimeInputs = [
    coreutils
    findutils
    git
    gnutar
    gzip
  ];
  text = ''
    # the expressions name files of the checkout
    export NIX_PBT_ROOT="''${NIX_PBT_ROOT:-${src}}"
    # The first evaluator is the reference; one process for each expression.
    # Read-write, or Nix calls every .drv path it did not write invalid.
    export NIX_PBT_EVALUATORS="''${NIX_PBT_EVALUATORS:-nix=${lib.getExe' nix "nix-instantiate"} --eval --strict --json --read-write-mode --extra-experimental-features 'nix-command flakes' -E;iets=${lib.getExe iets} eval --impure --strict --json -E}"
    suites=()
    while [ $# -gt 0 ] && [ "$1" != -- ]; do
      suites+=("$1")
      shift
    done
    [ $# -eq 0 ] || shift
    if [ ''${#suites[@]} -eq 0 ]; then
      mapfile -t suites < <(ls ${tests}/libexec/nix-pbt)
    fi
    status=0
    for s in "''${suites[@]}"; do
      echo "== $s" >&2
      ${tests}/libexec/nix-pbt/"$s" "$@" || status=1
    done
    exit $status
  '';
  passthru = { inherit tests; };
}
