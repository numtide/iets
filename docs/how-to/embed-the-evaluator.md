# Embed the evaluator in your own program

> **One session per evaluation in a long-lived host.** Memory is freed when
> the session goes
> ([Sessions and memory](../reference/library.md#sessions-and-memory)). The
> API can change.

The crate is a library with a thin `iets` binary on top. Your program links
the library and calls its entry points, which
[Using iets as a library](../reference/library.md) lists. Nothing is
process-global, so evaluators can run side by side in one process.

Keep your frontend out of evaluator internals: no `cli` module, no private
field, no copied helper. If you need something that lives only in `cli`, move
it into `crate::api` and let `cli` call it there.

## Sessions

A `Session` holds what would otherwise be statics: the settings, where
messages go, the tree store and the `nix.conf` that was read.

```rust
let session = iets::session::Session::new();
session.load_settings(&[])?;            // reads iets.toml; optional
session.log.write_to_stderr();          // or leave the sink unset
let _entered = session.enter();         // for this thread
let mut ev = iets::eval::Evaluator::new();   // adopts the session
```

The library never writes to stdout or stderr on its own, and never exits.
It returns errors and your program picks the exit code. A thread you start
yourself needs `Session::enter` or `session::bind(f)`
([Using iets as a library](../reference/library.md#what-the-host-owns)).

To evaluate again and again, give each evaluation its own session:

```rust
for request in requests {
    let session = iets::session::Session::new();
    let mut ev = iets::eval::EvalConfig::new().session(session).build();
    answer(&mut ev, request);
}   // the evaluator drops here, and the session's memory with it
```

Keep no value after its session is gone. The process stops if you use one.

## Evaluation

`examples/eval.rs` is a complete frontend. Its core:

```rust
use iets::api::{self, EvalArgs, Input};

let mut a = EvalArgs { strict: true, json: true, ..EvalArgs::new() };
a.input = Some(Input::Expr("{ a = 1; b = [1 2]; }".into()));

// `Err` is the exit code; the error is already in the session log
let Ok(run) = api::run::Run::new(&a) else { return ExitCode::from(1) };
// the closure runs as the main fiber, so `force` may park while helpers work
let code = run.go(move |ev, _cache| {
    match api::evaluate(ev, a) {
        Ok(v) => { /* force and print */ 0 }
        Err(e) => { api::report(ev, &e); 1 }
    }
});
```

Run it:

```sh
cargo run --release --example eval -- -E '{ a = 1; b = [1 2]; }'
```

```
{"a":1,"b":[1,2]}
```

`Run::new` copies the settings from the arguments onto an
`eval::EvalConfig`: the lints, the IR cache, the search path. `Run::go`
builds the evaluator from it and starts the worker threads.

`Run::new` evaluates purely, as the `iets` commands do, unless
`EvalArgs::pure` is `Some(false)` or the user's `pure` setting is off. A pure
run has no `<nixpkgs>` search path and no `builtins.currentSystem`.
`api::evaluate` applies no setting, and an evaluator has no setters, so
anything you want has to go on the `EvalConfig`:

```rust
let config = iets::eval::EvalConfig::new().system("aarch64-linux").pure(true);
let run = api::run::Run::over(config, &a)?;
```

The native commands that evaluate go through `Run`, and classic mode
use `Run::on`. `parallel::try_run(workers, config.build(), f)` does the same
without the tracing cache.

## Walking values

`api::value` is the safe way to read values: `Eval<'e>` wraps the evaluator,
and `Value<'e>` can't outlive the memory it points into. It's also the only
way in, because the `runtime::Word` that `api::evaluate` returns is opaque
outside the crate.

```rust
use iets::api::value::Eval;
use iets::eval::Evaluator;

let mut evaluator = Evaluator::new();
let mut e = Eval::new(&mut evaluator);
let root = e.eval_expr(r#"rec { name = "demo"; version = 3; greet = who: "hello ${who}"; }"#, b"/")
    .and_then(|v| e.force(v))?;

println!("root is {}", root.kind().type_of());

let n = root.attrs_len().unwrap();
let mut names: Vec<Vec<u8>> = Vec::with_capacity(n);
for i in 0..n {
    let (name, _) = e.attr_at(root, i).unwrap().unwrap();
    names.push(name.to_vec());
}
names.sort();   // index order is the evaluator's own, not alphabetical
```

`examples/value.rs` is that walk in full:

```sh
cargo run --release --example value
```

```
root is set
  dir : path = /tmp
  enabled : bool = true
  greet : lambda = lambda
  name : string = "demo"
  nested : set = 1 attributes
  nothing : null = null
  ratio : float = 0.5
  tags : list = 2 elements
  version : int = 3
version + 0 = Some(3)
greet "world" = "hello world"
```

A getter on an unforced value returns `None`, so force first. Attribute
order, allocation and the `nix_api_value.h` mapping are in
[Using iets as a library](../reference/library.md#the-value-api).

## Build derivations

`examples/build.rs` evaluates to derivations, plans the upload and builds
through the dispatcher:

```sh
cargo run --release --example build -- --builder daemon -E 'derivation {
  name = "greeting"; system = "x86_64-linux"; builder = "/bin/sh";
  args = [ "-c" "echo hello from iets > $out" ]; }'
```

```
/nix/store/hl2d12ai352g88ad47fh8844g939iyrd-greeting
```

The example evaluates purely, so there's no `builtins.currentSystem`. Put
your own system in place of `x86_64-linux`. The output path depends on it.

The example drives the lower-level `Dispatcher` itself. In your own program,
`api::build::Dispatch` does the same in fewer steps:

```rust
use iets::api;
use iets::build::{render::Style, Target};

let mut found = Vec::new();
if let Err(e) = api::installable::drv_roots(ev, value, &mut found) {
    api::report(ev, &e);
    return Err("evaluation failed".into());
}
let (roots, outputs) = api::build_targets(&[], &found);
let plan = api::build::plan(ev, &roots)?;
let transport = api::build::transport(Some("daemon"), &api::build::config())?;
let prov = iets::provenance::Provenance::snapshot(ev);
let options = api::build::Options::default();
let mut d = api::build::Dispatch::open(&transport, &options, plan, prov, Style::detect())?;
let targets = roots
    .iter()
    .map(|r| Target::new(r.name(), r.clone(), outputs.clone()))
    .collect();
for o in d.submit(targets)? {
    for path in o.outputs.values() {
        println!("{path}");
    }
}
```

`Dispatch` holds the plan, the source overrides and the dispatcher, so you
never assemble an upload yourself. Given `api::build::Root` values,
`Dispatch::realise` builds them and `Built::paths(roots)` returns the output
paths.

## Read derivations

`examples/drv_info.rs` lists every derivation a run created:

```sh
cargo run --release --example drv_info -- \
  -E 'derivation { name="x"; system="x86_64-linux"; builder="/bin/sh"; args=["-c" "true"]; }'
```

```
drv	attr	position	outputs
/nix/store/k5d0ypnr0h8bd289z0mkhpq5p95mfl5w-x.drv	-	-	out
1 derivations
```

The derivations are in `Evaluator::drv.drvs`, keyed by store path, and each
`DrvInfo` carries `attr` and `pos`. `provenance::Provenance::snapshot(ev)`
maps store paths back to the local trees they were copied from. How that
works is in
[Trace a store path back to its source](../explanation/provenance.md).

## Error handling

```rust
api::report(ev, &err);          // to the session log; returns the exit code
let record = api::report_of(ev, &err);   // to render yourself
```

`report` maps store paths and positions back to the local trees they came
from. To render the record yourself, call `report_of` and leave the session's
sink unset.
