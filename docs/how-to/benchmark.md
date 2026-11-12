# Benchmark evaluation

On Linux, build the Nix-packaged harnesses and tools:

```sh
nix build .#bench-ab .#bench-callgrind .#bench-workloads
```

Compare two release executables with the existing native A/B runner:

```sh
nix run .#bench-ab -- /absolute/path/to/before /absolute/path/to/after --json ab.json
```

This covers minimal, server and Plasma NixOS configurations, pure evaluation,
warm IR, tracing-cache hits and edits. `--full` adds release-small and chunked
and whole Nixpkgs jobsets, which can need more than 20 GiB. `--only` filters
scenario names. `NIXPKGS` overrides the locked input. Cold means evaluator
caches, not a cold OS page cache. The runner alternates pairs, checks output
parity and reports intervals; `--threshold` gates wall time and peak RSS.
Hardware counters are descriptive estimates scaled by PMU enabled/running time;
unscheduled events are omitted and short-run coverage can still be poor.

For repeatable instruction comparisons, use Gungraun's built-in baselines,
regression gates and per-thread reporting:

```sh
nix run .#bench-callgrind -- --save-baseline=before --save-summary=json
# Edit the evaluator; the next invocation rebuilds it.
nix run .#bench-callgrind -- --baseline=before --callgrind-limits='Ir=5%' \
  --callgrind-metrics=Ir --show-intermediate --save-summary=json
```

The four evaluation fixtures cover calls, strings, attribute updates/lookups
and derivations. They run with one worker, strict JSON and disabled evaluator
caches. Independent teardown checks compare full expected output outside the
measurement; successful exit alone does not prove that the work ran.

Four component cases share generated literal input with 2,000 nested bindings:

| Filter suffix | Command |
|---|---|
| `frontend::parse` | `parse -q`: read and parse |
| `frontend::ast_json` | `parse --json`: parse and render the syntax tree |
| `render::json` | `eval --json`: compile, evaluate, force and render JSON |
| `render::nix` | `eval --strict`: compile, evaluate, force and render Nix syntax |

```sh
nix run .#bench-callgrind -- --list
nix run .#bench-callgrind -- 'eval::components::*' --save-summary=json
```

This Rust harness registers executable commands, generates fixed input in a
sandbox, and checks outputs. Gungraun supplies collection, baselines, comparison,
regression detection, aggregation and reporting. Its binary defaults count all
executed threads from process startup and clear the measured environment.
Setup/teardown are outside that boundary. The production allocator is retained;
`std::process::exit` does not run stack destructors. Differences between whole
commands are not exact phase subtractions, and instructions do not establish
elapsed time, syscall cost or parallel scaling.

Raw profiles and JSON summaries are retained under `target/gungraun/<target-triple>`.
`--show-intermediate` exposes process/thread fragments as well as totals.
`--output-format=json` emits machine-readable summaries directly; no extra
reporter is needed. Use raw additive metrics such as `Ir` for gates. Aggregate
derived cache percentages can incorrectly sum per-thread rates; raw events and
individual profiles remain available for interpretation. Simulated cache/branch
events and `EstimatedCycles` are models, not native hardware measurements.

For regular/differential instruction and total-access flamegraphs, filter one
case and enable the small API configuration in the harness:

```sh
IETS_BENCH_FLAMEGRAPHS=1 nix run .#bench-callgrind -- '*::components::frontend::parse' \
  --save-baseline=graphs --save-summary=json
# After the change:
IETS_BENCH_FLAMEGRAPHS=1 nix run .#bench-callgrind -- '*::components::frontend::parse' \
  --baseline=graphs --save-summary=json
```

The runner exposes flamegraphs through its Rust API rather than a CLI option.
SVGs are opt-in because expanding recursive graphs can use substantial memory.
`TotalRW` includes instruction fetches. Raw profiles can also be opened with
`callgrind_annotate` or KCachegrind without generating SVGs.

Additional built-in tools and metrics are available:

