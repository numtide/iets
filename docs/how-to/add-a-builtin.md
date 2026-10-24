# Add a builtin

This guide is for contributors. You'll write a Rust function, register it
under its Nix name, and add a test that compares it with Nix.

## The function

Here's `concatMap` from `src/builtins/lists.rs`. It shows most of what a
builtin does:

```rust
fn concat_map(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.concatMap",
    )?;
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.concatMap",
    )?;
    parallel::fanout_list(ev, l);
    let mut lists = Vec::with_capacity(l.len as usize);
    for c in l.cells() {
        let r = ev.call1(f, c.get(), pos)?;
        lists.push(ev.force_list(
            r,
            pos,
            "while evaluating the return value of the function passed to builtins.concatMap",
        )?);
    }
    Ok(ev.concat_lists(&lists))
}
```

`parallel::fanout_list` is for a builtin that walks every element of a
list: it lets helper workers force the elements first. Leave it out if your
builtin stops early.

Every builtin has this type:

```rust
pub type PrimFn = fn(&mut Evaluator, &[Word], Pos) -> Result<Word, Box<Error>>;
```

You get exactly as many arguments as the arity you registered, and **none of
them is forced yet**. `pos` is the call site.

Return a forced value. The exception is a lazy application that you build on
purpose, with `ev.arena.mk_thunk_app(f, x)` or
`mk_thunk_app2(f, x, y)`. That's how `map`, `genList`, `mapAttrs` and
`zipAttrsWith` stay lazy.

Give each forcing helper the error context Nix uses, such as "while
evaluating the first argument passed to builtins.foo", so your traces match
Nix's.

If your builtin reads a store path, get the bytes through
`src/builtins/store_hooks.rs`. Its module comment explains how.

## Registration

Builtins live in `src/builtins/<family>.rs`. Each file has one `register`
function:

```rust
pub fn register(r: &mut Registry) {
    r.op("__stringLength", 1, string_length);   // global `__stringLength`, `builtins.stringLength`
    r.op("baseNameOf", 1, base_name_of);        // global `baseNameOf`, `builtins.baseNameOf`
}
```

The number is the arity, and names follow Nix. With a `__` prefix, the
prefixed name is a global and the plain name is in `builtins`. Without the
prefix, the name is both. A value that isn't a function, such as
`__langVersion`, goes in `install` with `r.constant(name, word)`.

For a new family file, declare it in `src/builtins/mod.rs` and call its
`register` from `install`.

## Tests

Put a case in `tests/corpus/<name>.nix`. If it needs flags, put them in
`<name>.flags`. Then record what Nix says. You need a real `nix-instantiate`
on your `PATH` for this, since iets in classic mode won't do:

```sh
scripts/corpus.nu --update <name>
```

That writes `<name>.exp` from `nix-instantiate --eval --strict`. To check
every case against iets, build a release binary and run:

```sh
scripts/corpus.nu
```

Finally, update the row for your builtin in the status table of
[Builtins](../reference/builtins.md).

The helpers that force arguments, make values and raise errors are in
[Builtin helpers](../../ARCHITECTURE.md#builtin-helpers).
