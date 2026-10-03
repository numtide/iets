# Using iets as a library

> **Not ready for long-lived hosts.** The evaluator's arena does not free
> memory until the process exits (`src/runtime/arena.rs`). A program that
> evaluates again and again, such as an editor server or a build service,
> grows without limit. Use the library for a run-once frontend: another
> command line, a test harness, a one-shot tool. The API can change.

Every public entry point of the crate: what a frontend calls, what a
`Session` owns, and the value API with its mapping to Nix's C API.

The crate is a library with a thin `iets` binary on top. A frontend —
another CLI, a test harness, a one-shot tool — links the library and
calls the entry points below. `examples/eval.rs`, `examples/build.rs`,
`examples/drv_info.rs` and `examples/value.rs` are working frontends; they
use `pub` items only and never touch `iets::cli`.

**Rule: frontends never reach into evaluator internals.** No `cli`
module, no private field, no copied helper. When something a frontend
needs lives only in `cli`, move it into the library (`crate::api`, or the
module it belongs to) and let `cli` call it there.

**Rule: the value representation is closed.** `crate::runtime` names the
evaluator's types — `Word`, `Arena`, the heap objects — but every operation
on them is `pub(crate)`. A frontend takes a `Word` from `api::evaluate`,
hands it back to the library, and reads it through `api::value`; it cannot
build one, dereference one or allocate in the arena. This is a soundness
rule, not a taste one: a word invented from bits makes a safe call
dereference an address of the caller's choosing.

## Entry points

| What | Where |
|:---|:---|
| Create an evaluator | `eval::Evaluator::new()` for the defaults; `eval::EvalConfig::new()`, its setters, then `build()` |
| Settings of the arguments | `EvalArgs::config(base)` puts the lints, the IR cache, the search path and `--dump-drvs` on an `EvalConfig` |
| The state a run owns | `session::Session::new()`, `Session::enter()` (below) |
| Environment settings | `Session::settings()`, `Session::load_settings(&opts)` (docs/reference/settings.md) |
| Where messages go | `Session::log.write_to(Box<dyn Write + Send>)`, `write_to_stderr()` |
| Arguments of one evaluation | `api::EvalArgs` (`EvalArgs::new()` for the defaults) |
| What to evaluate | `api::Input::{Expr, File, Flake}`, `api::ArgKind` for `--arg` |
| `-I` entries, `NIX_PATH` | `api::parse_include`, `api::full_includes` |
| Evaluate it | `api::evaluate(&mut Evaluator, EvalArgs) -> Result<Word, Box<Error>>` |
| Run on worker threads | `parallel::try_run(workers, ev, f)`; `parallel::default_workers()` |
| All of the above as one call | `api::run::Run`: `Run::new(&args)`, `Run::over(config, &args)` or `Run::on(config)`, then `cached`, `workers`, `hit`, and `go` or `eval` |
| Force a value | `Evaluator::force`, `force_deep`, `force_attrs` |
| Print a value | `print::print_json`, `print::print_ambiguous`; `Evaluator::coerce_to_string` for `--raw` |
| Report an error | `api::report_of(&Evaluator, &Error) -> log::Report` to render yourself; `api::report` sends it to the session log and gives the exit code (docs/reference/errors.md) |
| Error types | `eval::{Error, ErrorKind, EvalResult}`; `error::Error` for store and io |
| Derivations this run created | `Evaluator::drv.drvs` (`StorePath -> builtins::derivation::DrvInfo`) |
| A drv's attribute path and position | `DrvInfo::attr`, `DrvInfo::pos`; `DrvState::set_attr` records the walk |
| Position to file:line:col | `provenance::describe_pos`, else `Evaluator::describe_pos` |
| Store paths back to local trees | `provenance::Provenance::snapshot(ev)` |
| Sources this run copied in | `Evaluator::known.origins()` |
| Derivation roots of a value | `api::installable::drv_roots`; `get_drvs`/`get_drvs_at` for the `recurseForDerivations` walk; `to_install` for `meta.outputsToInstall`; `to_derivation` for the drv path alone |
| Derivations as Nix's JSON | `api::drvshow::show(ev, roots, recursive)`, the `nix derivation show` document; `recursive` walks the input-drv closure |
| Derivations of another run | `api::build::drvs_in_dir(dir)` over an `EvalArgs::dump_drvs` directory; `api::build::drv_info(path, aterm)` for one |
| Outputs to build | `api::build_targets(wanted, roots)` |
| Upload plan | `api::build::plan(ev, roots)`, or `plan_with(roots, drvs, sources)` over a table you hold; empty tables read every drv from `/nix/store` |
| Where the builds go | `api::build::transport(flag, &api::build::config())` |
| Build | `api::build::Dispatch::open(&transport, &options, plan, prov, style)`, then `submit`, `upload_only`, `missing` or `realise`; `overrides(map)` for `--source STOREPATH=LOCAL` |
| One output path per root | `Dispatch::realise(roots, &name)` then `Built::paths(roots)`; `api::build::Root { drv, output }` |
| Build targets and results | `build::Target::new(attr, drv, outputs)`; `build::Outcome` per target |
| Hash a URL, or a fixed-output derivation | `api::fetch::url_hash(url, unpack, algo)`, `api::fetch::drv_hash(ev, root)` |
| Daemon events | `remote::event::Event::{Started, Log, Phase, Stopped, Failed}` |
| Nix configuration | `config::Config::load(&[])` |
| Evaluation cache | `Run::cached(&args, subcommand)`, `Run::hit`, `trace::run::Cache`; `Cache::entry_path()` names the entry a run reads |
| What to watch for an answer to go stale | `api::watch::watch_paths(entry)` over an entry path, giving `Watched { path, kind }` capped at `api::watch::MAX` |
| Flake references | `api::flake_ref`, `api::flake::{is_flake_ref, parse_attr_path, select, outputs, info, tree}`; `crate::flake` is private |
| Where a tree comes from | `source::{SourceRef, Tree, Lock}`, `source::tree(ev, &r, pos)`; the fetchers, the input store and the SCM of a working tree (`source::scm::Scm`) are behind it |
| `eval-jobs` style walks | `jobs::run`, `jobs::Opts` |

