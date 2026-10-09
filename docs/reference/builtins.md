# Builtins

Every builtin of Nix 2.34.8, what it does in iets, and how to add one.

## Status

iets has every name that `builtins` has in Nix 2.34.8. The status column
says how much of each one works:

- `works`: gives what Nix gives, apart from the differences in the notes.
- `partial`: works for the inputs the notes give, and fails for the rest.
- `fails`: always stops with an error.

These rules apply to every builtin:

- **No import from derivation.** A builtin that reads a path fails when the
  path is the output of a derivation, because evaluation builds nothing.
  `import` of a `.drv` file works.
- **Classic mode falls back.** When a builtin fails because iets does not
  support the call, a classic name (`nix-instantiate`, `nix`, ...) runs the
  real Nix instead (`docs/reference/classic.md`).
- **Pure evaluation reads the project.** A pure run of a native command can
  read files below the project root, where Nix forbids them
  (`docs/reference/divergences.md`).

Each note that names a difference has a section in
`docs/reference/divergences.md`.

| name | status | notes |
|---|---|---|
| `abort` | works | |
| `add` | works | |
| `addDrvOutputDependencies` | works | |
| `addErrorContext` | works | |
| `all` | works | |
| `any` | works | |
| `appendContext` | works | |
| `attrNames` | works | |
| `attrValues` | works | |
| `baseNameOf` | works | |
| `bitAnd` | works | |
| `bitOr` | works | |
| `bitXor` | works | |
| `break` | works | Forces its argument. Nix returns the argument unforced. |
| `builtins` | works | |
| `catAttrs` | works | |
| `ceil` | works | |
| `compareVersions` | works | |
| `concatLists` | works | |
| `concatMap` | works | |
| `concatStringsSep` | works | |
| `convertHash` | works | |
| `currentSystem` | works | Not there in pure evaluation, as in Nix. |
| `currentTime` | works | Not there in pure evaluation, as in Nix. |
| `deepSeq` | works | |
| `derivation` | works | See `derivationStrict`. |
| `derivationStrict` | works | An `env` name, `builder` and `system` must be UTF-8. Nix accepts any bytes. |
| `dirOf` | works | |
| `div` | works | A `let` binding of `__div` does not change `/`. In Nix it does. |
| `elem` | works | |
| `elemAt` | works | |
| `false` | works | |
| `fetchClosure` | fails | Under a classic name, there only with the `fetch-closure` feature, as in Nix. |
| `fetchGit` | fails | |
| `fetchMercurial` | fails | |
| `fetchTarball` | partial | Needs a `sha256`. |
| `fetchTree` | partial | Type `path` works for an absolute path. Types `github`, `gitlab`, `sourcehut`, `git`, `tarball` and `hg` need a `narHash`. Other types fail. Under a classic name, there only with the `fetch-tree` or `flakes` feature, as in Nix. |
| `fetchurl` | fails | |
| `filter` | works | |
| `filterSource` | works | |
| `findFile` | works | |
| `flakeRefToString` | works | Accepts some references that Nix refuses. A `git` input that is not a URL fails. |
| `floor` | works | |
| `foldl'` | works | |
| `fromJSON` | works | Error text differs. Refuses nesting deeper than 100,000 levels. |
| `fromTOML` | works | TOML 1.0, as in Nix. Datetimes need the `parse-toml-timestamps` feature. Error text differs. |
| `functionArgs` | works | |
| `genericClosure` | works | When two keys cannot be compared, the error can name another pair. |
| `genList` | works | |
| `getAttr` | works | |
| `getContext` | works | Fails on the output of a dynamic derivation. Nix builds the producer first. |
| `getEnv` | works | |
| `getFlake` | partial | Impure evaluation only. Needs a local flake with a complete `flake.lock`. |
| `groupBy` | works | |
| `hasAttr` | works | |
| `hasContext` | works | |
| `hashFile` | works | |
| `hashString` | works | |
| `head` | works | |
| `import` | works | Also imports a `.drv` file. |
| `intersectAttrs` | works | |
| `isAttrs` | works | |
| `isBool` | works | |
| `isFloat` | works | |
| `isFunction` | works | |
| `isInt` | works | |
| `isList` | works | |
| `isNull` | works | |
| `isPath` | works | |
| `isString` | works | |
| `langVersion` | works | `6`. |
| `length` | works | |
| `lessThan` | works | A `let` binding of `__lessThan` does not change `<`. In Nix it does. |
| `listToAttrs` | works | |
| `map` | works | |
| `mapAttrs` | works | |
| `match` | works | |
| `mul` | works | A `let` binding of `__mul` does not change `*`. In Nix it does. |
| `nixPath` | works | |
| `nixVersion` | works | `"2.34.8"`. |
| `null` | works | |
| `outputOf` | works | A producer that this run did not make must be in `/nix/store`. Under a classic name, there only with the `dynamic-derivations` feature, as in Nix. |
| `parseDrvName` | works | |
| `parseFlakeRef` | works | Accepts some references that Nix refuses. `sourcehut:`, `hg+...` and other schemes fail. |
| `partition` | works | |
| `path` | works | |
| `pathExists` | works | |
| `placeholder` | works | |
| `readDir` | works | |
| `readFile` | works | |
| `readFileType` | works | |
| `removeAttrs` | works | |
| `replaceStrings` | works | |
| `scopedImport` | works | |
| `seq` | works | |
| `sort` | works | |
| `split` | works | |
| `splitVersion` | works | |
| `storeDir` | works | |
| `storePath` | works | |
| `stringLength` | works | |
| `sub` | works | A `let` binding of `__sub` does not change `-`. In Nix it does. |
| `substring` | works | |
| `tail` | works | |
| `throw` | works | |
| `toFile` | works | |
| `toJSON` | works | Writes a float with the fewest digits. Nix sometimes writes more. |
| `toPath` | works | |
| `toString` | works | |
| `toXML` | works | |
| `trace` | works | |
| `traceVerbose` | works | |
| `true` | works | |
| `tryEval` | works | |
| `typeOf` | works | |
| `unsafeDiscardOutputDependency` | works | |
| `unsafeDiscardStringContext` | works | |
| `unsafeGetAttrPos` | works | |
| `warn` | works | |
| `zipAttrsWith` | works | |

