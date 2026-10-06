# How iets evaluates

iets is a tree-walking evaluator over a compiled intermediate
representation. It is lazy, like Nix, and it forces thunks on several
threads at once. This page explains why those two properties can hold
together, and why the memory layout is the thing that decides how large a
run gets.

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
tables addressed by a range, so one file costs nine allocations instead of
one per node.

## Laziness, and what a thunk costs

Every argument, attribute value, list element and `let` binding that is
not cheap becomes a thunk. A cheap expression — a literal, a variable —
reuses the existing word or cell, so a variable that holds a thunk shares
that thunk pointer instead of allocating a second one.

Forcing overwrites the thunk object in place. A thunk is two words: `code`
holds the IR or the function and becomes the value; `state` holds the
environment or the argument and becomes empty. Cells that still point at
the thunk are rewritten with the value as they are read. That is graph
reduction, and it is why a shared subexpression is evaluated once.

`mapAttrs` is the case where even two words were too many. On the outpaths
workload there were 22 million `mapAttrs` thunks and 93 % of them were
never forced. A `mapAttrs` entry now has no thunk object at all: a
`THUNK_MAP` word carries a descriptor pointer and an entry index, and one
`MapDesc` per call holds an 8-byte state cell per entry. Sets over 65,536
entries fall back to an ordinary two-argument thunk. That change alone took
the arena from 8,936 MB to 7,981 MB, with the run time inside the noise.

## The value model

A `Word` is one 8-byte word with a 3-bit tag in the low bits.

| tag | meaning |
|---|---|
| `INT` | 61-bit immediate integer; a larger integer boxes to a heap object |
| `PTR` | pointer to a heap object with a one-byte kind header |
| `THUNK`, `THUNK_APP`, `THUNK_APP2` | pointer to a 16- or 24-byte thunk object |
| `THUNK_MAP` | a `mapAttrs` entry: descriptor pointer plus index, no object |
| `LAMBDA` | pointer to a two-word closure |
| `BLACKHOLE` | thunk under evaluation |
| `NULL`, `TRUE`, `FALSE` | immediates |

The heap kinds behind a `PTR` are `Float`, `BigInt`,
`String { bytes, context }`, `Path`, `List`,
`Attrs` (sorted by symbol id), `AttrsUnion` (the result of `//`),
`Lambda { env, ir }`, `PrimOp`, `PrimOpApp`, `Env { up, cells }` and
`Thunk { code, state }`.

A `Cell` is an `AtomicU64` holding a `Word`. Environment slots, list
elements and attribute values are all cells. That decision lets the
sequential path and the parallel path share one shape: there is no
"parallel value" and no "sequential value", only a cell that happens to be
read by one thread or by four. Cells that a value is read from never hold a
blackhole; only a thunk's own state cell does.

Attributes are sorted by symbol id, as they are in Nix. Name order — what
`attrNames`, printing, JSON and a derivation environment need — is computed
on demand.

### Strings carry context

A string value carries a context: `Opaque(store path)`, `DrvDeep(drv path)`
or `Built { drv path, output name }`. Context flows through `+`, through
interpolation and through `concatStringsSep`. `derivationStrict` is where it
is cashed in: it turns the context of every attribute into the derivation's
`inputDrvs` and `inputSrcs`. That is how a derivation learns its
dependencies without anyone declaring them.

## Scopes and errors

`with` is the one construct the compiler cannot resolve statically. The
evaluator keeps a chain of `with` scopes in the environment, and a
`WithVar` walks that chain innermost first. A lexical binding always wins,
which is why `WithVar` is emitted only when no lexical binder exists at
all.

An evaluation result is `Result<Word, Box<Error>>`. `tryEval` catches
`Throw` and `Assert` and nothing else: a lint raised at level `error` is a
different kind and escapes it, and so does the cancellation a bailed helper
fiber gets. An error carries a message, a span and a trace, and position
information is kept per IR node — for `unsafeGetAttrPos`, for lambda
positions and for error messages.

## `//` builds a union, not a copy

