# Profile-guided optimization experiments

Build the three matched executables with the locked Nixpkgs toolchain on Linux.
These optional experiments use production release settings and retain symbols
and relocations. PGO is available on AArch64 Linux too. BOLT refuses to rewrite
the Cortex-A53 erratum veneers in the generic AArch64 executable. Use a
configured x86 builder on ARM for the complete commands below.

```sh
nix build .#experiments.x86_64-linux.baseline -o result-baseline
nix build .#experiments.x86_64-linux.pgo -o result-pgo
nix build .#experiments.x86_64-linux.pgoBolt -o result-pgo-bolt
```

Both profiling stages independently execute the same nine commands once:

| Training workload | Workers | Evaluator cache |
|---|---:|---|
| NixOS server, two invocations | 1 and 4 | Disabled |
| Server cache fill, cache hit, warm IR | 1 | Enabled as appropriate |
| Calls, strings, attributes, derivations | 1 | Disabled |

Training uses the locked nixpkgs and private HOME/cache directories. Cache
fill/hit/warm-IR commands share those caches within a stage. These are `eval`
commands with no configured memory cap; the `eval-jobs` restart ceiling does not
apply. Whole-Nixpkgs, Plasma, minimal NixOS and two/eight-worker configurations
are held out. This initial mix emphasizes server evaluation and its cache paths.

LLVM merges raw counts with `llvm-profdata merge`. BOLT instruments the PGO
executable, replays the workloads, and uses `merge-fdata`. Neither stage
normalizes workload weights. Each checks stdout against production and retains
per-workload raw profiles in `profile/raw` or `boltProfile/raw`. The BOLT log
records skipped functions and debug-info warnings.

Inspect coverage and compare held-out workloads outside derivations:

```sh
nix build .#experiments.x86_64-linux.profile -o result-profile
nix shell .#experiments.x86_64-linux.tools --command \
  llvm-profdata show --topn=20 result-profile/merged.profdata
cat result-pgo-bolt/bolt.log
nix run .#bench-ab -- "$PWD/result-baseline/bin/iets" "$PWD/result-pgo/bin/iets" \
  --full --only minimal --rounds 10 --max-rounds 10 --json pgo-minimal.json
nix run .#bench-ab -- "$PWD/result-pgo/bin/iets" "$PWD/result-pgo-bolt/bin/iets" \
  --full --only minimal --rounds 10 --max-rounds 10 --json bolt-minimal.json
```

Repeat with `--only nixos/server-w1`. Compare the ordinary production executable with
the matched baseline to check for linker-setting effects. Use the same CPU set
for the whole comparison (`taskset -c CPU-LIST`), especially across different
cache clusters. Avoid competing builds/profilers, record system load, and check
output parity. Fixed pair counts avoid stopping based on interim results.
Pass `--full --only outpaths` to the runner for a whole-Nixpkgs comparison.