## Adding a builtin

Primops live in `src/builtins/<family>.rs`. Each file has one entry point:

```rust
pub fn register(r: &mut Registry) {
    r.op("__stringLength", 1, string_length);   // global `__stringLength`, `builtins.stringLength`
    r.op("baseNameOf", 1, base_name_of);        // global `baseNameOf`, `builtins.baseNameOf`
    r.constant("__langVersion", Word::imm_int(6));
}
```

Add the file to `src/builtins/mod.rs` and call its `register` from `install`.
Names follow Nix: a `__` prefix means the unprefixed name appears in
`builtins` and the prefixed one is a global.

### Signature

```rust
pub type PrimFn = fn(&mut Evaluator, &[Word], Pos) -> Result<Word, Box<Error>>;
```

`args` has exactly `arity` words and they are **not forced**. `pos` is the
call site. Return a forced value (never a thunk) unless you deliberately
return a lazy application built with `ev.arena.mk_thunk_app(f, x)`, which
is how `map`, `mapAttrs`, `genList` and `zipAttrsWith` stay lazy.

### Forcing helpers (all on `Evaluator`)

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

`ctx` is `&str`; pass the Nix error context ("while evaluating the first
argument passed to builtins.foo") so traces match.

### Constructing values

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

Well-known symbols are on `ev.s` (`out_path`, `drv_path`, `type_`, `name`,
`value`, `functor`, `to_string`, ...).

### Errors

```rust
use crate::eval::{ErrorKind, bail};
bail!(ErrorKind::Eval, pos, "cannot create list of size {}", n);
```

`ErrorKind` is in `src/eval/error.rs`. `tryEval` catches `Throw` and
`Assert` only. Use `Unsupported` for a call iets does not implement, so
that classic mode runs the real Nix. Add trace lines with
`e.add_trace(pos, msg)`.

### Worked example

```rust
fn concat_map(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let f = ev.force_function(args[0], pos, "while evaluating the first argument passed to builtins.concatMap")?;
    let l = ev.force_list(args[1], pos, "while evaluating the second argument passed to builtins.concatMap")?;
    let mut lists = Vec::with_capacity(l.len as usize);
    for c in l.cells() {
        let r = ev.call1(f, c.get(), pos)?;
        lists.push(ev.force_list(r, pos, "while evaluating the return value of the function passed to builtins.concatMap")?);
    }
    Ok(ev.concat_lists(&lists))
}
```

A builtin that reads a store path gets its bytes through
`src/builtins/store_hooks.rs`. Its module comment says how.

### Tests

Put a case in `tests/corpus/<name>.nix`, with an optional `<name>.flags`.
`scripts/corpus.nu --update <name>` writes `<name>.exp` from
`nix-instantiate --eval --strict`. `scripts/corpus.nu` checks every case
against iets.
