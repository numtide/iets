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
configuration. Evaluation speed is the first result of this.

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
2. Faster than Nix on `bench/workloads/nixos-server.nix` with the pinned
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
flakes (registries, `nix flake lock`), a git or Mercurial fetcher of our own
(`fetchGit` of anything but a local working tree runs the real nix), a REPL,
a debugger, a bytecode VM.

nix-grpc-store (github.com/Mic92/nix-grpc-store) is the supported gRPC
transport, spoken natively from its vendored `proto/nix_remote.proto`. Its
daemon is somebody else's daemon, which is the point: iets is a client of it.

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
- `builtins/`: primops, one file per family. `store_hooks.rs` maps each
  store path the evaluator reads to its bytes.
- `parallel/`: worker threads, fibers, work-stealing deques, the thunk
  claim protocol.
- `store/`: pure functions. Nix base32, the four hash algorithms, NAR
  serialization and hashing, store paths, ATerm derivations,
  `hashDerivationModulo`, `drvdiff.rs`; `locate.rs` finds derivation roots
  and local-store sources, `source.rs` holds where the bytes of a path are
  and the origin each source came from. `nar.rs` is the whole NAR
  codec. Its reader follows nix-community/go-nix PR #123: one buffered
  window per side, token groups matched as fixed byte strings, and no
  allocation per entry. `hash_derivation_modulo` drops the `children` of
  each input, as derivations.cc does, so a dependency on a dynamic
  derivation's output is not in the modulo hash. No one can see this: the
  producer's text-addressed output has no path, so it and every consumer
  are `Deferred`, and two consumers that take different outputs differ in
  the `env` that carries the placeholder.
- `corepkgs/`: `derivation.nix` and `fetchurl.nix`, embedded from Nix
  2.34.8.
- `print.rs`: the `nix-instantiate --eval` printer, and JSON.

Around it:

- `flake/`: locked flakes. Nix's `call-flake.nix` embedded verbatim,
  `fetchTree` for locked inputs.
- `source/`: a reference resolves to a tree and a lock. The fetchers, the
  input store and the source control system of a local working tree
  (`scm.rs`, with git as its one implementation) are behind it.
  `source::tree` is the one call that `builtins.fetchTree`, the flake root
  and `nix flake prefetch-inputs` share. Parsing a flake reference stays
  in `flake/`, reading one `flake.lock` is `api::flake::info`, and reading
  bytes out of a tree goes through `treestore::StoredTree`.
- `fetch/`: the HTTP client, the tarball spool and the unpack rules.
- `treestore/`: the input store, where a fetched input lives.
- `trace/`: the tracing evaluation cache.
- `events.rs`: the `IETS_EVENTS` stream. A group that is off costs one
  relaxed load and a branch. A record allocates nothing: it is built in a
  buffer the thread keeps and written with one `write`.
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
- `config.rs`: `nix.conf`. Its lists `KNOWN`, `ALIASES`, `CLIENT` and
  `UNIMPLEMENTED` put each Nix setting in a class of
  `docs/reference/settings.md`; `BOOLS`, `INTS` and `NARROW` type the
  values. `settings.rs`: `iets.toml` and every `IETS_*`
  variable. `ignored.rs`: the warning for a flag or a setting that has no
  effect.
- `nixbin.rs`: the real Nix binaries behind the classic fallback.
- `error.rs`: the error type outside the evaluator.
- `api.rs`: the library facade. Frontends use this and never reach into
  `cli`; `examples/` holds four of them. When a frontend needs something
  that lives only in `cli`, move it into `api` or the module it belongs
  to, and let `cli` call it there.
- `main.rs` and `cli/`: the command line on top of `api`, including the
  classic names in `cli/classic/`.

## Layering

A capability is implemented once, below the command line, and a porcelain
command is written against it. `tests/layering.rs` enforces it: no file
under `src/cli/` may name `crate::eval`, `crate::store`, `crate::build`,
`crate::remote`, `crate::flake`, `crate::jobs`, `crate::devenv`,
`crate::trace`, `crate::compile` or `crate::parallel` outside a per-file
allow-list whose length is asserted.

Porcelain-side libraries for presentation, argv compatibility and interactive flows are fine, however large.

What each command is written against. The operation layer is
`crate::api`. Five of its operations are also `iets plumbing` commands:
`resolve`, `plan`, `missing`, `build` and `fetch`. The other entries
(`eval`, `index`, `jobs`, `dev-env`, `drv-show`, `drv-diff`,
`closure-diff`, `flake-info`) are calls the command makes directly:
`api::evaluate`, `api::installable`, `api::flake`, `crate::jobs`.

