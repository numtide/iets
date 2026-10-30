# Using iets as a library

> **Memory goes with the session.** A program that evaluates again and
> again, such as an editor server, makes one session per evaluation
> ("Sessions and memory"). The API can change.

iets is a library with a thin `iets` binary on top. This page lists the entry
points, says what a `Session` owns and maps the value API to Nix's C API.
`examples/` has working frontends (`eval.rs`, `build.rs`, `drv_info.rs`,
`value.rs`) that use `pub` items only and never touch `iets::cli`. Do the
same in yours.

`crate::runtime` names the evaluator's types (`Word`, `Arena`, the heap
objects). Their operations are `pub` because `api` and `cli` live in a crate
above `iets-core`, but they are not part of this contract. You take a `Word`
from `api::evaluate`, give it back to the library, and read it through
`api::value`. Do not build one, dereference one or allocate in the arena,
because a word made up from bits would let a safe call read any address.

## Entry points

| What | Where |
|:---|:---|
| Create an evaluator | `eval::Evaluator::new()` for the defaults; `eval::EvalConfig::new()`, its setters, then `build()` |
| Settings of the arguments | `EvalArgs::config(base)` puts the lints, the IR cache, the search path and `--dump-drvs` on an `EvalConfig` |
| The state a run owns | `session::Session::new()`, `Session::enter()` ("What the host owns") |
| Environment settings | `Session::settings()`, `Session::load_settings(&opts)` (docs/reference/settings.md) |
| Where messages go | `Session::log.write_to(Box<dyn Write + Send>)`, `write_to_stderr()` |
| Arguments of one evaluation | `api::EvalArgs` (`EvalArgs::new()` for the defaults) |
| What to evaluate | `api::Input::{Expr, File, Flake}`, `api::ArgKind` for `--arg` |
| Evaluate it | `api::evaluate(&mut Evaluator, EvalArgs) -> Result<Word, Box<Error>>` |
| Run on worker threads | `parallel::try_run(workers, ev, f)`; `parallel::default_workers()` |
| All of the above as one call | `api::run::Run`: `Run::new(&args)`, `Run::over(config, &args)` or `Run::on(config)`, then `cached`, `workers`, `hit`, and `go` or `eval` |
| Force a value | `Evaluator::force`, `force_deep`, `force_attrs` |
| Print a value | `print::print_json`, `print::print_ambiguous`; `Evaluator::coerce_to_string` for `--raw` |
| Report an error | `api::report_of(&Evaluator, &Error) -> log::Report` to render yourself; `api::report` sends it to the session log and gives the exit code (docs/reference/errors.md) |
| Error types | `eval::{Error, ErrorKind, EvalResult}`; `error::Error` for store and io |
| Derivations this run created | `Evaluator::drv.drvs` (`StorePath -> builtins::derivation::DrvInfo`) |
| Derivation roots of a value | `api::installable::drv_roots`; `get_drvs`/`get_drvs_at` for the `recurseForDerivations` walk; `to_install` for `meta.outputsToInstall`; `to_derivation` for the drv path alone |
| Derivations as Nix's JSON | `api::drvshow::show(ev, roots, recursive)`, the `nix derivation show` document; `recursive` walks the input-drv closure |
| Derivations of another run | `api::build::drvs_in_dir(dir)` over an `EvalArgs::dump_drvs` directory; `api::build::drv_info(path, aterm)` for one |
| Outputs to build | `api::build_targets(wanted, roots)` |
| Upload plan | `api::build::plan(ev, roots)`, or `plan_with(roots, drvs, sources)` over a table you hold; empty tables read every drv from `/nix/store` |
| Where the builds go | `api::build::transport(flag, &api::build::config())` |
| Build | `api::build::Dispatch::open(&transport, &options, plan, prov, style)`, then `submit`, `upload_only`, `missing` or `realise`; `overrides(map)` for `--source STOREPATH=LOCAL` |
| One output path per root | `Dispatch::realise(roots, &name)` then `Built::paths(roots)`; `api::build::Root { drv, output }` |
| Hash a URL, or a fixed-output derivation | `api::fetch::url_hash(url, unpack, algo)`, `api::fetch::drv_hash(ev, root)` |
| Evaluation cache | `Run::cached(&args, subcommand)`, `Run::hit`, `trace::run::Cache`; `Cache::entry_path()` names the entry a run reads |
| What to watch for an answer to go stale | `api::watch::watch_paths(entry)` over an entry path, giving `Watched { path, kind }` capped at `api::watch::MAX` |
| Flake references | `api::flake_ref`, `api::flake::{is_flake_ref, parse_attr_path, select, outputs, info, tree}`; `crate::flake` is private |

## What the host owns

A `Session` holds the state of a run: the settings, where messages go,
the tree store, the nix.conf it read, the eval-jobs walk, and the hash
mismatches a build reported. Two sessions in one process share none of it,
so you can run two evaluators side by side.

The library never writes to stdout or stderr by itself, and never exits. A
session's log starts with nowhere to go, `iets` calls
`Session::log.write_to_stderr()`, and a host that renders its own errors
uses `api::report_of` and leaves the log unset.

An `Evaluator` keeps a handle to its session (`ev.session()`). Other code
calls `Session::current()`, the session this thread entered:

```rust
let session = iets::session::Session::new();
session.load_settings(&[])?;            // iets.toml, optional
let _entered = session.enter();         // for this thread
let mut ev = Evaluator::new();          // adopts the session
```

To skip the thread-local, use `EvalConfig::new().session(session)`.

An evaluator has no setters: put each setting on the `EvalConfig` before
`build()`. Two defaults differ from the command line, because a host has no
command line and no entry file: evaluation is impure, and there is no project
root. `api::run::Run::new` reads the `pure` setting and finds the root from
the arguments, so a frontend built on `Run` behaves as
`docs/reference/settings.md` says. With your own `EvalConfig`, set
`pure(true)` and `project_root(…)` yourself.

Threads that the library starts inherit the session of the thread that
started them. For your own threads, call `Session::enter` on each, or use
`session::bind(f)`. A thread that entered no session uses a fallback session
that writes nowhere. `docs/how-to/embed-the-evaluator.md` has the steps with
code.

## Sessions and memory

Every value an evaluator makes, and every name it interns, lives in its
session's memory (`Session::heap`). Nothing in it is freed while the session
lives. All of it is freed when the last handle goes: your `Arc<Session>`,
and each `Evaluator` made in the session. A `parallel::try_run` holds the
session only until it returns. The fallback session lives as long as the
process.

A `Value<'e>` borrows its evaluator, so the compiler stops you from keeping
one too long. A `runtime::Word` from `api::evaluate`, and an error that
carries one, are plain data: keep them only while the session lives. If you
use one later, the process stops with
`error: a value was used after its session was released`.

Released memory stays reserved and inaccessible, so its addresses are never
reused and a stale value never reads another session's data. That costs
address space and kernel mappings, not memory: a host that releases many
thousands of large sessions can reach `vm.max_map_count`. The handler that
prints the message replaces Rust's `SIGSEGV` handler.

## The value API

`api::value` is the safe way to look at what an evaluation produced
(`examples/value.rs`). `value::Eval<'e>` wraps a `&'e mut Evaluator`.
`value::Value<'e>` is one value of that evaluator: a `Copy` machine word
whose methods never reach into the arena by themselves. A `Value<'e>`
exists only while the evaluator is borrowed, so it can't outlive the arena it
points into, and it is invariant in `'e`, so it never widens to another
borrow. You never see a `runtime::Word`, an arena pointer or a tag.

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

- A getter on a value that is not forced yet reports `ValueKind::Thunk`
  and returns `None`. It never panics and never forces, so force first.
- `as_bytes`, `as_path` and the attribute names borrow the arena without
  copying, and the getters on `Value` never allocate. The constructors copy:
  `string` and `path` copy their bytes in, and `list` and `attrs` gather a
  temporary first.
- `attr_at` uses the evaluator's internal order, as `nix_get_attr_byidx`
  warns. Sort the names yourself when the order must be reproducible. On the
  result of `//`, the first `attr_at` flattens the union, which copies it
  once.
- Keep one `Eval` per evaluator. A value passed to a different `Eval` reads
  names through the wrong symbol table, and the lifetime can't stop you.
- External values (`NIX_TYPE_EXTERNAL`) do not exist in iets, so
  `ValueKind` has no `External`.

### Mapping to Nix's C API

The value API follows Nix's own C API, so a binding for another language
has a shape to copy:

| `nix_api_value.h` | `api::value` |
|:---|:---|
| `ValueType`, `nix_get_type` | `ValueKind`, `Value::kind` |
| `nix_get_int`, `nix_get_float`, `nix_get_bool` | `Value::as_int`, `as_float`, `as_bool` |
| `nix_get_string` | `Value::as_bytes` (bytes and length, no copy) |
| `nix_get_path_string` | `Value::as_path` |
| `nix_get_list_size`, `nix_get_list_byidx` | `Value::list_len`, `Value::list_get` |
| `nix_get_attrs_size` | `Value::attrs_len` |
| `nix_get_attr_byname`, `nix_has_attr_byname` | `Eval::attr`, `Eval::has_attr` |
| `nix_get_attr_byidx_lazy`, `nix_get_attr_name_byidx` | `Eval::attr_at` |
| `nix_value_force`, `nix_value_force_deep`, `nix_value_call` | `Eval::force`, `Eval::force_deep`, `Eval::call` |
| `nix_init_apply` | `Eval::apply` |
| `nix_init_int`, `_float`, `_bool`, `_null`, `_string`, `_path_string` | `Eval::int`, `float`, `boolean`, `null`, `string`, `path` |
| `nix_make_list_builder` + `nix_make_list` | `Eval::list(&[Value])` |
| `nix_make_bindings_builder` + `nix_make_attrs` | `Eval::attrs(&[(name, Value)])` |
| `EvalState *`, `nix_value *` | `Eval<'e>`, `Value<'e>` |
| `nix_value_incref`/`decref`, `nix_gc_*` | nothing: the arena is never freed |
| `nix_get_external`, `nix_init_external` | nothing: no external values |
| `nix_string_realise`, `nix_alloc_primop`, `nix_register_primop` | not mapped |

The header has nothing for the string context except
`nix_string_realise`. `ContextElem` holds the elements iets
keeps: `Opaque`, `DrvDeep`, `Built` and `Impure`. `Impure` is the marker
that a path coercion or the metadata of a source tree leaves, and
`builtins.getContext` does not show it.
