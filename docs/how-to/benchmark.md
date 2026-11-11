# Benchmark evaluation

On Linux, the flake packages the harnesses, workload files, evaluator and profiling tools.
No Cargo installation of the runner or host Rust toolchain is needed.

Build the commands without running measurements:

```sh
nix build .#bench-ab .#bench-callgrind .#bench-report .#bench-workloads
```

Compare two release executables with alternating native runs:

```sh
nix run .#bench-ab -- /absolute/path/to/before /absolute/path/to/after --json ab.json
```

This covers minimal, server and Plasma NixOS configurations, pure evaluation,
warm IR, tracing-cache hits and edits. `--only synthetic` runs the small
Callgrind fixtures natively; they are excluded by default because wall-time
noise dominates such short runs. `--full` adds release-small and chunked
and whole nixpkgs jobsets; these can need more than 20 GB of memory. `--only`
filters scenario names. `NIXPKGS` overrides the default workload input, the
flake's locked nixpkgs. Workload files are installed in the Nix store, so the
command works outside the checkout. Cold here means evaluator caches: the
filesystem page cache is warmed before measurement.
Native hardware counters are estimates scaled by the PMU's enabled/running
time when events are multiplexed; unscheduled events are omitted. Short runs
can still have poor counter coverage. Native `--threshold` gates wall time and
peak RSS; hardware counters remain descriptive. Use Callgrind for instruction
regressions on the synthetic fixtures.

For instruction counts and Callgrind profiles:

```sh
nix run .#bench-callgrind -- 'eval::evaluation::*' --save-baseline=before --save-summary=json
# Edit the evaluator, then rebuild automatically on the next invocation:
nix run .#bench-callgrind -- 'eval::evaluation::*' --baseline=before --save-summary=json
```

The synthetic workloads exercise repeated calls, unique strings, attribute
updates/lookups and repeated/distinct derivations. Each checks its result with
an assertion, and the harness independently checks the complete JSON output
outside the measurement after every tool run. An executable that exits successfully
without evaluating the fixture therefore fails validation. Runs use one worker,
strict JSON output and disabled evaluator caches. They include executed CLI startup,
evaluation, output and process exit, and use the production allocator. `iets` exits
with `std::process::exit`, so this does not promise that stack destructors run.
They need no downloads, Nix daemon or nixpkgs evaluation at measurement time.
The packaged evaluator retains debug symbols for profile annotation.
Gungraun clears the measured process's environment, excluding local `IETS_*`
settings. Use the native suite to assess parallel scaling and actual elapsed
time: fewer instructions need not mean fewer cycles.

The `components` group uses the same generated literal input for four commands:

| Filter suffix | Command / boundary |
|---|---|
| `frontend::parse` | `parse -q`: read and parse 2,000 nested attribute bindings |
| `frontend::ast_json` | `parse --json`: parse and render the syntax tree |
| `render::json` | `eval --json`: compile, evaluate, force and render the value |
| `render::nix` | `eval --strict`: compile, evaluate, force and render Nix syntax |

Setup writes the fixed input into a fresh writable sandbox before starting the
measured executable. Teardown checks the parsed AST, every JSON value, or complete
Nix output afterwards. Parsing and printing are visible without the `genList`,
string interpolation and folding scaffolding in the evaluation fixtures. These
are command boundaries: subtracting parser instructions from evaluation does not
give an exact evaluator-only cost, since lifecycles and output paths differ.

```sh
nix run .#bench-callgrind -- --list
nix run .#bench-callgrind -- 'eval::components::*' --save-baseline=before --save-summary=json
nix run .#bench-callgrind -- 'eval::components::*' --baseline=before --save-summary=json
nix run .#bench-report -- target/gungraun
```

Callgrind always saves raw profiles. For a focused benchmark, opt into aggregate
instruction (`Ir`) and total read/write (`TotalRW`) SVG flamegraphs; comparisons
also generate differential SVGs:

```sh
IETS_BENCH_FLAMEGRAPHS=1 nix run .#bench-callgrind -- '*::components::frontend::parse' \
  --save-baseline=graphs --save-summary=json
IETS_BENCH_FLAMEGRAPHS=1 nix run .#bench-callgrind -- '*::components::frontend::parse' \
  --baseline=graphs --save-summary=json
```