| Command | Operations | The command's own part |
|:---|:---|:---|
| `iets eval` | `eval` | the human value printer, `--timings` |
| `iets build` | `resolve` (the get-drvs walk for a set), `plan`, `missing`, `build`, through `api::build::Dispatch` | the closing line, the `--dry-run` text, `render_size`, the `fix-hashes` prompts, the `.drv` legacy input |
| `iets search` | `index` | regex compile and match, `--exclude`, colour, `--pretty`, the no-results hint |
| `iets diff` | `resolve`, `drv-diff`, `closure-diff` | the text rendering, the `diff(1)` exit codes |
| `iets eval-jobs` | `jobs` | the worker/collector process model, `--max-memory-size`, the summary line |
| `iets print-dev-env` | `resolve`, `dev-env` | the bash rendering, the direnv cache, the GC root |
| `iets prefetch` | `fetch` | the `--json` shape choice |
| `iets lint`, `iets parse`, `iets cache` | none | already thin over the library |
| `nix-instantiate` | `resolve` (get-drvs walk), `plan` | Nix argv parsing, `--add-root`, out-links, the exec fallback |
| `nix-build` | `resolve`, `plan`, `build` | `result` symlinks, `--add-root`, Nix's exit codes, the fallback |
| `nix-shell` | `resolve`, `plan`, `build` | the env, `execve` of the shell, `--run`, `--pure` |
| `nix-eval-jobs` | `jobs` | argv compatibility |
| `nix eval` | `eval` | Nix's printer and argv |
| `nix build` | `resolve`, `plan`, `build` | `result` symlinks, the `--json` Nix shape, the fallback |
| `nix derivation show` | `resolve`, `drv-show` | Nix's argv, `--pretty`, the fallback |
| `nix search` | `index` | Nix's output format and argv |
| `nix develop` | `resolve`, `dev-env` | the shell it execs |
| `nix print-dev-env` | `dev-env` | the bash rendering |
| `nix flake prefetch-inputs` | `flake-info` | Nix's argv, the fetch threads |

`nix-shell` and `print-dev-env` upload a plan and then speak the worker
protocol themselves, because going through `Dispatch` would open a second
connection. `iets diff` keeps derivations in an in-memory table, because it
evaluates and compares in one process.

Why the layer exists, and when an operation is warranted:
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

## Builtin helpers

### Forcing (all on `Evaluator`)

| helper | returns | error when |
|---|---|---|
| `force(w)` | forced `Word` | evaluation fails |
| `force_cell(&cell)` | forced `Word`, written back to the cell | |
| `force_int(w, pos, ctx)` | `i64` | not an int |
| `force_float(w, pos, ctx)` | `f64` (ints convert) | |
| `force_bool(w, pos, ctx)` | `bool` | |
| `force_string(w, pos, ctx)` | `&StringObj` (bytes + context) | not a string |
| `force_string_no_ctx(w, pos, ctx)` | `&[u8]` | string has context |
| `force_list(w, pos, ctx)` | `&ListObj` (`.cells()`) | |
| `force_attrs(w, pos, ctx)` | `&AttrsObj` (`.get(sym)`, `.attrs()`) | |
| `force_function(w, pos, ctx)` | the function word (lambda, primop, or functor set) | |
| `force_deep(w)` | recursively forced | |
| `coerce_to_string(w, &mut ctx, pos, err_ctx, coerce_more, copy_to_store, canonicalize)` | `Cow<[u8]>`, contexts appended to `ctx` | not coercible |
| `eq_values(a, b, pos)` / `less_than(a, b, pos)` | `bool` | |
| `call1(f, a, pos)`, `call2(f, a, b, pos)`, `apply(f, &args, pos)` | forced result | |

`ctx` is a `&str`: the error context of the builtin.

### Making values

| constructor | notes |
|---|---|
| `ev.mk_int(i)`, `ev.mk_float(f)`, `Word::from_bool(b)`, `Word::NULL`, `Word::TRUE`, `Word::FALSE` | ints are immediates when they fit 61 bits |
| `ev.mk_string(&bytes)` | no context |
| `ev.arena.mk_string_ctx(&bytes, &ctx)` | `ctx: &[Ctx]`; use `ctx_to_arena(&mut ev.arena, vec)` to sort and dedup |
| `ev.arena.mk_string_static(bytes, ctx)` | when bytes already live in the arena (e.g. a substring of an existing string) |
| `ev.arena.mk_path(&bytes)` | path value |
| `ev.mk_list(&words)` | or `ev.arena.alloc_list(n)` and fill `.cells()` |
| `ev.mk_attrs(&mut entries, pos)` | `entries: Vec<(Sym, Pos, Word)>`; sorts, errors on duplicates |
| `ev.arena.mk_attrs_sorted(&entries)` | entries already sorted by `Sym` id and unique |
| `ev.empty_attrs()` | |
| `ev.sym(b"name")` | intern a symbol; `ev.interner.name(sym)` reads it back |
| `ev.mk_pos_attrs(pos)` | `{ file, line, column }` or null |
| `ev.arena.mk_thunk_app(f, x)` | lazy application |

