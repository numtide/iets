# iets architecture

A map of the crate. The reasoning behind each part is in
`docs/explanation/`; this file says what exists and where it lives.

iets is a from-scratch Nix language evaluator in Rust. It parses Nix,
evaluates lazily, computes derivations and store paths in memory, and
prints results. It writes no store of its own: builds go to a Nix daemon or
to a nix-grpc-store service, and a flake input it fetches itself lives in
its own cache.

## Mission

Compress the Nix stack. A Nix user today assembles a working setup from a
dozen helper tools, and most of them exist because Nix lacks one
capability or one good default. iets supplies the capability or the
default, so the tool is no longer needed and Nix feels good to use with no
configuration. Evaluation speed is the first result of this, not the last.

The way to get there is good abstractions and good defaults, not more
code. iets is not a jumbo binary that bundles every tool. A feature
belongs when it falls out of something the evaluator already owns (the
derivation graph, the read set of a run, the fetchers, the cache stores)
and needs little new code. If a feature needs its own large subsystem, the
abstraction is wrong or the feature belongs elsewhere.

## Goals, in order

1. Byte-exact parity with `nix-instantiate --eval --strict` on the
   `.drvPath` of NixOS systems from nixpkgs. The judge is
   `scripts/difftest.nu`.
2. Faster than Nix on `bench/workloads/nixos-desktop.nix` with the pinned
   nixpkgs. Sequential speed first, parallel forcing second.
3. A polished experience out of the box. The good behaviour is the default
   and needs no flag: errors name the fix, nothing is ignored or fails in
   silence, a build output is not garbage-collected under its user, help
   works offline. The classic command names stay byte-compatible with Nix,
   so polish goes into the native commands, stderr, side effects and
   defaults.
4. Small, readable code. One crate, modules by phase, one implementation
   of each capability (`docs/explanation/plumbing-and-porcelain.md`).
5. The fastest client to a remote builder. Evaluate in memory, ask the
   store what it lacks, send only that in one operation, build.

## Non-goals

Local store writes, local builds, a builder daemon of our own, unlocked
flakes (registries, `nix flake lock`), fetchers that hit the network (a
locked flake input is the one exception), a REPL, a debugger, a bytecode
VM.

nix-grpc-store (github.com/Mic92/nix-grpc-store) is the supported gRPC
transport, spoken natively from its vendored `proto/nix_remote.proto`. Its
daemon is somebody else's daemon, which is the point: iets is a client of
it, not a replacement for it.

## Pipeline

```
source bytes
  -> syntax::lexer / syntax::parser     -> ast::Ast (per file, arena of Expr)
  -> compile                            -> ir::Ir (scopes resolved, attrpaths merged)
  -> eval                               -> runtime::Word (lazy graph, forced on demand)
  -> print / json / drvPath
```

Why each stage is shaped as it is: `docs/explanation/evaluation.md`.

## Modules

The evaluator:

- `syntax/`: `symbol.rs` (interner), `ast.rs`, `lexer.rs`, `parser.rs`,
  `dump.rs` (`iets parse`, as text and as JSON). The parser keeps raw
  bindings; attribute-path merging and scope resolution happen in
  `compile`.
- `compile/`: AST to IR (`ir.rs`). Every variable becomes a local slot or a
  `with` lookup. `cache.rs` is the on-disk IR cache.
- `runtime/`: `arena.rs` (bump allocator per worker, no GC), `word.rs`
  (8-byte tagged word), `value.rs` (heap objects), `census.rs` (the memory
  census).
- `eval/`: the tree-walking evaluator over IR — `force`, apply, the `with`
  chain, operators, equality, `coerce.rs`, `error.rs`.
- `builtins/`: primops, one file per family.
- `parallel/`: worker threads, fibers, work-stealing deques, the thunk
  claim protocol.
- `store/`: pure functions. Nix base32, the four hash algorithms, NAR
  serialization and hashing, store paths, ATerm derivations,
  `hashDerivationModulo`, `drvdiff.rs`; `locate.rs` finds derivation roots
  and local-store sources, `source.rs` holds where the bytes of a path are
  and the origin each source came from.
- `corepkgs/`: `derivation.nix` and `fetchurl.nix`, embedded from Nix
  2.34.8.
- `print.rs`: the `nix-instantiate --eval` printer, and JSON.

