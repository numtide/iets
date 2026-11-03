# The evaluation profile

Set `IETS_PROFILE` to get a sampled flame graph of an evaluation. Use
`eval` to see where the time went, and `alloc` to see what built the
memory.

```
IETS_PROFILE=eval IETS_PROFILE_TO=/tmp/p.folded iets eval -f . -A x
flamegraph.pl /tmp/p.folded > /tmp/p.svg
```

`IETS_PROFILE_TO` says where the profile goes: a file, or `-` for stderr,
which is the default. The output is the folded form that `flamegraph.pl` and
every tool that reads its input take as it is, one line per stack:

```
frame;frame;frame <count>
```

## Weights

With `IETS_PROFILE=eval`, each sample counts the microseconds of CPU time
since the sample before it, so a stack's number is the time it spent on the
CPU. Waiting, for a download or a subprocess, does not count.

With `IETS_PROFILE=alloc`, each sample counts the arena bytes allocated
since the sample before it, so a stack's number is the memory it built.
The graph then answers "which stack made the 27 MB this run holds", which
Nix's profiler cannot. `IETS_STATS=2` answers it by kind of object,
not by call stack.

Every byte goes to the next sample, so only the bytes after the last sample
are missing: a `hello` evaluation counts about 98 % of its 27 million bytes,
and a test checks that it counts more than 90 %.

A frame is `file:line:column:name`, the call site and the lambda it
entered. A lambda with no name is `file:line:column`. A builtin is a frame
too, written `file:line:column:«__genList»`, so the time inside it is not
charged to the lambda that called it. A frame holds no `;` and no space,
so a path that has either is rewritten.

A forced thunk is a frame too, so a value's cost goes to the binding that
holds it and not only to the call that forced it. A thunk in a binding is
`file:line:column:name«thunk»`, at the binding. Any other thunk is
`file:line:column:«thunk»`, at its expression, and a `mapAttrs` entry is
`name«attr»`.

## What it costs

Without `IETS_PROFILE` a call and a return pay one branch: +0.3 %
instructions on a NixOS evaluation. A profile costs
about 0.9 % of a `hello` evaluation, measured with
`perf stat -e instructions:u`, for `alloc` as for `eval`. `IETS_PROFILE_HZ`
sets the sample rate, 99 samples a second by default.

## Profiling runs on one worker

A profile pins the run to one worker. If you set `--workers` or `workers`
to anything but 1, the run stops with an error.

Each worker can only sample its own stack, and that stack starts at the
thunk the worker was handed. At many workers you would get many short
stacks, often one frame each. At one worker, the same `hello` evaluation
gives stacks of more than 80 frames. A thunk's cost sits under whichever
caller forced it first, in its own frame.

## What it does not see

A sample is taken when a function is called, so you do not see time spent with
no call in it: parsing and compiling a file, and the inside of one long
builtin. A NAR hash shows up on its `«__hashFile»` frame, outside the loop
that does the work. Use `perf` to look inside a builtin, and `IETS_STATS` to
count each kind of work (`docs/reference/settings.md`).

A run too short for one sample writes nothing, and says so.

## Nix's own flags

iets does not implement `--eval-profiler`, `--eval-profiler-frequency` or
`--eval-profile-file`. It reads its evaluator settings only from
`iets.toml`, `--iets-option` and the `IETS_*` variables.

The folded output has the shape Nix writes, and a lambda frame is the same
`callsite:name`. One label differs: Nix writes a builtin as
`callsite:primop foo`, and iets writes `callsite:«__foo»`, so a diff of a Nix
profile and an iets profile doesn't line up on builtins.