Common symbols are ready on `ev.s`: `out_path`, `drv_path`, `type_`,
`name`, `value`, `functor`, `to_string` and more.

### Errors

```rust
use crate::eval::{ErrorKind, bail};
bail!(ErrorKind::Eval, pos, "cannot create list of size {}", n);
```

The kinds are in `src/eval/error.rs`. `tryEval` catches only `Throw` and
`Assert`. When iets does not support a call, use `Unsupported`. Then a
classic command name runs the real Nix instead. To add a trace line, call
`e.add_trace(pos, msg)`.

## Testing

`cargo test` for the units. `nix flake check` adds clippy, fmt, ast-grep, the
examples, the NixOS module and the scripts. The scripts that need no
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
A comparison typed by hand has no guard: name the reference by its store
path, and check that `readlink -f $(command -v nix-eval-jobs)` does not end
in an `iets` output.

| script | what it compares | runs |
|---|---|---|
| `tests/langtests.rs` | Nix 2.34.8's functional tests, `tests/lang/` | `cargo test` |
| `corpus.nu` | `tests/corpus/` against `.exp` from `nix-instantiate` (`--update` rewrites them) | by hand |
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
| `evaljobs-flags.nu` | `iets eval-jobs` against the pinned `nix-eval-jobs` 2.35.2, flag by flag, on `tests/evaljobs/`; a newer pin changes three things (`docs/reference/commands.md`) | check, container |
| `evaljobs-cmp.nu` | two `eval-jobs` outputs, attribute by attribute | by hand |

`bench/` holds the workloads and the runners. Each one runs from the
repository root, in the dev shell. `NIXPKGS` names the nixpkgs tree to
measure against, and defaults to the pinned source in the scripts;
`nar.nu` always uses the pinned one.

| script | what it measures |
|---|---|
| `bench/run.nu [workload...]` | `nix-instantiate` against iets with hyperfine, on `nixos-server` and `nixos-plasma` by default |
| `bench/perf.nu` | wall, user, sys and peak RSS for one binary |
| `bench/ab.nu` | the same, comparing two binaries; `--runs N`, `--skip-outpaths` |
| `bench/nar.nu` | the NAR writer, reader and hashes; `--nix` adds the real-nix reference rows |
| `bench/eval-jobs.nu [rounds]` | `iets eval-jobs` against `nix-eval-jobs` on `nixos/release-small.nix`, 4 workers, cold and warm cache runs interleaved; `--no-parity` skips the `--force-recurse` job-set comparison |
| `bench/cache-growth.nu` | evaluates `bench/workloads/nixos-server.nix` at two nixpkgs revisions and reports what the second adds to each cache |

The two standard workloads:

```
iets eval --impure --strict --no-tracing-cache --workers 1 -I nixpkgs=$NIXPKGS bench/workloads/nixos-server.nix
iets eval-jobs --impure --no-instantiate --workers 1 --chunks 8 --chunk 0   # ci/eval/outpaths.nix
```

Both reach nixpkgs through `<nixpkgs>`, which a pure evaluation does not
have, so they run with `--impure`.

The evaluation counters (thunks, calls, derivations, compile time) are
per fiber, and so are the arena figures. A fiber adds its own to the run
total whenever it parks or finishes, so the printed numbers cover every
worker, and a workload's derivation count is the same at one worker and
at four. The arena sum grows with workers because each arena interns its
own strings: on nixos-server, ~630 MB at one worker and ~645 MB at four.
`rss_kb` is always the whole process.

## Conventions

- Rust 2024. Every `unsafe` block names the invariant it relies on.
- No comments that restate code. Comments explain why.
- `cargo clippy` clean, `cargo fmt`, `ast-grep scan` clean (`nix/ast-grep`).
- Commit small, descriptive messages.

## Changing the evaluator

Fibers migrate between threads and share every value, so these hold inside and around `src/eval` and `src/parallel`:

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
- A new user-visible effect — a warning, a file, a line on stderr — must say what a helper does when it reaches it before demand: cancel the task
  (`parallel::journal`) or hold it back (`parallel::defer`).
- `Cell::get` is `Acquire` and `Cell::set` is `Release`. An object is fully
  written before the word that points at it is stored into a cell or pushed
  onto a deque.
