# Architecture review, 2026-10-12

Commit `f685575b`, 77,807 lines of Rust under `src/`. This review is
adversarial. It ranks defects by what breaks. It does not fix anything.

Every finding has a concrete failure. "Verified" means the reviewer read
the code and, where it says "ran", ran a build of `f685575b` from the farm.
Fix sizes: **S** is less than 50 lines, **M** is less than 300, **L** is
more.

## How to read this

Correctness findings and taste findings are in separate lists. A taste
finding costs speed, memory or maintenance. It does not give a wrong
answer.

## Correctness, ranked

### 1. The library replaces the host's SIGSEGV and SIGBUS handlers

- Where: `src/sys/fault.rs:29` calls `sigaction(sig, &sa, NULL)` and keeps
  no old handler. `src/sys/fault.rs:51` resets to `SIG_DFL` for a fault it
  does not own.
- When: the first fiber starts (`src/parallel/sched.rs:283`, `:304`), or the
  first session heap drops (`src/runtime/heap.rs:102`).
- Failure: a host that uses SIGSEGV for its own work loses its handler.
  The JVM, the Go runtime and wasm engines do this for null checks and
  bounds checks. The host's next expected fault kills the process with the
  default action.
- `docs/reference/library.md` says the handler "replaces Rust's `SIGSEGV`
  handler". It replaces any handler.
- Fix: keep the old `sigaction` and call it when `classify` declines. **S.**

### 2. Safe library code can read out of bounds

- Where: `src/runtime/value.rs:209` has `pub len: AtomicU32` on the `pub`
  type `AttrsObj`. `src/eval/mod.rs:1314` is
  `pub fn force_attrs(...) -> EvalResult<&'static AttrsObj>`.
  `force_list` (`:1323`) and `force_string` (`:1360`) are the same.
- Failure: a host writes
  `ev.force_attrs(w, Pos::NONE, "")?.len.store(u32::MAX - 1, Relaxed)`.
  The next `attrs_raw()` (`value.rs:431`) makes a slice of about four
  billion entries. This is undefined behaviour from safe code.
- The same `&'static` returns outlive the session without a compile error.
  A host that keeps one gets an abort, not a borrow error.
- `docs/reference/library.md` says "You can't build one, dereference one or
  allocate in the arena". That claim is false through these functions.
- Fix: make the object fields `pub(crate)`, and return borrows tied to the
  evaluator, as `Value<'e>` already does. **S** for the fields, **M** for
  the returns.

### 3. `builtins.storePath` accepts a path that does not exist

- Where: `src/builtins/paths.rs:370-406`.
- Ran: `iets eval --impure --expr 'builtins.storePath "/nix/store/00000000000000000000000000000000-x"'`
  prints the string with context. `nix eval` fails in `storePath`.
- Failure: the error comes later, in a read or a build, far from its cause.
  Or it never comes, and a wrong path goes into a derivation.
- This is the worst class of builtin defect: a wrong value, not an error.
- Fix: check validity and fail as Nix does. **S.**

### 4. `readFile` of a `drvPath` has three outcomes, set by flags

- Where: `src/builtins/store_hooks.rs:55` serves a new `.drv` only from
  the kept ATerm text. `src/cli/eval.rs:23` keeps the text only when the
  tracing cache is on.
- Ran, with `E = builtins.readFile (derivation {...}).drvPath`:

  | flags | result |
  |---|---|
  | none | `access to absolute path '/nix/store/…drv' is forbidden in pure evaluation mode` |
  | `--impure` | the ATerm text |
  | `--no-tracing-cache` | `… is not in the store and this run did not keep its text` |

- Failure: a cache flag changes what a program returns. The default pure
  error is also wrong: the path is the program's own output.
- Fix: keep the typed derivation and print its text on demand, then stop
  keeping text by default (see taste finding T3). **M.**

### 5. `builtins.path` and `filterSource` fail on the program's own store paths

- Where: `src/builtins/store_hooks.rs:242-252` (`recopy_unsupported`),
  `src/builtins/paths.rs:542`.
- Ran: `iets eval --expr 'builtins.path { path = builtins.toFile "a" "b"; }'`
  fails with `copying '/nix/store/…-a' to the store again is not supported`.
  A `.drv` made in the same run fails the same way. Nix copies both.
- `docs/reference/builtins.md` does not list this.
- Fix: a source origin for in-memory store paths. **M.** Document it first. **S.**

### 6. Helper fibers do network I/O that the program never asked for

- Where: only `trace` and `warn` stop a helper
  (`src/builtins/core.rs:196`, `src/parallel/force.rs:330`). `fetchurl`,
  `fetchTarball`, `fetchGit` and store adds run in helpers.
  `elem`, `any` and `all` fan out before they stop early
  (`src/builtins/lists.rs:75`, `:266`, `:291`).
