# Line coverage of iets from the e2e tests in NixOS containers: an
# instrumented iets runs each script, and the profiles of every process are
# merged into one report ($out/report.txt, by directory and by file).
# `nix build .#coverage-e2e`, on x86_64-linux.
{ lib, rustc, gawk, runCommand, iets, e2eTest, e2eCases }:
let
  # the LLVM that rustc was built with reads its profiles
  llvm = rustc.llvmPackages.llvm;
  instrumented = iets.override {
    wrapGit = false;
    extraArgs = {
      pname = "iets-instrumented";
      RUSTFLAGS = "-Cinstrument-coverage";
      dontStrip = true;
    };
  };
  e2e = name: args: e2eTest ({
    inherit name;
    iets = instrumented;
    coverage = true;
  } // args);
  runs = lib.mapAttrsToList e2e e2eCases;
in
runCommand "iets-coverage-e2e" { nativeBuildInputs = [ llvm gawk ]; } ''
  mkdir -p $out
  find ${lib.concatMapStringsSep " " (r: "${r}/cov") runs} -name '*.profraw' > profiles
  echo "$(wc -l < profiles) profiles from ${toString (builtins.length runs)} e2e runs" | tee $out/report.txt
  llvm-profdata merge -sparse -f profiles -o $out/coverage.profdata
  # paths in the binary are those of the build sandbox; the report names
  # them from src/ on
  llvm-cov report ${instrumented}/bin/iets -instr-profile=$out/coverage.profdata \
    -ignore-filename-regex='/(\.cargo|cargo-vendor-dir|rustc)/' > files.txt
  awk '$1 ~ /src\// {
         f = $1; sub(/.*src\//, "", f); n = split(f, p, "/"); d = n > 1 ? p[1] : "(top)"
         lines[d] += $8; miss[d] += $9; L += $8; M += $9
       }
       END {
         printf "\n%-12s %7s %8s\n", "directory", "lines", "covered"
         for (d in lines) printf "%-12s %7d %7.1f%%\n", d, lines[d], 100 * (lines[d] - miss[d]) / lines[d] | "sort -k2 -nr"
         close("sort -k2 -nr")
         printf "%-12s %7d %7.1f%%\n", "TOTAL", L, 100 * (L - M) / L
       }' files.txt >> $out/report.txt
  printf '\n' >> $out/report.txt
  sed 's|^.*/src/|src/|' files.txt >> $out/report.txt
''