Around it:

- `flake/`: locked flakes. Nix's `call-flake.nix` embedded verbatim,
  `fetchTree` for locked inputs.
- `source/`: a reference resolves to a tree and a lock. The fetchers, the
  input store and the source control system of a local working tree
  (`scm.rs`, with git as its one implementation) are behind it.
- `fetch/`: the HTTP client, the tarball spool and the unpack rules.
- `treestore/`: the input store, where a fetched input lives.
- `trace/`: the tracing evaluation cache.
- `pure-eval`: `EvalConfig::pure`, read with `Evaluator::pure()`, checked in
  `builtins::store_hooks`.
- `cachefs.rs`: the on-disk hygiene all three stores share.
- `lint/`: the lint registry, level resolution, and one file per family.
  A lint is here only if it needs evaluation to decide
  (`docs/explanation/lint-scope.md`). `site.rs` finds the byte span of a
  finding.
- `remote/`: the store transports. `plan.rs` (the upload plan), `worker/`
  (the Nix worker protocol), `grpc/` (nix-grpc-store), `copy.rs`,
  `event.rs`.
- `build/`: the dispatcher every build goes through, its two backends, the
  uploaders and the renderer.
- `jobs/`: the eval-jobs walk.
- `cli/search.rs`: the package search and its index.
- `devenv/`: `print-dev-env`.
- `provenance.rs`: store paths and derivations back to local trees and
  source positions.
- `config.rs`: `nix.conf`. `settings.rs`: `iets.toml` and every `IETS_*`
  variable. `ignored.rs`: the warning for a flag or a setting that has no
  effect.
- `nixbin.rs`: the real Nix binaries behind the classic fallback.
- `error.rs`: the error type outside the evaluator.
- `api.rs`: the library facade. Frontends use this and never reach into
  `cli`; `examples/` holds four of them.
- `main.rs` and `cli/`: the command line on top of `api`, including the
  classic names in `cli/classic/`.

## The layer rule

A capability is implemented once, below the command line, and a porcelain
command is written against it. `tests/layering.rs` enforces it: no file
under `src/cli/` may name `crate::eval`, `crate::store`, `crate::build`,
`crate::remote`, `crate::flake`, `crate::jobs`, `crate::devenv`,
`crate::trace`, `crate::compile` or `crate::parallel` outside a per-file
allow-list whose length is asserted.

What the rule does *not* ban is porcelain-side libraries for presentation,
argv compatibility and interactive flows, however large.

Why, and what the migration to it measured:
`docs/explanation/plumbing-and-porcelain.md`.

## Where the explanations are

| question | page |
|---|---|
| How does evaluation work, and why is parallel evaluation safe? | `docs/explanation/evaluation.md` |
| What is on disk, and why is the input store safe without a daemon? | `docs/explanation/caching.md` |
| How does a flake reference become bytes? | `docs/explanation/flakes-and-sources.md` |
| Why is there a plumbing layer, and what did it cost? | `docs/explanation/plumbing-and-porcelain.md` |
| Why trace a store path back to a file? | `docs/explanation/provenance.md` |
| What was tried and removed? | `docs/explanation/history.md` |

Reference material — every command, setting, error code, lint, builtin,
divergence and library entry point — is under `docs/reference/`.

## Testing

`cargo test` for the units. `nix flake check` adds clippy, fmt, ast-grep, the examples,
the NixOS module and the scripts, in two ways. The scripts that need no
`nix-daemon` run in the build sandbox. The scripts that need a daemon and a
store to write to run in a NixOS test container (`nix/e2e-test.nix`,
x86_64-linux only): systemd-nspawn, so no KVM; the real Nix 2.34.8 on `PATH`, a
`nix-daemon` with its build sandbox on, and no network. The nixpkgs tree there
is the flake input, through `$NIXPKGS`, and the closure of every path a case
realises is in the container's store, so nothing is substituted. The builder
needs the `uid-range` system feature (`auto-allocate-uids = true`).

