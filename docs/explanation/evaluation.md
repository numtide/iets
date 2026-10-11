# How iets evaluates

iets doesn't walk the syntax tree. It compiles your code to an intermediate
representation first, and it evaluates that lazily, as Nix does, but with
several threads forcing thunks at once. This page explains how both work, and
why the memory layout decides how big a run gets.

## The pipeline

```
source bytes
  -> syntax::lexer / syntax::parser   -> ast::Ast (per file, arena of Expr)
  -> compile                          -> ir::Ir (scopes resolved, attrpaths merged)
  -> eval                             -> runtime::Word (lazy graph, forced on demand)
  -> print / json / drvPath
```

The parser keeps raw bindings. Attribute-path merging and scope resolution
happen in the compile step, so the evaluator never searches a scope by
name: every variable is already a `Local { depth, index }`, or a `WithVar`
when no lexical binder exists. Variable-length node payloads live in side
tables addressed by a range, so a file costs a handful of allocations
instead of one per node.

## Thunks

Every argument, attribute value, list element and `let` binding that is
not cheap becomes a thunk. A cheap expression, like a literal or a
variable, reuses the existing word or cell. So a variable that holds a
thunk shares that thunk pointer instead of allocating a second one.

Forcing overwrites the thunk object in place. A thunk is two words. `code`
holds the IR or the function, and becomes the value. `state` holds the
environment or the argument, and becomes empty. Cells that still point at
the thunk get the value written over them as they are read. That is graph
reduction, and it is why a shared subexpression is evaluated only once.

`mapAttrs` is the case where even two words were too many. On the outpaths
workload there were 22 million `mapAttrs` thunks and 93 % of them were
never forced. A `mapAttrs` entry has no thunk object at all: a
`THUNK_MAP` word carries a descriptor pointer and an entry index, and one
`MapDesc` per call holds an 8-byte state cell per entry. Sets over 65,536
entries fall back to an ordinary two-argument thunk. That change alone took
the arena from 8,936 MB to 7,981 MB, with the run time inside the noise.

## Values

A `Word` is one 8-byte word with a 3-bit tag in the low bits.

| tag | meaning |
|---|---|
| `INT` | 61-bit immediate integer; a larger integer boxes to a heap object |
| `PTR` | pointer to a heap object with a one-byte kind header |
| `THUNK`, `THUNK_APP`, `THUNK_APP2` | pointer to a 16- or 24-byte thunk object |
| `THUNK_MAP` | a `mapAttrs` entry: descriptor pointer plus index, no object |
| `LAMBDA` | pointer to a two-word closure |
| `IMM` | immediates: `NULL`, `TRUE`, `FALSE` and `BLACKHOLE` (a thunk under evaluation) |

The heap kinds behind a `PTR` are `Int` (a boxed integer), `Float`,
`String { bytes, context }`, `Path`, `List`, `Attrs` (sorted by symbol id),
`AttrsUnion` (the result of `//`), `Lambda { env, ir }`, `PrimOp`,
`PrimOpApp` and the thunk kinds. Environments, `Env { up, cells }`, are heap
objects too.

A `Cell` is an `AtomicU64` holding a `Word`. Environment slots, list
elements and attribute values are all cells, so the sequential path and
the parallel path share one shape. There is no "parallel value" and no
"sequential value", only a cell read by one thread or by four. Cells that a
value is read from never hold a blackhole. Only a thunk's own state cell
does.

Attributes are sorted by symbol id, as they are in Nix. Name order, which
`attrNames`, printing, JSON and a derivation environment need, is computed
on demand.

### String context

A string value carries a context: `Opaque(store path)`, `DrvDeep(drv path)`
or `Built { drv path, output name }`. Context flows through `+`, through
interpolation and through `concatStringsSep`. `derivationStrict` is where
it pays off: it turns the context of every attribute into the derivation's
`inputDrvs` and `inputSrcs`. That is how a derivation learns its
dependencies without you declaring them.

## `with` scopes

`with` is the one construct the compiler cannot resolve statically. The
evaluator keeps a chain of `with` scopes in the environment, and a
`WithVar` walks that chain innermost first. A lexical binding always wins,
which is why `WithVar` is emitted only when no lexical binder exists at
all.

An evaluation result is `Result<Word, Box<Error>>`. `tryEval` catches
`Throw` and `Assert` and nothing else. A lint raised at level `error` is a
different kind, so it escapes, and so does the cancellation a bailed helper
fiber gets. An error carries a message, a span and a trace. Each IR node
keeps its position, for `unsafeGetAttrPos`, lambda positions and error
messages.

## The `//` operator

When the left side has at least eight entries, `a // b` is not a merged
copy. It is a union object: a tail with the flat leaves of both sides, at
most eight, lowest priority first. A lookup probes the leaves from the
last. An iteration merges them as it goes. A consumer that needs one
contiguous slice asks for a flat copy, which is made once and cached.