- Ran: a local HTTP server counted requests for
  `builtins.elem "hi\n" (genList (i: readFile (fetchurl {...})) 64)`.

  | workers | requests, three runs |
  |---|---|
  | 1 | 1, 1, 1 |
  | 4 | 2, 3, 4 |
  | 8 | 9, 6, 2 |

- Failure: network traffic and downloads change from run to run. A slow or
  metered URL in an unused branch costs time and money. A probe that read
  the result back through `pathExists` saw no change, so the program's
  value is not affected in that case.
- "Speculation can't change the result" in `docs/explanation/evaluation.md`
  holds for values. It does not cover I/O.
- Fix: treat a fetch in a helper like `trace`: bail and let the demand path
  do it. Measure the loss of parallel fetches first. **M.**

### 7. `max-jobs` defaults to the CPU count

- Where: `src/config.rs:637-641`. `cores` defaults to 0, "all of them"
  (`:646`). Nix's default `max-jobs` is 1.
- Failure: on a 32-core host with no `max-jobs` in `nix.conf`, a trusted
  client asks for 32 parallel builds with 32 cores each. That is about
  1,000 compiler threads, and a real risk of the OOM killer.
- Fix: default to 1, as Nix does. **S.**

### 8. Two evaluators in one session mix symbol numbers

- Where: `src/eval/mod.rs:546` makes a new `Interner` for every `build()`.
  The heap is shared, so a `Word` from evaluator A is still valid memory
  in evaluator B.
- Failure: a host passes a `Word` from `ev1` to `ev2`. User attribute names
  print wrong, or a lookup panics with "unknown symbol". Nothing faults.
- Fix: one interner per session. This also saves memory. **M.**

### 9. A failed reference query is cached for the life of the process

- Where: `src/store/locate.rs:15` is a process-wide `static`.
  `:27` stores `query_references(..).unwrap_or_default()`.
- Failure: a path that is not valid yet gets an empty reference set. When
  it is built later in the same process (eval-jobs, a library host), the
  empty set stays. `addPath` then makes a path with missing references.
- Fix: do not cache a failure, and move the cache into `Session`. **S.**

### 10. The store directory is fixed once per process

- Where: `src/store/path.rs:26`, a `static DIR: OnceLock<StoreDir>`.
- Failure: a host makes session A, then makes session B for another store
  directory. Session B hashes and prints `/nix/store` paths. The paths are
  wrong for B's store.
- Fix: put the store directory in `Session`. **S** to **M.**

### 11. Released sessions keep their address space for ever

- Where: `src/runtime/heap.rs:110-130` makes each released range
  `PROT_NONE` and never unmaps it. `heap.rs:142` is a list that only grows.
  Each session holds at least one 32 MiB arena chunk and a 1 MiB name block.
- Failure: a host that makes one session per request reaches
  `vm.max_map_count` (65,530 by default). The next `mmap` fails and
  `heap.rs:85` calls `process::exit(1)` inside the host.
- `docs/reference/library.md` states the cost. It does not state that the
  end is an exit.
- Fix: return an error, not an exit, and reuse ranges behind a session
  generation check. **M.**

### 12. A malformed `fetchTarball` hash is reported as a missing one

- Where: `src/builtins/fetch.rs:226-249` discards the parse error with
  `.and_then(parse_any).ok()`.
- Ran: `sha256 = "bogus"` gives `builtins.fetchTarball without a sha256 is
  not supported`.
- Fix: report the parse error. **S.**

### 13. Latent: thread-locals in code that a fiber can migrate across

- Where: `thread_local!` at `src/events.rs:22`, `src/session.rs:53`,
  `src/store/drv.rs:195`, `src/builtins/text/mod.rs:60`.
- A fiber that parks on worker A can resume on worker B. LLVM treats a
  thread-local address as constant inside one function. If a function reads
  a thread-local, then parks, then reads it again after inlining, it can use
  worker A's copy from worker B. `REGEX_CACHE` holds `Rc` values, so that
  is a non-atomic reference count shared by two threads.
- No current site was shown to do this. `get_regex` is `#[inline(never)]`
  for this reason, and its comment says callers hold the `Rc` across no
  force. The rule is in a comment, not in a check.
- Failure, if a future change breaks the rule: a silent reference count
  race, then a use after free. ThreadSanitizer would not see it without
  fiber annotations.
- Fix: an ast-grep rule that allows `thread_local!` only in a named list of
  files. **S.**

### 14. Latent: `SyncEv` is `Send` and `Sync` by assertion

- Where: `src/parallel/force.rs:45-46`. The impls cover the whole
  `Evaluator`, which the compiler would refuse because `Ctx.slot` is a raw
  pointer.