When the left side has at least eight entries, `a // b` is not a merged
copy. It is a union object: a tail with the flat leaves of both sides, at
most eight, lowest priority first. A lookup probes the leaves from the
last. An iteration merges them as it goes. A consumer that needs one
contiguous slice asks for a flat copy, which is made once and cached.

The merge runs a loser tree over the leaf heads, so emitting one attribute
costs one root path — three comparisons at eight leaves — instead of a scan
of every head to pick the minimum and a second scan to advance it. Equal
syms are ordered by leaf, last leaf first, so the winner is the entry the
`//` gives and the duplicates behind it are dropped as they surface. The
flat copy is sized at the sum of the leaf lengths, filled in one merge pass
and then handed the tail the duplicates left back at the bump pointer;
sizing it exactly would mean merging twice.

On nixos-desktop the merge ran 8.4 million
times, 4.9 million of them over six to eight leaves, where a rescan cost 16
head checks per attribute. The tree took the run from 10.03 G to 9.74 G
instructions, and the merge itself from 7.1 % to 4.5 % of it.

The reason is measurement, not taste. On outpaths, `//` copied 5.12 GB, of
which 3.94 GB was left sides, and 80 % of the results were never iterated
at all. The union took the arena from 13,253 MB to 8,936 MB. An earlier
version that kept recursive `(a, b)` pointers instead of inline leaves cost
3.8 % of wall time; inline leaves cost nothing.

## Strings shared by content

The arena keeps a table of string bytes by 64-bit content hash, the
context-free string object over them, and a set of context slices. A string
without context shares the whole object; a string with context shares the
bytes. Interpolation joins its pieces in the arena and gives them back when
the content already exists.

On outpaths, 5.93 million of 7.31 million strings repeated bytes that were
already there — 662 MB. Sharing them took the arena from 7,981 MB to
7,106 MB, for +3 % wall at one worker and −7 % at four. Keying the table by
the bytes instead of by their hash cost 6 % and 12 %, so the hash stayed.

## ATerm text kept only when something reads it

A derivation's ATerm text is kept only for the runs that read it back:
instantiation, the tracing cache, `build` and `print-dev-env`. Everything
else handles a derivation with no ATerm, which is also the state of a
derivation read from `/nix/store`. It was 331 MB of heap on outpaths, and
dropping it took peak RSS from 7,680 MB to 7,219 MB.

Building it is on the other side of the same argument. Each derivation is
unparsed three times — masked for the output paths, plain for the drvPath,
and again against the input hashes — so a nixos-desktop run writes about
20,000 ATerms. They are written into one buffer per thread that keeps the
capacity it grew to, so only the first few pay for growing it, and the
store paths, hashes and method names go into that buffer directly instead
of through a `String` each. The evaluator's own values live in the arena,
but this pipeline is ordinary `String` and `BTreeMap` work, and it was
9.3 % of a warm run in glibc `malloc` and `free` alone.

## Why there is no garbage collector

There is one bump arena per worker and nothing is ever freed. Every object
lives until the process exits. This is why object layout, not allocation
speed, decides peak memory, and it is why the four packing changes above
were worth their complexity.

Four changes, measured from commit `e6b8971` to `155cbdf`:

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

A census of the outpaths run after those four changes named six more ideas
and priced them all out: attribute positions in a side table (a 16-byte
attribute pads back to 16 without a struct-of-arrays rewrite), compiling
`hackage-packages.nix` lazily (~15 MB, but the AST stays alive), flattening
`let` frames (under 56 MB), a 24-byte string header (43 MB), one shared
closure for lambdas that capture nothing (16 MB), and one shared empty
attribute set (22 MB). Scoped release without a collector was ruled out
too: failed `tryEval` scopes held 1.28 MB and not one was free of escapes,
and per-unit scopes in eval-jobs are 0.9 % of the arena, the rest being the
package set the next unit reuses.

## The fiber scheduler

Parallel evaluation is implicit. Worker threads force thunks ahead of and
beside the demand path. Four units make it up:

- a **fiber** is a stackful coroutine with its own stack and its own
  evaluator. It is the unit of blocking, and it migrates: a fiber that
  parks on worker A can resume on worker B;