The merge is a loser tree over the leaf heads, so emitting one attribute
costs a few comparisons instead of a scan of every head. Measurement decided
on a union: on outpaths, `//` copied 5.12 GB, and 80 % of the results were
never iterated at all. The union took the arena from 13,253 MB to 8,936 MB.

## Shared strings

The arena keeps a table of string bytes by 64-bit content hash, the
context-free string object over them, and a set of context slices. A string
without context shares the whole object. A string with context shares the
bytes. Interpolation joins its pieces in the arena and gives them back when
the content already exists.

On outpaths, 5.93 million of 7.31 million strings repeated bytes that were
already there. That is 662 MB. Sharing them took the arena from 7,981 MB to
7,106 MB, for +3 % wall at one worker and −7 % at four. Keying the table by
the bytes instead of by their hash cost 6 % and 12 %, so the hash stayed.

## ATerm text

A derivation's ATerm text is kept only for the runs that read it back:
instantiation, the tracing cache, `build` and `print-dev-env`. Everything
else handles a derivation with no ATerm, which is also the state of a
derivation read from `/nix/store`. It was 331 MB of heap on outpaths, and
dropping it took peak RSS from 7,680 MB to 7,219 MB.

## Memory

There is one bump arena per worker and nothing is ever freed. Every object
lives until the process exits, so object layout, not allocation speed,
decides peak memory. That is why the packing changes on this page (the
`mapAttrs` thunks, the union, shared strings and dropped ATerms) were worth
their complexity. Together they changed:

| | before | after | Nix for scale |
|---|---|---|---|
| desktop arena | 937 MB | 629 MB | |
| desktop peak RSS, 1 worker | 988 MB | 659 MB | 690 MB (`nix-instantiate`) |
| outpaths arena | 13,253 MB | 7,106 MB | |
| outpaths peak RSS, 1 worker | 13,495 MB | 7,219 MB | 4.8 GB per nix-eval-jobs worker, restarted at that size |
| outpaths wall, 1 / 4 workers | 32.7 / 15.2 s | 32.5 / 13.5 s | |

What is left is thunks, frames and attribute sets the program can still
reach. No layout change reduces that. Only a collector or a restart bounds
it, which is why `eval-jobs` restarts its worker processes at
`--max-memory-size` instead.

A census of the outpaths run after those changes priced several more ideas
and built none of them. They are in
[What was tried and removed](history.md).

## The fiber scheduler

You don't ask for parallel evaluation. It happens on its own: worker threads
force thunks ahead of the demand path and beside it. The parts are:

- A **fiber** is a stackful coroutine with its own stack and its own
  evaluator. It is the unit of blocking, and it moves: a fiber that parks
  on worker A can resume on worker B.
- A **worker** is an OS thread running a drain loop. It owns a bump arena,
  a fiber pool and work-stealing deques.
- A **task** is work not yet started, such as forcing a range of list or
  attribute cells. It runs in a pooled fiber.
- The **main fiber** runs the command's own closure. When it returns,
  evaluation is done.

Each fiber's evaluator is a clone of the root one. Per-fiber state is
private. Shared state (the interner, the file table, the import cache, the
derivation table) sits behind handles that clone by reference. A fiber's
arena field is swapped with the worker's arena on every resume, and swapped
back on every yield. So N workers own N arenas, and no fiber ever allocates
from an arena another thread is using. Pointers are global, and cross-arena
references are fine, because no arena is ever freed.

`parallel::try_run` clones the evaluator as the template before it starts
the workers, so every setting must be in place by then. A setting written
after the clone reaches the main fiber only, with silent symptoms: partial
output, a cache that no worker reads, a search path that resolves to
nothing. So the evaluator has no setter: `eval::EvalConfig` holds each
setting fixed, and `build()` makes the evaluator.

### Claiming thunks

The thunk's *state* cell is the synchronisation word. Its three states are
disjoint: a payload, a blackhole, or empty.

```
payload ──CAS──> BLACKHOLE(claimer, waiters bit) ──store──> EMPTY
   ^                          │
   └────────── reset ─────────┘   (error, cancel, or traced-in-helper)
```

A force loads the word first, and tries the compare-exchange only on a
payload word. A failed compare-exchange still takes the cache line
exclusive, and most forces hit a thunk another fiber already evaluated:
everything in `lib`, everything in `pkgs`. The plain load keeps those hot
lines shared.

The winner gets the payload back and evaluates. A loser reads the current
word. If it is done, it returns the value. If it is a blackhole holding its
*own* claimer id, that is infinite recursion. The id is per fiber, not per
thread, so it follows a computation that moved. If another fiber holds
it, the loser parks on a side table sharded by thunk address, so thunk
objects never grow.