## What the host owns

A `Session` holds everything a run used to keep in process-wide statics:
the settings, where messages go, the tree store, the nix.conf it read,
the eval-jobs walk, the hash mismatches a build reported. Two sessions in
one process are independent — different settings, different log
destination, different mismatch list — so a host may run two evaluators
side by side.

The library never writes to stdout or stderr on its own. A session's log
sink starts as nothing; `Session::log.write_to_stderr()` is what `iets`
itself does. A host that renders its own errors uses `api::report_of` and
leaves the sink unset. The library never exits either: it returns errors,
and the frontend decides the exit code.

An `Evaluator` keeps a handle to its session, so the code that has an
evaluator reads `ev.session()` — no lookup. The rest reaches
`Session::current()`, the session this thread entered:

```rust
let session = iets::session::Session::new();
session.load_settings(&[])?;            // iets.toml, optional
let _entered = session.enter();         // for this thread
let mut ev = Evaluator::new();          // adopts the session
```

`EvalConfig::new().session(session)` skips the thread-local altogether.
An evaluator has no setter: each setting goes to the `EvalConfig` before
`build()`. Threads the library starts inherit the session of the thread
that started them; a host that starts its own calls `Session::enter` on
each, or `session::bind(f)`. A thread that entered none uses a fallback
session that writes nowhere.

## Shape of a frontend

1. Make a `Session`, enter it, and build an `EvalArgs`.
2. `api::run::Run::new(&args)`, then `.go(move |ev, cache| …)`:
   everything inside runs as the main fiber, so `force` may park while
   helpers work. `Run` puts the settings of the arguments on the evaluator
   and opens the tracing cache. `parallel::try_run` takes an evaluator that
   you built, and does neither.
3. `api::evaluate(ev, args)` for the value, then force and print it, or
   `drv_roots` + `plan` + `build` for a build.
4. Report errors with `api::report`, which rewrites store paths and
   positions to the local trees they came from, or let `Run::eval` do it.

## The value API

`api::value` is the safe way to look at what an evaluation produced.
`examples/value.rs` is a working frontend on it.

`value::Eval<'e>` wraps a `&'e mut Evaluator`. `value::Value<'e>` is one
value of that evaluator: a `Copy` machine word with no methods that reach
into the arena on their own. The shared `'e` is the safety story — a
`Value<'e>` only exists while the evaluator is borrowed, so it cannot
outlive the arena it points into. `Value` is invariant in `'e`, so it
never widens to another borrow. Nothing in the surface exposes
`runtime::Word`, an arena pointer or a tag.

Methods on `Value` read an already-forced value and need nothing else.
Methods on `Eval` may evaluate, intern a name or allocate.