- No field holds an `Rc` or `RefCell` today. Nothing stops one being added.
  A worker thread clones the template while another clones it too
  (`new_user`, `force.rs:49`), so an added `Rc` races.
- Fix: a compile-time assertion that every field but `par` is `Send + Sync`,
  or a wrapper on the one raw pointer. **S.**

### 15. Latent: the heap kind byte is trusted

- Where: `src/runtime/value.rs:770` matches `*(p as *const Kind)`.
  `thunk_value` (`:910`) ignores its `state` argument.
- Failure, if a bug puts an IR or environment pointer where a value is
  expected: a byte in 1 to 14 reads the memory as a string or a list and
  prints wrong output with no error. Other bytes are undefined behaviour.
- Fix: match on a `u8` with a panic for an unknown kind, and assert the
  forced state in debug builds. **S.**

## The unsafe concurrency core

The documentation says that ThreadSanitizer has not been run. This section
says what each part of the core assumes, and whether the code checks it.

| part | assumption | checked or believed | a race would |
|---|---|---|---|
| claim protocol, `parallel/thunk.rs:51-83` | one CAS from payload to blackhole; a plain load first | checked by the CAS; the states are disjoint by bit layout | not happen: losers re-read |
| park and wake, `thunk.rs:101-151`, `sched.rs:526-607` | `WAITERS_BIT` is set under the shard lock; one atomic `state` carries PARKING, WOKEN, SUSPENDED, RUNNABLE | checked: every transition is a CAS or swap, and the worker swaps to SUSPENDED after the switch | not happen in the reviewed paths |
| work-stealing deque, `parallel/deque.rs` | the orderings of Lê et al., PPoPP 2013; fixed size, so no reclamation | follows the paper; a torn read in `steal` is discarded by the failed CAS | not happen |
| value publication, `runtime/word.rs:285-291` | every `Cell` read is Acquire and every write is Release; object bodies are written before the pointer | checked by the type: there is no Relaxed `Cell` path | not happen. Relaxed stores exist only for idempotent memos (union `len`, `value.rs:484`) |
| arena give-back, `runtime/arena.rs:162-182` | nothing was allocated since the mark | `release` compares the chunk end, so an arena swap is safe. `shrink_last` checks nothing. Its one caller (`value.rs:1280`) does not force, so it is safe today | corrupt silently if a future caller forces between the allocation and the shrink |
| slot ownership, `force.rs:91-146` | one fiber owns a slot; `ev_of` makes a `&'static mut` | believed; argued in comments | corrupt silently |
| `SyncEv`, thread-locals, kind byte | see findings 13, 14, 15 | believed | corrupt silently |
| deadlock resolution, `sched.rs:751-795` | all workers asleep and main not done means a cycle | checked; a blocked syscall keeps its worker awake, so no false trigger | abort with an internal error if no parked helper exists |

Verdict: the protocol core is sound as far as reading it can show. The
risk is in the believed rows, which a later change can break without a
test failing.

### What it would take to run ThreadSanitizer once

1. A nightly toolchain in the dev shell, and `-Zbuild-std`, because the
   standard library must be built with TSan too.
2. `RUSTFLAGS="-Zsanitizer=thread" cargo +nightly test -Zbuild-std
   --target x86_64-unknown-linux-gnu -- parallel::`
3. Fiber annotations. TSan sees one fiber stack used by several threads as
   races everywhere. `Fiber::resume` and `Fiber::yield_`
   (`parallel/fiber.rs:266-288`) need `__tsan_create_fiber`,
   `__tsan_switch_to_fiber` and `__tsan_destroy_fiber` behind
   `cfg(sanitize = "thread")`. About 40 lines.
4. Without step 3, the run produces noise, not evidence.

The deque and the claim protocol are small enough for `loom` too, which
needs no nightly. That is a model check of the parts that `loom` can
express, not of the context switch.

## Your five suspicions

1. **"The arena never frees."** Refuted as written. Since #88 a session
   owns its heap, and the heap is freed when the last handle drops
   (`src/runtime/heap.rs`, `docs/reference/library.md`). `README.md:114`
   is stale. There is also no C ABI: the library is a Rust API that maps
   to Nix's C API. What is left is findings 1, 2, 8 and 11. The fix is not
   a collector. It is generation-checked handles in place of `'static`,
   and a chained signal handler.
2. **"Two representations."** Reframed. A `drvPath` is the hash of the
   ATerm text by Nix's definition, so printing the text to hash it is
   correct. String context holds only paths (`value.rs:102-120`). The
   defect is the reverse: the typed derivation is dropped after creation,
   so nine consumers parse text that this process printed (T3), and the
   kept text decides a `readFile` result (finding 4).
3. **"Defaults that pessimise."** Confirmed, with new ones: `max-jobs`
   (finding 7), ATerm text kept by default and never stored (T3), and two
   caches with no size limit (T4).