```sh
# Branch predictor simulation:
nix run .#bench-callgrind -- '*::evaluation::*::attrs' \
  --callgrind-args=--branch-sim=yes --callgrind-metrics='@default,@bs'
# Native perf counters and DWARF call stacks (requires event access):
nix run .#bench-callgrind -- '*::evaluation::*::attrs' \
  --default-tool=perf --perf-events=instructions:u --perf-record \
  --perf-record-args=--call-graph=dwarf --save-summary=json
# Instrumented syscall diagnostics, not native syscall latency:
nix run .#bench-callgrind -- '*::evaluation::*::attrs' \
  --callgrind-args=--collect-systime=nsec --callgrind-metrics='Ir,SysCount,SysTime,SysCpuTime'
```

`--tools=perf` adds a separate native execution alongside Callgrind;
`--default-tool=perf` selects perf instead. A failed/unsupported hardware event
is not a simulated measurement. Callgrind serializes threads and perturbs
scheduling, syscall arrival and elapsed time. `Ir` excludes kernel instructions;
Gungraun does not compensate instrumented syscall timings into native costs.

For Nix-expression and allocation context, use the evaluator's existing
[`IETS_PROFILE=eval|alloc`](../reference/profile.md), `IETS_STATS`, `IETS_PAR_STATS`
and `eval --timings`. Pass settings through a cleared Gungraun environment with
`--envs=IETS_STATS=1 --nocapture=stderr`. Keep diagnostics separate from regression
baselines. The evaluator profiler pins evaluation to one worker; generic heap
tools do not classify objects inside its arenas automatically.

`IETS_BENCH_BIN` selects an existing executable; `IETS_BENCH_WORKLOADS` overrides
the installed fixture directory. `GUNGRAUN_HOME` changes the output location.
Keep harness, fixtures, compiler, collection settings and hardware identical
for both sides. Recreate both baselines after changing those inputs. The wrapper
separates target architectures by default; local Cargo runs should pass
`--separate-targets`. `--help` lists the runner's remaining options.

`nix develop .#bench` supplies the toolchain and matching runner. Local Cargo
usage is `cargo build --release --bin iets`, then
`cargo bench --manifest-path bench/Cargo.toml --bench eval`. The standalone lock
and Crane fileset isolate benchmark dependencies from the production package.
Nix builds the runner from that same graph. Profiling Valgrind carries the
[upstream auxiliary-vector fix](https://sourceware.org/cgit/valgrind/commit/?id=de9c4730516522bbc02b55caf4c4df376dd0f169)
needed for rustix on recent Linux until nixpkgs includes it.

`nix build .#checks.aarch64-linux.bench-smoke` (or `x86_64-linux`) exercises all
cases natively with Gungraun's `--test` and then under Valgrind. Build-time counts
are discarded. CI measures outside derivations, using the candidate's locked
inputs for both executables. It publishes the runner's instruction/thread
output directly and uploads JSON/raw profiles. The baseline may have stripped
debug information; retain debug information on both sides for source annotation.

For an unbounded whole-Nixpkgs evaluation, explicitly disable the default 4 GiB
worker-process restart threshold. `--max-memory-size 0` selects one shared
process without a ceiling; `--workers` controls evaluator threads:

```sh
nix build .#default -o result
np=$(nix eval --raw --impure --expr '(builtins.getFlake (toString ./.)).inputs.nixpkgs.outPath')
printf 'import %s/ci/eval/outpaths.nix { path = %s; systems = [ "x86_64-linux" ]; }\n' "$np" "$np" > outpaths.nix
./result/bin/iets eval-jobs --impure --no-instantiate --no-cache \
  --max-memory-size 0 --workers 8 -I "nixpkgs=$np" ./outpaths.nix > jobs.jsonl
```

Compare records independently of line order, excluding diagnostic `stats` but
retaining errors. Record system load, peak RSS and swapping alongside time.
Removing the evaluator ceiling does not remove inherited OS limits or pressure.

Workloads follow the upstream native runner in [#406](https://github.com/numtide/iets/pull/406),
server/Plasma measurements in [#371](https://github.com/numtide/iets/pull/371) and
[#372](https://github.com/numtide/iets/pull/372), and call-path investigations in
[#325](https://github.com/numtide/iets/pull/325) and [#330](https://github.com/numtide/iets/pull/330).
The small synthetic fixtures are new bounded checks.