Every script that compares against the real nix calls `require-real-nix`
first. On a host that installs the classic farm, `nix` and
`nix-instantiate` on `PATH` are iets, so outside the dev shell the
comparison would be iets against itself, and the run would pass while
testing nothing. The guard resolves the symlink chain of each name and
aborts when it ends in `iets`, the same test the fallback uses
(`is_iets`, `src/nixbin.rs`). Nothing behavioural can tell them apart:
the farm execs the real nix for whatever it does not implement,
`--version` included. The same guard warns when the reference nix is not
2.34.8. The iets-only scripts do not call it and still run anywhere.

| script | what it compares | runs |
|---|---|---|
| `langtests.nu` | Nix 2.34.8's functional tests, `tests/lang/` | check, sandbox |
| `corpus.nu` | `tests/corpus/` against `.exp` from `nix-instantiate` (`--update` rewrites them) | by hand |
| `parity.nu` | `match`, `split`, `fromTOML` against the real `nix-instantiate` | by hand |
| `partest.nu` | 1 against N workers | check, sandbox |
| `linttest.nu` | the lints on their fixtures | check, sandbox |
| `cachetest.nu` | the tracing cache's hits and misses | check, container |
| `difftest.nu`, `drvdiff.nu` | one expression against `nix-instantiate`; the first differing derivation | by hand |
| `diff-e2e.nu` | `iets diff` on two small derivations | check, sandbox |
| `classic-e2e.nu` | the classic names against the real nix, stdout byte for byte | check, container |
| `daemon-e2e.nu` | `iets build` over the worker protocol | check, container; not by hand on a host whose store refuses `nix-store --delete` |
| `build-e2e.nu` | `iets build` over an attribute tree: the verdicts, the exit code, `--json` | check, container |
| `grpc-e2e.nu` | `iets build` over `grpc://` | by hand |
| `flaketest.nu` | flake evaluation and the input store against `nix eval` | check, container, less the 3 cases that need the network (`online` in `lib.nu`); all 39 by hand |
| `plumbing-e2e.nu` | `iets plumbing`, one case per op | check, container |
| `prefetch-e2e.nu` | `iets prefetch` against `nix store prefetch-file` and `nix hash` | check, container |
| `devenv-test.nu` | `print-dev-env` against `nix print-dev-env` | by hand |
| `provenance-e2e.nu` | the rewritten paths and positions | by hand |
| `evaljobs-flags.nu` | `iets eval-jobs` against `nix-eval-jobs`, flag by flag | check, container |
| `evaljobs-cmp.nu` | two `eval-jobs` outputs, attribute by attribute | by hand |

`bench/` holds the workloads and the runners (`bench/run.nu`, `ab.nu`,
`perf.nu`, `eval-jobs.nu`, `nar.nu`, `cache-growth.nu`).

## Conventions

- Rust 2024. Every `unsafe` block names the invariant it relies on.
- No comments that restate code. Comments explain why.
- `cargo clippy` clean, `cargo fmt`, `ast-grep scan` clean (`nix/ast-grep`).
- Commit small, descriptive messages.

## Constraints on evaluator changes

Fibers migrate between threads and share every value, so six rules hold
inside and around `src/eval` and `src/parallel`:

- `Evaluator` derives `Clone`. A new field must be `Clone`, and per-fiber
  state must be cheap to clone; a shared table goes behind a handle in
  `parallel::shared`.
- An inline cache on an IR node or a heap object is read and written by
  several threads. Use atomics, and expect stale misses.
- A function that reads thread-local storage must be `#[inline(never)]` and
  must not force anything. LLVM may cache a TLS base across calls inside one
  function, and a fiber that migrated between two accesses would touch
  another thread's slot. No thread-local is read on the context-switch path
  or inside the scheduler.
- A new strict list or attribute-set consumer can call
  `parallel::fanout_list` or `fanout_attrs_coerce` after forcing the
  container. It is one line, and a no-op with one worker.
- A new place that writes a forced value into a cell must do so only on
  `Ok`, and only on the demand path (`ev.par.is_main`). Otherwise a
  cancelled helper could leak a value built from a bailed sub-computation,
  and a cell that holds a value would no longer mean the program asked for
  it, which `==` and the printers read.
- A new user-visible effect — a warning, a file, a line on stderr — needs a
  rule for a helper that reaches it before demand: cancel the task
  (`parallel::journal`) or hold it back (`parallel::defer`).
- `Cell::get` is `Acquire` and `Cell::set` is `Release`. An object is fully
  written before the word that points at it is stored into a cell or pushed
  onto a deque.
