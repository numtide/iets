# Benchmark harness boundaries and audit

The harness uses Gungraun 0.20.0 as resolved by its standalone Cargo lockfile.
The implementation was checked against the matching
[binary benchmark runner](https://github.com/gungraun/gungraun/blob/v0.20.0/crates/gungraun-runner/src/runner/bin_bench.rs),
[tool configuration](https://github.com/gungraun/gungraun/blob/v0.20.0/crates/gungraun-runner/src/runner/tool/config.rs),
and [output API](https://docs.rs/gungraun/0.20.0/gungraun/struct.OutputFormat.html#method.show_intermediate),
then exercised through the installed Nix commands. Usage is in the
[benchmark how-to](../how-to/benchmark.md).

## What is counted

Gungraun's binary default is `EntryPoint::None`; the harness makes that explicit.
It measures the actual executable from startup through its executed exit path,
with its production mimalloc allocator. `iets` calls `std::process::exit`, so
this does not imply that stack destructors or every possible session cleanup run.
The harness and its setup/teardown processes are outside the measured executable.
Fixture generation and output checks therefore do not enter the instruction count.

Gungraun enables child tracing and separate thread profiles by default. Its
summary and regression checks aggregate the fragments, while
`--show-intermediate` exposes each fragment. Each raw thread profile is retained.
The native suite supplies elapsed time, RSS and worker scaling; instruction counts
cannot establish speedup, blocking cost or parallel throughput.

The independent output checks matter: a successful exit alone does not establish
that a workload ran. The four evaluation cases compare full expected JSON;
component teardown checks parser success, AST bindings, every JSON entry, and the
complete Nix rendering. An empty-output executable and an executable returning
`{}` both fail these checks despite returning exit status zero.

## Visibility choices

The original evaluation fixtures intentionally include Nix-level workload
construction. Their hotspots include calls and string interpolation used to
construct the inputs, not only the attribute or derivation operation in a name.
Four component cases use identical, generated literal input containing 2,000
nested bindings. They expose parsing, AST rendering, JSON value output and strict
Nix value output without dynamic input generation. Each is a whole command;
differences between commands are not precise phase subtractions.

Raw Callgrind profiles are always collected; `--save-summary=json` saves structured
JSON summaries as well. SVGs are opt-in with
`IETS_BENCH_FLAMEGRAPHS=1` and a focused filter. They combine all thread fragments
for `Ir` and `TotalRW`, which includes instruction fetches. Differential SVG
creation on the whole suite consumed about 25 GB RSS and was stopped during the
audit; this is why CI does not generate SVGs automatically. The focused parser
baseline/comparison generated regular and differential SVGs successfully.

`bench-report` checks additive fragment totals and calculates cache miss rates
from raw counts. In Gungraun 0.20, summed derived percentages in JSON totals can
be invalid: an observed three-thread profile reported `L1HitRate=289.8%`.
The report recomputes `I1mr / Ir` and `(D1mr + D1mw) / (Dr + Dw)` instead of
using those aggregate percentages. These are still simulated cache rates.

The runner's `runner` Cargo feature includes perf support; there is no missing
separate perf feature. `--tools=perf` adds a separate native execution; it does not
estimate hardware counters from Callgrind. Perf-only collection and native
`perf.data` recording with DWARF stacks were also exercised. Tool argument values
use equals syntax: `--perf-record-args=--call-graph=dwarf`.

Callgrind's syscall collection is available with `--collect-systime=nsec` and
`SysCount`, `SysTime`, `SysCpuTime`. Those timings describe an instrumented run,
not native syscall latency. Gungraun does not compensate instrumentation or
serialized scheduling back into native timing. `Ir` excludes kernel instructions.

Library benchmarks can isolate a Rust API more closely, and Gungraun supports
custom entry points and client requests. This suite retains the executable
boundary so the allocator, CLI and actual process lifecycle stay representative.
A library entry-point toggle would also need care with evaluator worker threads.
No instrumentation dependency or profiling branch was added to the evaluator.
CI gates instructions on all eight cases; filters are available for focused
iteration and graph generation.
DHAT/Massif and the error tools are available through Gungraun, but generic heap
profiles do not automatically expose object kinds inside mimalloc and arenas.
For that semantic view, use the evaluator's `IETS_STATS` and Nix-level
[`IETS_PROFILE=eval|alloc`](../reference/profile.md) outside count gates. The
program profiler pins evaluation to one worker and does not observe all frontend
work or the inside of every long builtin.

## Validation evidence

The November 11, 2026 audit used the Nix-packaged tools and unmodified release
evaluator. Actual ARM runs exercised all eight cases both directly and under
Callgrind. The final packaged smoke also passed on actual x86 through the remote
Nix builder, including every component check. An eight-case saved-baseline A/A comparison passed the 5% instruction
gate; seven counts matched exactly and the remaining drift was under 0.001%.
The component counts on that ARM build were:

| Command | Aggregate instructions |
|---|---:|
| Parser, quiet | 13,602,332 |
| Parser, AST JSON | 65,216,271 |
| Evaluation, JSON value | 27,747,183 |
| Evaluation, strict Nix value | 29,076,663 |

The parser profile attributes 24% of worker-thread self instructions to
`Lexer::next_token_raw`; the AST JSON profile shows substantial copying and string
formatting. These are concrete investigation leads, not demonstrated avoidable
costs or evidence that they dominate NixOS evaluations.

A separate native ARM follow-up repeated the locked NixOS server evaluation 20
times at one and four workers, recording user cycles with DWARF stacks. All
outputs matched and perf reported no lost samples. Weighted across the host's
two PMUs, single-worker self cycle shares included SHA-256 compression (5.6%),
string interning (4.8%), function application (4.6%), plain-thunk execution
(4.5%) and attribute lookup (4.5%). Four-worker profiles additionally highlighted
scheduler entry points, atomics and thunk waiting. Scheduler symbols also include
inlined work and teardown; their whole shares are not proven queue overhead.
These are native cycle samples, distinct from the synthetic Callgrind instruction
shares, and do not establish a worker speed comparison.

Separate semantic diagnostics on that server imported 3,574 files and allocated
556 MB of arena objects at one worker, including 105 MB of IR, 98 MB of
environments and 96 MB of plain thunks. Four workers allocated 568 MB and created
6.26 million plain thunks versus 5.98 million at one worker. One four-worker run
reported 119,727 tasks and 3,439 fiber parks. These observations motivate checking
repeated work, allocation volume and synchronization together; allocation totals
are not live heap measurements, and one diagnostic run is not a stable average.
The local audit artifacts under `target/benchmark-audit-20261111/` retain raw
profiles, diagnostics and `native-server-analysis.md` with the sampling scope.

The totals equalled the sum of every per-thread fragment. A deliberate fourfold
call-workload increase preserved the expected result but raised instructions by
286.6%; the 5% gate rejected it with exit code 3. A report input with
one instruction added only to the total was rejected. These A/A results measure
repeatability, not a performance improvement.

Native perf collection, DWARF recording, branch simulation, syscall diagnostics
and explicit `IETS_STATS=1` propagation were tested separately. The latter
reported object counts, arena allocation and mapping sizes as intended. A focused
parser comparison produced six valid SVG files: baseline/current/differential
for each of two event kinds. The expensive whole-suite graph experiment was
terminated, and the default suite produces no SVGs.

All cases use a fixed Nix derivation `system` where needed for output parity;
that value does not select the machine-code architecture. Target-separated output
paths prevent automatic ARM/x86 baseline mixing. Changing compiler, fixtures,
collection options or measurement boundaries requires fresh baselines for both
executables. Native workload comparisons still require the same hardware and
machine conditions.