- a **worker** is an OS thread running a drain loop. It owns a bump arena,
  a fiber pool and work-stealing deques;
- a **task** is work not yet started — force a range of list cells or
  attribute cells. It runs in a pooled fiber;
- the **main fiber** runs the command's own closure. When it returns,
  evaluation is done.

Each fiber's evaluator is a clone of the root one. Per-fiber state is
private. Shared state — the interner, the file table, the import cache, the
derivation table — sits behind handles that clone by reference. A fiber's
arena field is swapped with the worker's arena on every resume and swapped
back on every yield, so N workers own N arenas and no fiber ever allocates
from an arena another thread is using. Pointers are global, and cross-arena
references are fine, because no arena is ever freed.

The clone is a deadline. `parallel::try_run` clones the evaluator as the
template before it starts the workers. A setting that is written after that
clone is on the main fiber only. Each symptom is silent: partial output, a
cache that no worker reads, a search path that resolves to nothing.

The evaluator therefore has no setter. `eval::EvalConfig` is a builder. It
holds each setting that is fixed before evaluation: the system, `pure-eval`,
the project root, the session, the search path, the lints, the IR cache,
`keep_aterm`, `keep_site`, the `--dump-drvs` directory and the trace switch. `build()`
makes the evaluator. `api::run::Run` holds the builder, not an evaluator.
`Run::go` sets the trace switch, calls `build()`, then calls `try_run`. This
is the one construction site of each command that goes through `Run`.

The setting fields of the evaluator are private to `eval`. Other modules
read them through `#[inline]` accessors. A write from another module does
not compile. State that a fiber changes during evaluation is not a setting:
the arena, the counters, the eval-store connection and the shared tables.

An `Arc<OnceLock>` for each setting was considered and dropped. It makes a
late write visible, not correct. A fiber that read the old value does not do
its work again, so the answer depends on the time of the write. It also
cannot hold the search path, because the compiler reads `__nixPath` from the
globals table of each fiber. And it puts a load behind fields as small as
`keep_aterm`.

### The claim protocol

The thunk's *state* cell is the synchronisation word. Its three states are
disjoint: a payload, a blackhole, or empty.

```
payload ──CAS──> BLACKHOLE(claimer, waiters bit) ──store──> EMPTY
   ^                          │
   └────────── reset ─────────┘   (error, cancel, or traced-in-helper)
```

A force loads the word first and only attempts the compare-exchange on a
payload word. That ordering is deliberate: a failed compare-exchange still
takes the cache line exclusive, and most forces hit a thunk another fiber
has already evaluated — everything in `lib`, everything in `pkgs` — so the
plain load keeps hot shared lines shared.

The winner gets the payload back and evaluates. A loser reads the current
word. If it is done, it returns the value. If it is a blackhole holding its
*own* claimer id, that is infinite recursion; the id is per fiber, not per
thread, so it follows a computation that migrated. If another fiber holds
it, the loser parks on a side table sharded by thunk address, so thunk
objects never grow.

### Why more workers stop helping

The default is four. The reason is the critical chain, not contention:

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
[caching](caching.md)). That table was measured while the helper fibers had
no IR cache of their own, so a warm run now loads that file instead of
compiling it ([history](history.md)).

The wake policy is where the scheduler's own cost showed. Waking a sleeper
on every push cost the critical path a futex call per fan-out task and
handed cache-warm work to cold cores: 26,000 parks per run at sixteen
workers and 0.9 s of system time. Waking only on a handoff — a ready fiber
always, queued tasks only past a threshold — brought that to about 2,000.

## Why the answer is identical

Three properties together:

1. **Forcing is pure.** The value graph is the same whichever fiber forces
   a thunk.
2. **A thunk is published at most once per successful evaluation.** A reset
   followed by a re-evaluation yields the same value, because the input did
   not change.
3. **Attribute order, list order and derivation hashing do not depend on
   scheduling.** They are decided by symbol ids and by the ATerm, both of
   which are computed from the program.

### Speculation is invisible

A helper computes ahead of the demand path, so what it computes must leave
no trace the program can read. Two places would show it, and both are
closed:

- **The cell.** Only the demand path unboxes a forced cell. A helper
  forces the thunk and leaves the cell alone (`Evaluator::force_cell`,
  `parallel::force::take`). A cell that holds a value is therefore a
  cell the program asked for, which is what `==` reads for the identity of
  a function and what the printers read for `<CODE>`.
- **The thunk state.** A helper publishes `SPECULATED` where the demand
  path publishes `EMPTY` (`Word::thunk_finish`). Both say "forced"; only
  the demand path's mark makes the value printable. The first demand of a
  speculated thunk upgrades the mark.

This is the price of the guarantee, and it is paid on the demand path. A
helper leaves its value inside the thunk, so the demand path reads the
cell, the thunk state and the value, writes the value into the cell, and
writes `EMPTY` over a helper's mark; a run that let helpers unbox finds
the value in the cell with one load. `force_thunk` answers a forced thunk
inline for that reason, and `take` does the whole cell in one read of the
thunk. Measured on bld1, nixos-desktop at four workers, against the same
tree with helper unboxing put back: 3.2 % of the instructions, of which
about 1 point is the store that upgrades the mark. At one worker there is
no helper and no boxed cell, so the same run costs nothing.

The instruction count is the number to quote here. Rebuilding one source
moved this workload's wall by up to two points at an instruction count
equal to 0.01 %, so a wall difference under two points says nothing about
the change that produced it.

A lint finding is not a value and is held back instead. One a helper
produced is parked on the thunk the helper was forcing
(`parallel::force::defer`). The demand path runs it when it takes that
thunk's value and drops it otherwise. Cancelling the helper, as `trace`
does, was measured at 15 % of the wall for lints, and turning the fan-out
off in the builtins that stop early (`any`, `all`, `elem`, `filter`,
`foldl'`, `concatMap`) costs 37 % at four workers, so neither is the
answer. A finding whose value the demand path reaches through a different
thunk than the helper's first claim is dropped, which is a lost warning
and never a wrong one.

A `--dump-drvs` file is written where the derivation is made, not held
back. The drop above is only safe for an effect the program may lose
without losing anything it returns, and that is not true of a `.drv`: the
value escapes into its cell whichever thunk the demand path takes, so a
parked dump goes missing while the `drvPath` that names it is returned.
Held back, the directory lost between 4 % and 26 % of the derivations of a
repkgs evaluation, and a different set on every run.

#### One cell is not enough

A thunk whose word reaches exactly one cell looks safe to unbox: no second
cell can disagree with it. That holds for `==` and for nothing else, so
the idea does not work. The cell is not only the proxy for Nix's slot
identity. It is the record of demand, and the reader that needs that
record needs no second cell:

- `print_ambiguous` reads a cell. A cell that holds a thunk prints
  `<CODE>`; a cell that holds a value prints the value. A helper that
  unboxes the one cell of a `genList` thunk therefore makes the value
  printable, where Nix prints `<CODE>` (`scripts/partest.nu`,
  `print-code-single`). The same reader is behind `trace`, `warn` and the
  error messages that print a value, so no run mode is free of it.
- A lint finding a helper held back is released when the demand path takes
  the thunk (`parallel::force::take_speculated`). A demand path that finds
  the value in the cell never reads the thunk, so the finding is lost
  (`lint-demanded`).

Two distinct single-cell thunks that force to one function are a third
case. Unboxing both makes their cells hold the same word, and `==` then
answers true where the sequential run answers false.

A mark would also have to be set wherever a thunk word reaches a second
cell. Those routes are most of the evaluator: a variable and a selection
in `maybe_thunk`, an argument bound into an env slot, `++`, `tail`,
`filter`, `attrValues`, `catAttrs`, `listToAttrs`, `removeAttrs`,
`intersectAttrs`, `zipAttrsWith`, the argument of a `map` or `mapAttrs`
thunk, the flat copy of a `//`, a `rec` set built from its env, `inherit
(e)`, and `with`. The mark is one bit in the thunk state, set by a
compare-and-exchange at each of those routes. With nothing reading it, it
costs 3.1 % of the instructions on bld1, on nixos-desktop at four
workers, and 1.2 % at one worker, where no mark is ever set and only the
branch at each route remains. `scripts/partest.nu` keeps a case per
route, so the routes stay covered whatever reads the box next.

