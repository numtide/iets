# Builtins API

The primop signature, the forcing helpers, the constructors, the error
kinds, and every builtin iets implements.

Primops live in `src/builtins/<family>.rs`. Each file has one entry point:

```rust
pub fn register(r: &mut Registry) {
    r.op("__stringLength", 1, string_length);   // global `__stringLength`, `builtins.stringLength`
    r.op("baseNameOf", 1, base_name_of);        // global `baseNameOf`, `builtins.baseNameOf`
    r.op_hidden("__foo", 1, foo);               // only `builtins.foo`
    r.constant("__langVersion", Word::imm_int(6));
}
```

Add the file to `src/builtins/mod.rs` and call its `register` from `install`.
Names follow Nix: a `__` prefix means the unprefixed name appears in
`builtins` and the prefixed one is a global.

## Signature

```rust
pub type PrimFn = fn(&mut Evaluator, args: &[Word], pos: Pos) -> Result<Word, Box<Error>>;
```

`args` has exactly `arity` words and they are **not forced**. `pos` is the
call site. Return a forced value (never a thunk) unless you deliberately
return a lazy application built with `ev.arena.mk_thunk_app(f, x)`, which
is how `map`, `mapAttrs`, `genList` and `zipAttrsWith` stay lazy.

## Forcing helpers (all on `Evaluator`)

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

## Constructing values

| constructor | notes |
|---|---|
| `ev.mk_int(i)`, `ev.mk_float(f)`, `Word::from_bool(b)`, `Word::NULL`, `Word::TRUE`, `Word::FALSE` | ints are immediates when they fit 61 bits |
| `ev.mk_string(&bytes)` | no context |
| `ev.mk_string_ctx(&bytes, &ctx)` | `ctx: &[Ctx]`; use `ctx_to_arena(&mut ev.arena, vec)` to sort and dedup |
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

## Errors

```rust
use crate::eval::{ErrorKind, bail};
bail!(ErrorKind::Eval, pos, "cannot create list of size {}", n);
```

Kinds: `Eval`, `Type`, `MissingAttr`, `Io`, `Throw` and `Assert` (the two
`tryEval` catches), `Abort`, `InfiniteRecursion`, `StackOverflow`. Add trace
lines with `e.add_trace(pos, msg)`.

## Worked example

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

## Store seam

`src/builtins/store_hooks.rs` connects the evaluator to the pure `store`
module: `copy_path_to_store(ev, path, pos)` returns the virtual store path of
a source path (NAR hash, `make_fixed_output_path`), cached in
`ev.src_to_store`. `paths.rs` builds `toFile`, `path`, `filterSource` and
`storePath` on `crate::store::{nar, path, hash}`; `derivationStrict` and
`placeholder` should build on `crate::store::drv`. Reading a path whose
context is a virtual store path (`readFile "${./x}"`) maps back to the
source through `src_to_store`; a derivation output in context is an error,
since nothing is ever built.

## Implemented

core: import, scopedImport, toString, typeOf, isNull/isFunction/isInt/isFloat/
isString/isBool/isPath/isList/isAttrs, seq, deepSeq, abort, throw, trace,
traceVerbose, warn, addErrorContext, tryEval, functionArgs, unsafeGetAttrPos,
hasContext, unsafeDiscardStringContext, unsafeDiscardOutputDependency,
findFile.
attrs: attrNames, attrValues, getAttr, hasAttr, removeAttrs, intersectAttrs,
mapAttrs, listToAttrs, zipAttrsWith, catAttrs, groupBy.
lists: head, tail, length, elem, elemAt, map, filter, foldl', concatLists,
concatMap, genList, any, all, sort, partition.
strings: stringLength, substring, concatStringsSep, splitVersion,
compareVersions, parseDrvName, baseNameOf, dirOf.
math: add, sub, mul, div, bitAnd, bitOr, bitXor, lessThan.
paths: readFile, readDir, readFileType, pathExists, toPath, storePath,
filterSource, path, toFile; fetchTree on a locked input and fetchTarball with
a sha256 (`flake/fetch.rs`); getFlake on a local flake (`flake/mod.rs`);
parseFlakeRef and flakeRefToString (`flake/flakeref.rs`);
fetchurl, fetchGit, fetchMercurial, fetchClosure, outputOf exist but fail
when called.
context: getContext, appendContext, addDrvOutputDependencies.
text: match, split (ERE via `text/regex.rs`, per-thread regex cache), replaceStrings,
hashString, hashFile, convertHash, floor, ceil, genericClosure, getEnv, break,
toJSON, fromJSON, toXML, fromTOML (`--extra-experimental-features
parse-toml-timestamps` for datetimes), currentTime. Divergences:
`docs/reference/divergences.md`.
exec: only with `allow-unsafe-native-code-during-evaluation`, and only impure
(`exec.rs`, `docs/reference/settings.md`). importNative is not implemented.
constants: true, false, null, builtins, currentSystem, nixVersion,
langVersion, storeDir, nixPath, derivation (from corepkgs/derivation.nix).
derivation: derivationStrict, placeholder, `import` of a `.drv` store path.
State lives in `ev.drv` (`DrvState`: hash cache, created-drv table,
`--dump-drvs` dir). `scripts/drvdiff.nu` finds the first derivation that
differs from nix-instantiate.

## Corpus tests

`tests/corpus/<name>.nix` holds one case per builtin family member, with
`.exp` captured from `nix-instantiate --eval --strict` and an optional
`.flags` file. `scripts/corpus.nu --update` regenerates the `.exp` files;
`scripts/corpus.nu` checks them against `target/release/iets`.