| What | Where |
|:---|:---|
| Borrow an evaluator | `value::Eval::new(&mut Evaluator)` |
| The evaluator back, for the rest of the library | `Eval::evaluator()` |
| Evaluate an expression | `Eval::eval_expr(src, base_dir)` |
| Evaluate an `EvalArgs` | `Eval::evaluate(args)` |
| Force to weak head normal form | `Eval::force(v)` |
| Force through lists and sets | `Eval::force_deep(v)` |
| The type | `Value::kind() -> ValueKind`, `ValueKind::type_of()` |
| Integer, float, Boolean, null | `Value::as_int`, `as_float`, `as_bool`, `is_null` |
| String bytes and its context | `Value::as_bytes`, `Value::context() -> impl ExactSizeIterator<Item = ContextElem>` |
| Path | `Value::as_path` |
| List | `Value::list_len`, `Value::list_get(i)` (unforced) |
| Attribute count | `Value::attrs_len` |
| Attribute by name | `Eval::attr(v, name)` (unforced), `Eval::has_attr(v, name)` |
| Attribute by index | `Eval::attr_at(v, i) -> Option<(name, value)>` |
| Call a function | `Eval::call(f, arg)`; `Eval::apply(f, arg)` for a thunk |
| Construct | `Eval::int`, `float`, `boolean`, `null`, `string`, `path`, `list`, `attrs` |
| Errors | `eval::EvalResult<T>`, reported with `api::report` |

Notes:

- A getter on an unforced value reports `ValueKind::Thunk` and returns
  `None`; it never panics and never forces. Force first.
- `as_bytes`, `as_path` and the attribute names borrow the arena: no copy.
  The constructors do copy — `string` and `path` copy their bytes in, and
  `list` and `attrs` gather a temporary before the arena copy. Construction
  is the cold path; nothing on a walk allocates.
- `attr_at` index order is the evaluator's internal symbol order, not
  alphabetical, exactly as `nix_get_attr_byidx` warns. Sort the names
  yourself when the order must be reproducible. On a `//` result the first
  `attr_at` flattens the union, which copies it once.
- An evaluator's arena is never freed, so a value of one evaluator passed
  to a *different* `Eval` does not dangle; it reads names through the wrong
  symbol table. The lifetime does not rule that out (two borrows can share
  one region). Keep one `Eval` per evaluator.
- External values (`NIX_TYPE_EXTERNAL`) do not exist in iets, so
  `ValueKind` has no `External`.

### Mapping to Nix's C API

This surface is modelled on Nix's own C API, so a binding for another
language has a shape to follow. These are the `nix_api_value.h` names each
item corresponds to.

| `nix_api_value.h` | `api::value` |
|:---|:---|
| `ValueType`, `nix_get_type` | `ValueKind`, `Value::kind` |
| `nix_get_typename` | `ValueKind::type_of` |
| `nix_get_int`, `nix_get_float`, `nix_get_bool` | `Value::as_int`, `as_float`, `as_bool` |
| `nix_get_string` | `Value::as_bytes` (bytes and length, no copy) |
| `nix_get_path_string` | `Value::as_path` |
| `nix_get_list_size`, `nix_get_list_byidx` | `Value::list_len`, `Value::list_get` |
| `nix_get_attrs_size` | `Value::attrs_len` |
| `nix_get_attr_byname`, `nix_has_attr_byname` | `Eval::attr`, `Eval::has_attr` |
| `nix_get_attr_byidx_lazy`, `nix_get_attr_name_byidx` | `Eval::attr_at` |
| `nix_value_force`, `nix_value_force_deep` (nix_api_expr.h) | `Eval::force`, `Eval::force_deep` |
| `nix_value_call` (nix_api_expr.h) | `Eval::call` |
| `nix_init_apply` | `Eval::apply` |
| `nix_init_int`, `_float`, `_bool`, `_null`, `_string`, `_path_string` | `Eval::int`, `float`, `boolean`, `null`, `string`, `path` |
| `nix_make_list_builder` + `nix_make_list` | `Eval::list(&[Value])` |
| `nix_make_bindings_builder` + `nix_make_attrs` | `Eval::attrs(&[(name, Value)])` |
| `EvalState *` | `Eval<'e>` |
| `nix_value *` | `Value<'e>` |
| `nix_value_incref`/`decref`, `nix_gc_*` | nothing: the arena is never freed |
| `nix_get_external`, `nix_init_external` | nothing: no external values |
| `nix_string_realise`, `nix_alloc_primop`, `nix_register_primop` | not yet |

The string context has no C counterpart in that header beyond
`nix_string_realise`; `ContextElem` carries the four element kinds iets
keeps (`Opaque`, `DrvDeep`, `Built`, and the `Impure` marker that a path
coercion or the metadata of a source tree leaves, which
`builtins.getContext` does not show).
