# Profile-guided optimization experiments

The flake exposes optional native Linux builds under `experiments`. They use
production Crane packaging, release settings and allocator, with tools from the
locked compiler's `rustc.llvmPackages`. Default packages and checks do not train
profiles. No production source, Cargo dependencies or toolchain pins change.

Build the matched variants on native x86-64 Linux:

```sh
nix build .#experiments.x86_64-linux.baseline -o result-baseline
nix build .#experiments.x86_64-linux.pgo -o result-pgo
nix build .#experiments.x86_64-linux.pgoBolt -o result-pgo-bolt
```

Configured remote builders can perform these native x86 builds from ARM.
`aarch64-linux` is also exposed, but only its profiling-runtime check has been
validated; full ARM PGO/BOLT builds and performance have not been measured.
All variants retain symbols and linker relocations. An explicit Cargo target
keeps instrumentation out of build scripts and proc macros. Rust target crates
receive LLVM instrumentation; GCC-built C dependencies do not. BOLT can optimize
eligible linked native code as well as Rust code.

The locked Rust compiler's bundled profiling runtime has a different ABI from
its external LLVM backend: even a minimal instrumented Rust program crashes.
`profilingSysroot` preserves the compiler, standard libraries and Rust profiler
metadata/object, replacing only the profiler's native objects with the matching
nixpkgs compiler-rt archive. Every variant selects that sysroot. `profilerCheck`
compiles/runs an indirect-call reproducer and merges its nonempty profile before
evaluator compilation. This compatibility check passes on x86-64 and ARM64;
the default package is unaffected.

Intermediate outputs are available for inspection:

| Attribute under `experiments.SYSTEM` | Output |
|---|---|
| `profilingSysroot`, `profilerCheck` | Runtime compatibility workaround and check |
| `instrumented` | LLVM-instrumented evaluator |
| `profile` | Raw/merged LLVM profiles, summary and checked outputs |
| `pgo` | Recompile using the merged LLVM profile |
| `boltInstrumented` | BOLT-instrumented PGO evaluator |
| `boltProfile` | Raw/merged BOLT profiles and checked outputs |
| `pgoBolt` | Optimized evaluator and BOLT log |
| `tools` | Matching LLVM/BOLT inspection commands |

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

There are no explicit weights or workload normalization. LLVM merges raw counts
with `llvm-profdata merge -failure-mode=any`; BOLT replays training on the PGO
executable and combines its separate instrumentation profiles with `merge-fdata`.
It does not convert LLVM or Callgrind profiles. Both stages require at least nine
nonempty raw files and exact stdout parity with the production evaluator. Profile
inputs depend on their executable, source, workloads and tools; changes rebuild
them. Cached build-time training is not a performance measurement.

Inspect coverage and compare held-out workloads outside derivations:

```sh
nix build .#experiments.x86_64-linux.profile -o result-profile
nix shell .#experiments.x86_64-linux.tools --command \
  llvm-profdata show --topn=20 result-profile/merged.profdata
cat result-pgo-bolt/bolt.log
nix run .#bench-ab -- "$PWD/result-baseline/bin/iets" "$PWD/result-pgo/bin/iets" \
  --only plasma --rounds 10 --max-rounds 10 --json pgo-plasma.json
nix run .#bench-ab -- "$PWD/result-pgo/bin/iets" "$PWD/result-pgo-bolt/bin/iets" \
  --only plasma --rounds 10 --max-rounds 10 --json bolt-plasma.json
```

Repeat with `--only minimal`. Compare the ordinary production executable with
the matched baseline to check for linker-setting effects. Use the same CPU set
for the whole comparison (`taskset -c CPU-LIST`), especially across different
cache clusters. Avoid competing builds/profilers, record system load, and check
output parity. Fixed pair counts avoid stopping based on interim results.
See the [benchmark guide](benchmark.md) for whole-Nixpkgs commands and profiling.

The full x86 pipeline passed nine-output training checks, all eight Gungraun
checks and a 36-run whole-Nixpkgs output comparison at 1/2/4/8 workers. Shared-host
timing varied with load; performance claims need repeated native comparisons.
Compiler block counts and Callgrind instructions are not CPU-time rankings.
Unstripped file sizes include debug information and do not measure code size.
BOLT reports DWARF rewriting warnings for Rust metadata; retain its log and treat
post-BOLT source annotations as incomplete. Verify runtime behavior separately.

Rust's [ordinary LLVM PGO](https://doc.rust-lang.org/rustc/profile-guided-optimization.html)
is supported; this experiment does not implement context-sensitive IR PGO.
[BOLT's input requirements](https://github.com/llvm/llvm-project/blob/main/bolt/README.md)
apply to Rust executables too. Raw profiles and timing logs are experiment
artifacts, not versioned inputs or portable baselines across revisions/targets.
