# WebAssembly

`nix build .#wasi` gives you `bin/iets.wasm`, a `wasm32-wasip1` build of the
whole CLI, made to run in a browser. It evaluates Nix and prints the result.
It has no network, runs no builds, and sees files only where the host gives
it a directory. Stripped, it was 5.6 MiB when last measured.

The `cross-wasi` check runs it under Node's `node:wasi` with no directories,
an empty environment and a stack the size of a Chromium web worker's, which is
what a page in a browser gives it. No check runs it in a browser. By hand, it
ran in the web workers of Firefox 158 and Chromium 156 with
`@bjorn3/browser_wasi_shim` 0.4.2. Safari is not tested.

## What the host gives it

The module imports only `wasi_snapshot_preview1`, so any WASI host can run it.
Its arguments and stdin come from the host. A page runs it in a web worker
and uses a shim for those imports. The shim above has all of them.

| iets needs | in this build |
|---|---|
| threads | evaluation is sequential, and `--workers` has no effect |
| child processes | `eval-jobs` is one process, as with `--max-memory-size 0`; `git` does not run |
| network | `this platform has no network`, from each fetch of a URL |
| a daemon or builder | `iets build` cannot connect, so it fails with an error; `eval-jobs` needs `--no-instantiate` |
| files | only the directories the host preopens; without any, `import` and `readFile` find nothing |
| environment | what the host passes, which is usually nothing |

`builtins.currentSystem` is `wasm32-wasi`, in impure evaluation. Store paths
are hashes, so `drvPath` and `outPath` work with no store.

The network is out at the dependency level. `tokio`, `rustls`, `tonic` and
`mimalloc` do not compile for this target, or have no use on it, so
`Cargo.toml` leaves them out, and `src/fetch/http/client_none.rs` and
`src/remote/grpc_none.rs` take their place.

## The stack

Native iets gives evaluation a 4 GiB stack and allows 10000 calls. WebAssembly
cannot do this. It has two stacks: a shadow stack in linear memory, for locals
whose address is taken, and the engine's own stack, for everything else. The
linker sets the shadow stack, and a page cannot set the engine stack. A web
worker in Chromium 156 holds about 650 levels of plain Nix recursion on it, and
Firefox 158 holds more. When the engine stack runs out, the instance traps and
takes all of its state with it, with no iets error message.

So the limits are lower here, and iets stops before the trap:

| limit | native | WebAssembly |
|---|---|---|
| call depth | 10000 | 1000 |
| what counts toward it | function calls | calls, each evaluated expression, thunks being forced, `deepSeq` and `==` recursion |
| parse depth | 50000 | 500 |
| what counts toward it | nesting | nesting, and each link of an operator chain or attribute path |
| `fromJSON` nesting | 100000 | 500 |
| regex groups and stacked quantifiers | no limit | 500 |

Calls alone cannot bound this stack: an expression nested in a function body
uses it with no call between the levels. So each level of evaluation counts,
and one Nix call costs several. The plain recursion
`f = n: if n == 0 then 0 else 1 + f (n - 1)` stops at about 250 calls.

Chains such as `1 + 1 + 1` parse in a loop, but they are trees as deep as they
are long for each stage after the parser. So a chain of 500 is a parse error
here.

The parser, `fromJSON` and regexes can run at the bottom of a deep evaluation.
With the most that these limits allow of both, neither browser traps. Chromium
trapped at a call depth of 1250.

Over the call depth, evaluation stops with
`error: stack overflow; max-call-depth exceeded`, and exit status 1. Over the
parse depth, it stops with `error: stack overflow (possible infinite
recursion)`. `fromJSON` and regexes stop with their own errors.

The shadow stack is 8 MiB, which is more than the engine stack lets evaluation
use. The link flags in `nix/iets.nix` set it, and `--stack-first` puts it at
the bottom of linear memory, so an overflow traps and cannot write over the
heap.

The limit is the reason this build is not for nixpkgs: an evaluation of that
size goes much deeper. It suits small expressions, examples and teaching.

## Memory

Linear memory stops at 4 GiB, and `usize` is 32 bits. The symbol table holds
`1 << 22` symbols, and more gives `error: too many symbols`. The module starts
with 182 MiB of linear memory, and touches little of it. A page must be able
to allocate that much.

The allocator is the default for the target, not `mimalloc`.
