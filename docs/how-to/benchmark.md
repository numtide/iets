# Benchmark evaluation

On Linux, compare two release executables with the native runner.

```sh
nix run .#bench-ab -- /absolute/path/to/before /absolute/path/to/after --json ab.json
```

The runner alternates the binaries and checks output parity on NixOS server,
Plasma, minimal, cache and release-initial-ramdisk evaluations. Use `--only SUBSTR`
to select scenarios. For example, compare the full host-system NixOS release
jobset or the whole host-system Nixpkgs outpaths jobset on a larger machine:

```sh
nix run .#bench-ab -- BEFORE AFTER --full --only jobset/release-w4
nix run .#bench-ab -- BEFORE AFTER --full --only nixpkgs/outpaths-w8
```

`--full` also includes release-small and a Nixpkgs chunk. Whole evaluations
can use more than 20 GiB.
`NIXPKGS` overrides the locked input. Record system load, peak RSS and swapping
when interpreting small timing changes. With `--threshold`, the native runner
checks wall time, peak RSS and, when fully available, retired hardware instructions.
The GitHub Actions job summary shows the native paired intervals followed by
a table of base and candidate Callgrind instructions for each workload. Both
binaries run on the same hosted Linux VM. The 5% gates use the lower bound
of each native interval and the change in total guest instructions. To run the CI comparison locally, use
`nix run .#bench-compare -- /absolute/path/to/baseline/iets` in a writable
directory with the candidate flake. The `ab` artifact contains `ab.json`,
including the Linux one-minute system load sampled before each native run
(`load1`), and the raw Gungraun profiles.

## Count instructions and find hotspots

```sh
nix run .#bench-callgrind -- --list
nix run .#bench-callgrind -- '*::nixpkgs::*::initial_ramdisk' --save-baseline=before --save-summary=json
# After editing the evaluator, rerun the same workload.
nix run .#bench-callgrind -- '*::nixpkgs::*::initial_ramdisk' --baseline=before \
  --callgrind-limits='Ir=5%' --callgrind-metrics=Ir --show-intermediate --save-summary=json
```

Omit the filter to run the suite. Set `IETS_BENCH_BIN` to select a built evaluator.
Keep the harness, fixtures, compiler and collection options identical across a
comparison. Look under `target/gungraun/<target-triple>` for JSON summaries and
raw profiles. Open a profile in KCachegrind or use `callgrind_annotate` to see
which functions account for a change. For instruction addresses, pass
`--callgrind-args='--dump-instr=yes --skip-plt=no'`. For differential SVGs,
set `IETS_BENCH_FLAMEGRAPHS=1` on both runs.

| Workload | What the command exercises |
|---|---|
| `nixpkgs::job::hello` | Import the locked Nixpkgs and force the derivation path of `hello` |
| `nixpkgs::job::initial_ramdisk` | Evaluate the NixOS release jobset’s `initialRamdisk` derivation for the host system |
| `calls`, `strings`, `attrs` | Function calls, string operations, attribute updates and lookups |
| `derivations` | Construct and hash input-addressed derivations |
| `producer_inputs` | Build consumer derivations with shared and individual producer inputs |
| `fixed_derivations` | Derive store paths for flat and recursive fixed-output recipes |
| `frontend::parse`, `frontend::ast_json` | Parse the same 2,000-binding literal quietly or emit its AST |
| `components::plain_sets` | Print 8,192 ordinary attribute sets through classic `nix eval`, checking the bytes against Nix |
| `render::json`, `render::nix` | Evaluate and render that literal as strict JSON or Nix syntax |

The two real jobs check the resulting derivation path against `nix-instantiate`.
The synthetic fixtures isolate individual operations when investigating changes
seen in larger evaluations.

Counts include CLI startup and all executed threads. Setup and output checks run
outside collection. The evaluation workloads use one worker and disabled caches.
Differences between commands include all work along each path. Callgrind
serializes threads, changes syscall timings and can select different CPU feature
paths, including software SHA. Verify a simulated hotspot on the native binary.
Use `--help` for branch simulation, native perf and other collection options.
The [evaluator profiler](../reference/profile.md) attributes work to Nix expressions.

## Evaluate whole Nixpkgs without a worker memory limit

```sh
nix build .#default -o result
np=$(nix eval --raw --impure --expr '(builtins.getFlake (toString ./.)).inputs.nixpkgs.outPath')
printf 'import %s/ci/eval/outpaths.nix { path = %s; systems = [ "x86_64-linux" ]; }\n' "$np" "$np" > outpaths.nix
./result/bin/iets eval-jobs --impure --no-instantiate --no-cache \
  --max-memory-size 0 --workers 8 -I "nixpkgs=$np" ./outpaths.nix > jobs.jsonl
```

Vary `--workers` for scaling comparisons. Compare records independently of line
order, excluding diagnostic `stats` and retaining errors.