SVG generation is disabled by default. Expanding recursive graphs can be expensive:
an all-suite differential run consumed about 25 GB before it was stopped. Filter
one case at a time when generating graphs. `TotalRW` includes instruction fetches as well as data accesses. The graphs
show counted events, not time. Raw per-thread Callgrind profiles remain available
for `callgrind_annotate` or KCachegrind. `--show-intermediate` shows each thread
and process fragment as well as their aggregate; gating uses the aggregate.
The report command checks that raw additive totals equal the sum of fragments
and computes weighted cache miss rates from their raw events. Gungraun 0.20's
aggregate derived percentages can sum per-thread percentages, producing invalid
values above 100%; do not interpret those aggregate rates as hardware metrics.

Results and profiles go to `target/gungraun/<target-triple>` in the invoking
directory. The packaged command enables Gungraun's target separation by default;
local Cargo invocations should pass `--separate-targets`. Set
`GUNGRAUN_HOME` to keep them elsewhere. To compare an existing executable using
the same harness and fixtures, set `IETS_BENCH_BIN` to its absolute path.
`--callgrind-limits='Ir=5%'` fails on an instruction increase above 5 percent;
`--help` lists filters, profiles and other runner options.
`IETS_BENCH_WORKLOADS` can override the installed fixture directory; keep identical
fixtures on both sides. Recreate both baselines when changing fixtures, harness,
compiler or collection options. The explicit `EntryPoint::None` matches Gungraun's
binary default and counts every thread from process startup, unlike a library
benchmark's default function toggle.

Additional visibility is opt-in:

```sh
# Branch predictor simulation, alongside the default cache simulation:
nix run .#bench-callgrind -- '*::evaluation::*::attrs' \
  --callgrind-args=--branch-sim=yes --callgrind-metrics='@default,@bs' --show-intermediate
# Native perf counters, in a separate execution in addition to Callgrind:
nix run .#bench-callgrind -- '*::evaluation::*::attrs' \
  --tools=perf --perf-events=instructions:u --save-summary=json
# Instrumented syscall diagnostics, nanoseconds (not native syscall latency):
nix run .#bench-callgrind -- '*::evaluation::*::attrs' \
  --callgrind-args=--collect-systime=nsec --callgrind-metrics='Ir,SysCount,SysTime,SysCpuTime'
# Program-level allocation counters; keep these results separate from count gates:
nix run .#bench-callgrind -- '*::evaluation::*::attrs' \
  --envs=IETS_STATS=1 --nocapture=stderr
```

Perf requires permission to access the requested events; a failed or unsupported
event is not a simulated measurement. Use `--perf-record` with `--tools=perf` to
also produce native `perf.data`; `--perf-record-args=--call-graph=dwarf` requests
call stacks. `--default-tool=perf --tools=perf` selects only perf, replacing the
explicitly configured Callgrind tool as well as the default.

Callgrind instruments user-space instructions, serializes threads and perturbs
syscall arrival, scheduling, cache state and elapsed time. `Ir` does not count
kernel instructions. Gungraun does not compensate these effects back to native
syscall costs; `SysTime` and `SysCpuTime` describe that instrumented execution.
Simulated cache/branch events and `EstimatedCycles` are model results, not the
host CPU's counters. Use the native suite/perf for elapsed time and scaling.

For the Nix-expression view on real workloads, use the evaluator's own
[`IETS_PROFILE=eval` or `alloc`](../reference/profile.md), `IETS_STATS=1`,
`IETS_PAR_STATS`, and `eval --timings` directly. Those profiles explain Nix call
stacks and arena allocations; the profiler pins evaluation to one worker.
Keep diagnostic profiles separate from regression baselines. Generic DHAT/Massif
heap views do not automatically classify objects inside mimalloc or the arenas.
See the [Gungraun audit](../explanation/benchmark-harness.md) for the checked
measurement boundaries, tool choices and validation evidence.