Only a `Thunk` can carry that bit. A `ThunkApp` and a `ThunkApp2` keep the
argument as their unforced state. An argument is any value word: the tag
owns the low three bits, and an immediate integer owns the rest. So `map`,
`genList`, `mapAttrs` over a large set and `zipAttrsWith` all count as
shared, and only a `Thunk` cell could be unboxed. That alone returns
2.3 % of the instructions. The mark costs 0.8 points more than the
unboxing it allows can return, so the box stays.

### A failure stops the speculation

Nix evaluates a failed thunk again at every force, and so does iets: a
failure resets the thunk. Under fan-out that turned one failing thunk into
one failing evaluation per task, which was measured at 20 s and 11 GB
against 0.01 s and 38 MB at one worker. The runtime now notes the thunk
that failed in a small direct-mapped table. A helper that meets it drops
its task, before the claim and again after a wait, so only the demand path
evaluates it once more and reports. A flag says whether the table can hold
anything, so a run that fails nothing never reads it. A publish also wakes
the demand fiber first, since every fiber behind it only speculates; the
demand fiber takes the front of a thunk's waiter list when it parks, so
the wake order costs the publish nothing.

### Stack

The demand fiber has the stack of the process main thread
(`fiber::MAIN_STACK`), so a program that works at one worker is not a
crash at four. A helper's stack is smaller, and the force path and the
deep walk stop a fiber that comes within `fiber::MARGIN` of its guard
page. The check reads the stack pointer with one instruction
(`fiber::sp`), because it runs on every force: a helper that returned the
address of its own frame could not be inlined and cost a call each time.
A helper drops its task in silence and the demand path computes the
value, the demand fiber reports what Nix reports. A guard page that is hit
anyway — the compiler recurses over a deeply nested expression without
forcing — is caught by a handler on an alternate signal stack, which
prints Nix's `stack overflow (possible infinite recursion)` and exits 1
instead of dying without a word. `sigaltstack` is per thread and the
alternate stack is not a fiber stack, so a fiber switch keeps it valid.

Two effects would break purity, and both are handled by giving them back to
the demand path. `builtins.trace` and `builtins.warn` in a helper fiber
print nothing: the helper sets a sticky cancel flag and abandons the task.
Every thunk still open in that fiber is reset, and because a cancelled
force returns an error, no value is written back into any list, attribute
or environment cell. The demand path then evaluates that thunk itself and
prints, exactly once. An earlier design that journalled the warning and
kept computing lost warnings, because the write-back happened and demand
never re-evaluated.

Cancel is sticky for the same reason. Once set, every publish the fiber
would still do becomes a reset, so a swallowed error cannot leak a value
built from a bailed sub-computation into a shared thunk.

Two divergences are known and accepted. The order of trace lines on stderr
can differ, because the demand walk does every unclaimed cell first and
parks on busy ones afterwards. And a helper starts at call depth zero, so a
program that fails only by exceeding the maximum call depth sequentially may
succeed in parallel.

### Deadlock, and how it resolves

Fan-out can claim the two halves of a cycle on two fibers, which then wait
on each other. Sequential evaluation would have reported infinite recursion
instead. The last worker to go to sleep — when every worker sleeps, nothing
is pending and main is not done — cancels every parked helper fiber. Each
one unwinds, every thunk it claimed is reset, its waiters wake, and the main
fiber re-evaluates the cycle sequentially and reports the error the
sequential path would have reported.

## What is not proven

ThreadSanitizer needs a nightly compiler and has not been run. There is no
model checker. The invariants above are the argument, and the tests are the
evidence: a claim-and-park stress test on a shared graph from eight workers,
the deadlock resolution, the scheduler handshake, twelve expressions
compared at one against N workers, the Nix language suite at eight workers,
and the drvPath comparison against real `nix-instantiate` at eight workers.
The aarch64 context switch is written and untested.