### Scaling limits

The default is four workers. What limits it is the critical chain, not
contention:

| workload | workers | wall | user | sys |
|---|---|---|---|---|
| nixos-desktop | 1 | 1.67 s | 1.44 s | 0.21 s |
| nixos-desktop | 4 | 1.01 s | 2.37 s | 0.31 s |
| nixos-desktop | 16 | 1.27 s | 5.88 s | 0.74 s |
| nixos-minimal | 1 | 1.32 s | 1.16 s | 0.15 s |
| nixos-minimal | 4 | 0.89 s | 1.95 s | 0.27 s |
| nixos-minimal | 16 | 1.18 s | 4.86 s | 0.59 s |

At eight workers the main fiber spends about 0.95 s of 1.22 s parked while
helpers walk one chain. The chain is the module fixpoint in
`lib/modules.nix`, which has no strict consumer to fan out from, plus
imports: on nixos-minimal at four workers one helper spends about 0.17 s
alone reading and compiling the 16.6 MB `hackage-packages.nix` while
everyone else waits. Only parse and compile speed move that phase, which is
what the IR cache does on a warm run (see
[How caching works](caching.md)).

## Determinism

These properties give the same answer:

- **Forcing is pure.** The value graph is the same whichever fiber forces a
  thunk.
- **A thunk is published at most once per successful evaluation.** A reset
  followed by a re-evaluation yields the same value, because the input did
  not change.
- **Attribute order, list order and derivation hashing do not depend on
  scheduling.** They are decided by symbol ids and by the ATerm, both of
  which are computed from the program.

### Speculation can't change the result

A helper computes ahead of the demand path, so what it computes must leave
no trace the program can read. The places that could show it are closed:

- **The cell.** Only the demand path unboxes a forced cell. A helper
  forces the thunk and leaves the cell alone (`Evaluator::force_cell`,
  `parallel::force::take`). A cell that holds a value is therefore a
  cell the program asked for, which is what `==` reads for the identity of
  a function and what the printers read for `<CODE>`.
- **The thunk state.** A helper publishes `SPECULATED` where the demand
  path publishes `EMPTY` (`Word::thunk_finish`). Both say "forced"; only
  the demand path's mark makes the value printable. The first demand of a
  speculated thunk upgrades the mark.

The demand path pays for this guarantee. A helper leaves its value inside
the thunk, so the demand path reads the cell, the thunk state and the value,
writes the value into the cell, and writes `EMPTY` over the helper's mark.
Against the same tree with helper unboxing put back, this costs 3.2 % of the
instructions on nixos-desktop at four workers. At one worker there is no
helper, so it costs nothing.

A lint finding is not a value and is held back instead. One a helper
produced is parked on the thunk the helper was forcing
(`parallel::force::defer`). The demand path runs it when it takes that
thunk's value and drops it otherwise. Cancelling the helper, as `trace` does, was measured at 15 % of the wall
for lints, and turning the fan-out off in the builtins that stop early costs
37 % at four workers, so neither is the answer. A finding whose value the
demand path reaches through a different thunk than the helper's first claim
is dropped, which is a lost warning and never a wrong one.

A `--dump-drvs` file is written where the derivation is made. A parked dump goes missing while the `drvPath` that names it is
returned.

### Failed thunks

Nix evaluates a failed thunk again at every force, and so does iets: a
failure resets the thunk. Under fan-out that turned one failing thunk into
one failing evaluation per task, which was measured at 20 s and 11 GB
against 0.01 s and 38 MB at one worker. The runtime notes the thunk that
failed in a small direct-mapped table. A helper that meets it drops its
task, so only the demand path evaluates it once more and reports. A run that
fails nothing never reads the table.

### Stack limits

The demand fiber has the stack of the process main thread
(`fiber::MAIN_STACK`), so a program that works at one worker is not a
crash at four. A helper's stack is smaller, and the force path and the
deep walk stop a fiber that comes within `fiber::MARGIN` of its guard
page. A helper drops its task in silence and the demand path computes the
value, so the demand fiber reports what Nix reports. A guard page can still
be hit, for example when the compiler recurses over a deeply nested
expression without forcing. A handler on an alternate signal stack catches
it, prints Nix's `stack overflow (possible infinite recursion)` and exits 1.

### `trace` and `warn`

`builtins.trace` and `builtins.warn` would break purity in a helper, so
the helper gives them back to the demand path. In a helper fiber they
print nothing: the helper sets a sticky cancel flag and abandons the task.
Every thunk still open in that fiber is reset, and because a cancelled
force returns an error, no value is written back into any list, attribute
or environment cell. The demand path then evaluates that thunk itself and
prints, exactly once. Journalling the warning and computing on loses it,
because the write-back happens and the demand path never re-evaluates.