`nix develop .#bench` supplies Cargo, Rust, Valgrind and the matching Gungraun
runner. For a local Cargo build, first run `cargo build --release --bin iets`,
then `cargo bench --manifest-path bench/Cargo.toml --bench eval`.
The standalone benchmark package has its own lockfile, like the fuzz tools;
benchmark edits do not invalidate the production package or its dependencies.
Nix derives the runner version and source checksum from `bench/Cargo.lock`,
rather than maintaining a second version setting. Both Rust harnesses and the
runner use Crane with separately cached dependency artifacts.
The runner is packaged locally because the locked nixpkgs has no Gungraun
package. Other tools come from that nixpkgs. The benchmark environment applies
Valgrind's [upstream auxiliary-vector fix](https://sourceware.org/cgit/valgrind/commit/?id=de9c4730516522bbc02b55caf4c4df376dd0f169)
until nixpkgs includes it; without it, rustix can crash under Valgrind on Linux
6.4 and newer.

`nix build .#checks.aarch64-linux.bench-smoke` (or `x86_64-linux`) executes every
fixture through the installed harness, both directly and under Valgrind.
The build-time instruction counts are discarded. Performance measurements
run outside derivations so the build cache never substitutes for a measurement.
PR CI builds both executables with the candidate's locked Nix inputs, runs the
native comparison and all eight evaluation/component instruction comparisons, and uploads the raw profiles and summaries. Its step summary includes the checked
aggregate/per-thread report. The baseline package may have stripped debug
information; candidate source/inline annotations are richer. For complete source-level differential graphs, compare two executable
builds with debug information retained.

The harness uses [Gungraun](https://github.com/gungraun/gungraun), the
continuation of iai-callgrind; the linked
[Clockwork Labs fork](https://github.com/clockworklabs/iai-callgrind) is archived.
Binary benchmarks retain the CLI's production allocator and executed process lifecycle.
The small fixtures make instruction comparisons practical on every PR; the
native suite measures the effects of scheduling, memory and parallelism.

The workload choices follow upstream's open and closed PRs:

- [Merged #406](https://github.com/numtide/iets/pull/406) supplies the native
  runner: alternating pairs, output parity, confidence intervals, cache
  scenarios and optional large jobsets. This change packages and extends it.
- [Open #371](https://github.com/numtide/iets/pull/371) and
  [#372](https://github.com/numtide/iets/pull/372) measure server and Plasma
  configurations at one and four workers, including cold and warm IR. Plasma
  is included here alongside the existing server scenarios.
- [Closed #325](https://github.com/numtide/iets/pull/325) and
  [merged #330](https://github.com/numtide/iets/pull/330) motivate call-path
  coverage; the latter also measures Fibonacci. The synthetic fixtures are
  new, bounded checks, rather than copies of those PRs' measurement inputs.
- [Closed #403](https://github.com/numtide/iets/pull/403) and
  [open #401](https://github.com/numtide/iets/pull/401) show why instruction
  counts cannot replace native runs: huge-page availability and memory
  pressure can change the result. The privileged machine-state controller
  proposed in #403 is outside this harness.

Keep the same harness, fixtures, Nix inputs and hardware for both executables.
Baselines from different architectures or toolchains are not comparable.

For an unbounded whole-Nixpkgs evaluation, use the locked input's outpaths
expression and explicitly disable the job runner's default 4 GiB worker restart
threshold. `--max-memory-size 0` selects one shared evaluator process with no
memory ceiling; `--workers` controls its evaluator threads. This differs from
the default capped worker-process model in the native suite's large scenarios.
For example, on native x86-64 Linux:

```sh
nix build .#default -o result
np=$(nix eval --raw --expr '(builtins.getFlake (toString ./.)).inputs.nixpkgs.outPath' --impure)
printf 'import %s/ci/eval/outpaths.nix { path = %s; systems = [ "x86_64-linux" ]; }\n' "$np" "$np" > outpaths.nix
./result/bin/iets eval-jobs --impure --no-instantiate --no-cache \
  --max-memory-size 0 --workers 8 -I "nixpkgs=$np" ./outpaths.nix > jobs.jsonl
```

Repeat with both executables and identical inputs at each worker count. Retain
per-job errors as part of output parity; Nixpkgs
contains intentional failing attributes. Compare JSON records independently of
line order and exclude their diagnostic `stats` values. Record wall time, peak
RSS and swapping: removing the evaluator ceiling does not prevent host memory
pressure, swap or inherited OS limits from affecting a result.