4. **"Builtins that exist and fail."** Confirmed. `builtins.attrNames
   builtins` matches Nix 2.34.8. `fetchMercurial` and `fetchClosure` exist
   and always fail, and the docs say so. The doc gaps are `path` and
   `filterSource` (finding 5), the string form of `fetchTree`, and type
   `file` of `fetchTree`. `flake-compat` uses `builtins.fetchTree or
   fallback`, so a `file` lock node fails in iets's own commands and the
   fallback never runs. The `outputOf` row is now correct.
5. **"Module boundaries."** Confirmed as taste (T1). There is no layer that
   can be built alone.

## Taste, ranked

### T1. All 33 top-level modules are one dependency cycle

Only `argv`, `project`, `corepkgs` and `main` are outside it. Examples:

- `src/log.rs:21` uses `crate::cli::help`. The logger depends on the CLI.
- `src/syntax/symbol.rs:8` re-exports `crate::parallel::interner::Interner`,
  and `src/syntax/ast.rs:15` uses `crate::lint`. The parser carries the
  scheduler, the session and the heap.
- `src/runtime/value.rs:13` uses `crate::eval`; `eval` uses `runtime`.

Cost: no layer has a test that runs without the rest. `sys` is the one
clean module. **L** in total, **M** per edge.

### T2. Four workers by default

Given in the brief: 0.66 s against 0.52 s at one worker on a sequential
workload. `src/parallel/mod.rs:66-74`. Not measured again.

### T3. The typed derivation is dropped; text is kept and parsed again

- `DrvInfo` (`src/builtins/derivation.rs:41-56`) keeps outputs, references
  and optional text. Consumers parse the text again:
  `jobs/mod.rs:533`, `builtins/derivation.rs:938`, `cli/devenv.rs:562`,
  `cli/diff.rs:44`, `api/fetch.rs:66`, `remote/plan.rs:370`,
  `build/mod.rs:482`, `build/mod.rs:627`, `remote/grpc/build.rs:86`.
- `iets eval` keeps all text when the cache is on (`src/cli/eval.rs:23`),
  which is the default. Then it writes the entry with `tables = false`
  (`src/cli/eval.rs:84`, `src/trace/run.rs:152`), so nothing reads the text.
  This is the 331 MB class of cost, back by default.
- Fix: keep `system`, input outputs and `requiredSystemFeatures` typed. **M.**

### T4. Two disk caches with no size limit

- The trace cache is on by default and expires by age only, 30 days
  (`src/cachefs.rs:51`). On the reviewer's machine: 2.1 GB, 614 entries,
  largest 58 MB. A `build` entry holds the text of every derivation.
- The IR cache key includes the absolute path (`src/compile/cache.rs:81-104`).
  Every nixpkgs revision writes every file again. On the reviewer's machine:
  510 MB, 61,393 entries.
- Fix: a size budget in the sweep. **S.**

### T5. Process-wide statics that should be per runtime

`ANY_FAILED` (`src/parallel/sched.rs:33`), `NPENDING` and `EFFECT_BAILS`
(`src/parallel/force.rs:340-351`), `SAMPLE_POS` and `START`
(`force.rs:199-201`). One session's failure or debug setting slows every
later session. **S.**

### T6. `stats2` is not built in CI

It compiles today. No check builds it (`nix/checks.nix`). **S.**

## What is well built

These are what make the ranking above mean something.

- **The claim protocol.** One state word, three disjoint states, a plain
  load before the CAS, and waiters in a side table so thunks stay two words.
- **The park and wake handshake.** One atomic carries the whole fiber
  state. A wake that comes before the park is not lost.
- **The `Cell` discipline.** All publication goes through one type with
  Release and Acquire. There is no Relaxed path to misuse.
- **Speculation is invisible to values.** `SPECULATED` against `EMPTY`, the
  sticky cancel, and the held-back lint effects are each argued and each
  tested.
- **Use after release is loud.** A stale `Word` hits a `PROT_NONE` range and
  stops with a clear message. It never reads another session's data.
- **`Word` cannot be forged.** The bits are private, `from_bits` is `unsafe`,
  and compile-fail doctests check this (`src/runtime/word.rs:26-37`).
- **`sys` is a clean platform layer** with nothing above it.
- **Builtin surface parity.** `builtins.attrNames builtins` matches Nix
  2.34.8 exactly, and feature gating under classic names matches too.

## Not covered

- The Windows, aarch64 and riscv64 context switches. The docs say aarch64
  is untested.
- `remote/`, the gRPC transport, flake locking, `lint/` and `fetch/http.rs`.
- eval-jobs memory at four worker processes. Not measured.
- No measurement ran on bld1. No finding above depends on a timing.