Cancel is sticky for the same reason. Once set, every publish the fiber
would still do becomes a reset, so a swallowed error cannot leak a value
built from a bailed sub-computation into a shared thunk.

Trace lines on stderr can come in another order than in a sequential run,
because the demand walk does every unclaimed cell first and parks on busy
ones afterwards. A helper starts at call depth zero, so a program that
fails only by exceeding the maximum call depth sequentially may succeed in
parallel. Both are listed in
[Known divergences from Nix 2.34](../reference/divergences.md#parallel-evaluation).

### Deadlocks

Fan-out can claim the two halves of a cycle on two fibers, which then wait
on each other. Sequential evaluation would have reported infinite recursion
instead. When every worker sleeps, nothing is pending and the main fiber is
not done, the last worker to go to sleep cancels every parked helper fiber.
Each one unwinds, every thunk it claimed is reset, its waiters wake, and the
main fiber re-evaluates the cycle sequentially and reports the error the
sequential path would have reported.

## Speed

The numbers are in the README's [Benchmarks](../../README.md#benchmarks)
section, and the raw results are in `bench/results/`. `bench/run.nu`
measures them again.

iets runs fewer instructions than Nix and gives the same derivation paths.
Part of that comes from the compiled-file cache: a second evaluation skips
the files it already compiled. `--no-ir-cache` turns it off, to compare the
evaluators alone.

More workers finish an evaluation sooner, but they spend instructions to
coordinate. So the gain in wall time depends on the machine more than the
gain in instructions does.

### What the benchmarks leave out

Our benchmarks don't show the full picture. They time two NixOS systems,
`nixos-minimal` and `nixos-desktop`. Both are module-system evaluations
of nixpkgs through `NIX_PATH`. Don't let the name fool you: `nixos-desktop`
has a few services and command-line packages, and no desktop. So the
numbers tell you how iets does on one kind of work, and that kind suits it.

[fix](https://github.com/psyclyx/fix)'s benchmark suite covers more: synthetic
hot paths, JSON output of wide trees, and several configurations on one
command line. We ran iets 0.1.0 (a48ee8d7) through it on 2026-10-11, with
no caches on either side. iets was faster than Nix on nineteen of twenty
workloads, and slower than fix on six:

| workload | fix | iets | why |
|---|---|---|---|
| shared-mfe | 13 ms | 109 ms | fix moves work that doesn't depend on a function's argument out of the function; iets does it on every call |
| wide-call-tree | 17 ms | 82 ms | fix reuses the result of the same call; iets computes `fib 22` 96 times |
| string-churn | 363 ms | 616 ms | every new string goes into the string table, which grows without limit; iets is slower than Nix (424 ms) here too |
| module-option-shape | 13 ms | 20 ms | partly startup: four workers start about 2 ms slower than one |
| math-heavy | 722 ms | 900 ms | `+` on integers goes through the general concatenation path |
| fixpoint-heavy | 18 ms | 22 ms | partly startup, as above |

All nineteen of its fixtures give the same JSON under iets as under Nix.

Two more things to keep in mind when you read a speed claim. With the
tracing cache on, a repeated evaluation doesn't evaluate at all: it
replays the result, about 13 ms on every one of fix's synthetic workloads.
That measures the cache, not the evaluator, so compare it only with
another result cache. And the default of four workers makes a sequential
evaluation slower, not faster: string-churn took 0.66 s with four workers
and 0.52 s with one.

### Search speed

`iets search` caches an index of the packages it finds. These numbers are
wall time, median of three, on 2026-09-23, on a 16-core EPYC shared with
other work, with a release build. The input was the pinned nixpkgs, loaded
as
`{ legacyPackages.${builtins.currentSystem} = import <pin> {}; }`:

| run | `^hello$` | all packages (`^`) |
|---|---|---|
| iets, no index yet | 6.5 s | 6.8 s |
| iets, index cached | 0.15 s | 0.47 s |
| `nix search`, the same installable | 21 s | 21 s |
| `nix search`, the same tree as a flake, its cache warm | 1.8 s | 2.6 s |
| `nix search`, the same tree as a flake, its cache cold | 37 s | — |

Nix caches a flake installable only, and evaluates `--expr` and `--file`
every time. The `^ --json` output is the same byte for byte on both sides.

## Unproven claims

ThreadSanitizer needs a nightly compiler and hasn't been run, and there is no
model checker. The invariants above are the argument, and the tests
are the evidence: a claim-and-park stress test on a shared graph from eight
workers, the deadlock resolution, the scheduler handshake, a set of
expressions compared at one against N workers, the Nix language suite at
eight workers, and the drvPath comparison against real `nix-instantiate` at
eight workers. The aarch64 context switch is written but untested.
