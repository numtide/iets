# Known divergences from Nix 2.34

iets aims to give you exactly what Nix 2.34.8 gives. This page lists every
place where it knowingly doesn't. Each entry says what differs, why, and how
you can see it for yourself. Classic mode adds more, listed in
`docs/reference/classic.md`.

The reference is Nix 2.34.8, the `nixVersions.stable` of the nixpkgs in
`flake.lock`. iets is not tested against a newer Nix: where Nix 2.35 or
later changed a behavior, iets keeps the 2.34.8 one unless this page says
otherwise.

## How parity is checked

`checks.conformance` runs the cases of nix-conformance against the answers
Nix 2.34.8 gave, including every entry of the regex and TOML corpora
(`lang/corpora/`). `fuzz/conformance-known` lists the cases iets fails on
purpose, each under its section of this page. The cases that need `--xml`
output are skipped (`docs/reference/fuzz.md`).

[repkgs](https://github.com/Mic92/repkgs), a package set apart from
nixpkgs, checks whole-set parity. On
2026-10-10, at iets c7766f63 and repkgs 06a70b5, `iets eval-jobs` and
nix-eval-jobs 2.35.2 gave the same `drvPath` for every package:

| system | packages | same `drvPath` | fail on both, with the same error |
|:---|---:|---:|---:|
| `x86_64-linux` | 199 | 198 | 1 (`libiconv`, not for Linux) |
| `aarch64-linux` | 199 | 196 | 3 |
| `riscv64-linux` | 199 | 186 | 13 |

To reproduce it, with a repkgs checkout next to this repository:

```sh
nix-eval-jobs --impure -I repkgs=../repkgs bench/workloads/repkgs.nix > nej.jsonl
iets eval-jobs --impure --no-instantiate -I repkgs=../repkgs bench/workloads/repkgs.nix > iets.jsonl
scripts/evaljobs-cmp.nu nej.jsonl iets.jsonl
```

For another system, give both tools
`-E 'import <repkgs> { system = "aarch64-linux"; }'` (`--expr` for
nix-eval-jobs) in place of the file.

## Regular expressions (`builtins.match`, `builtins.split`)

Nix compiles patterns with libstdc++ `std::regex` in `extended` (POSIX ERE)
mode. iets has its own engine, written from the behavior Nix shows and
the POSIX spec. The matches, the capture groups, the accept/reject set and
both error messages agree, including the
`memory limit exceeded by regular expression` limit (`a{99993}` compiles,
`a{99994}` does not).

One limit is iets's own. A match stops after
`64 × (len + 1) × insts + 2^24` steps, where `len` is the length of the
string and `insts` the size of the compiled pattern, and `builtins.match`,
`builtins.split` and `iets search` then fail with "the regular expression
'…' backtracks too much on its input". Nix has no such limit: `(a|a)*b` on
a string of `a`s runs for as long as its exponential backtracking takes,
or overflows the stack. The budget also stops a legitimate match whose
backtracking is quadratic in its input, once the input is a few kilobytes
long.

## `builtins.fromTOML`

Only the error messages differ. Nix reads TOML 1.0 (toml11 with
`toml::spec::v(1, 0, 0)`). iets uses a parser for TOML 1.1, and first
refuses what only 1.1 allows: a newline or a trailing comma inside an
inline table, the `\e` and `\xHH` escapes, and a time with no seconds.
Under `parse-toml-timestamps`, datetimes print as Nix prints them (`T`
separator, 3, 6 or 9 fractional digits).

## `builtins.fromJSON`

Errors carry a message shaped like nlohmann's (`parse error at line L,
column C: ...`) but the `last read` detail is not reproduced.

Nesting deeper than 100,000 levels is refused, because the parser
recurses and a worker stack is 256 MiB. Nix has no limit.

## The `__json` derivation attribute

iets parses the text with the `fromJSON` parser and writes it again, so
the `.drv` holds the same normal form as Nix. That parser refuses a whole
number above 9223372036854775807, which Nix keeps as an unsigned value and
writes back unchanged. Only a hand-written `__json` can hold one:
`builtins.toJSON` cannot produce it.

## Bytes that are not UTF-8 in a derivation

A Nix string is bytes. Nix writes a derivation's strings into the `.drv`
as they are. In iets, an `env` value and an argument keep their bytes,
and the `.drv` path is the one Nix gives. These places differ:

- The name of an `env` entry, `builder` and `system` must be UTF-8. Nix
  takes any bytes there; iets's `derivationStrict` stops with `string is
  not valid UTF-8`. iets holds these three as text. The drv-eval fuzz
  target meets it in each of the three (`drv-eval-builder-not-utf8`,
  `drv-eval-system-not-utf8`, `drv-eval-env-name-not-utf8`). For
  `system` this is the rule, not a limit: a system names a platform,
  and a platform name is text.
- `system` must also not hold `"`, `\`, a newline, a carriage return or a
  tab. iets stops with `the system holds '"', which a .drv cannot hold`.
  Nix writes the system into the `.drv` without escapes, so the file it
  writes cannot be read back (`"`) or reads back as another derivation
  (`\`); Nix fails too, but after it has written the file
  (nix-conformance `nix-bugs/drv-system-quote`). With a `"`, the written
  file can even parse with another builder, arguments and environment.
  `tests/lang/eval-fail-derivation-system-escape.nix` holds the case.
- `nix derivation show` writes each such byte as U+FFFD. Nix fails with
  `[json.exception.type_error.316]`.
- iets's daemon client reads the strings of a reply as UTF-8 and writes
  each byte that is not as U+FFFD. Two output names that differ only in
  such bytes are then one name. A daemon sends only valid output names
  (`[A-Za-z0-9+._?=-]`), so this happens only with a broken daemon, as in
  the wire fuzz target (`fuzz/divergences.toml`, `output-name-not-utf8`).

Observe it with `builtins.substring 0 1 "é"`, which is one byte that is not
UTF-8: as `builder = "/bin/sh" + …` the evaluation fails, as `v = …` it
gives the same `.drv` as Nix. `lang/eval-okay-derivation-non-utf8.nix` of
nix-conformance makes one derivation for each field. Such derivations exist
in real stores, and `fuzz/conformance-known` lists the case.

## Reading a `.drv` file

iets reads a `.drv` as Nix 2.34.8 does, with these exceptions. The
`drv-read` and `drv-gen` fuzz targets find each one; `fuzz/divergences.toml`
names them.

- **Bytes that are not UTF-8** in an output name, in `system`, in
  `builder` or in the name of an `env` entry: iets stops with `non-UTF-8
  string in derivation`, Nix reads the file. This is the same limit as
  in the section "Bytes that are not UTF-8 in a derivation". Real
  files in `drv/` of nix-conformance hit it.
  (`drv-string-not-utf8`)
- **A `blake3` hash:** Nix needs the experimental feature
  `blake3-hashes`; in iets every feature is on (see "Experimental-feature
  builtins"). (`drv-blake3-without-feature`)
- **A `git:` output hash or an `impure` output:** iets stops with
  `unsupported output hash method` or `impure derivations are not
  supported`. Nix reads both with the experimental features
  `git-hashing` and `impure-derivations`, which `drv-oracle` turns on.
  `drv/shape/fixed-git-sha1.drv` and `drv/shape/impure.drv` of
  nix-conformance hit it, and `fuzz/conformance-known` lists them.
  (`drv-impure-output`, `drv-git-hash-method`)
- **A store path with no dash after the hash**, such as
  `/nix/store/<hash>Xname`: Nix's `StorePath` checks the length, the hash
  characters and the name, but not the byte between them (`path.cc`).
  iets refuses the path with `missing dash`. (`store-path-without-dash`)
- **A store path that is not in canonical form**, such as
  `/nix/store/<hash>-name/` or `/nix/store//<hash>-name`: Nix's
  `parseStorePath` makes it canonical first (`canonPath`,
  `store-dir-config.cc`), so it reads the path. iets refuses it with `is
  not in the Nix store`. (`store-path-not-canonical`)
- **Two outputs with one name:** Nix keeps the first
  (`drv.outputs.emplace`, `derivations.cc`), iets keeps the last. This is
  an iets bug. `drv-gen` gives each output its own name, and
  the entry `drv-duplicate-output` hides every difference in the outputs.

## `builtins.genericClosure`

The result list and its order match. When two keys cannot be compared,
the error can name another pair, with other trace lines, because iets
keeps the keys in its own structure.

## `builtins.break`

Nix returns a copy of the unforced argument thunk; inside a list,
`nix-instantiate --eval --strict` then prints that element as `<CODE>`
(`[ (builtins.break { a = 2; }) ]` gives `[ <CODE> ]`). iets forces the
argument and prints the value. Selecting from the result behaves the same
in both.

## A local binding of an operator's builtin

`parser.y` desugars `a - b` to `__sub a b`, `a * b` to `__mul a b`, `a / b`
to `__div a b`, `-a` to `__sub 0 a` and the four comparisons to
`__lessThan`. A lexical binding of one of those names therefore changes the
operator in Nix: `let __sub = a: b: 42; in 5 - 1` gives 42. iets compiles
the operators to their own operations and gives 4.

A `with` doesn't reach them in Nix either. Only a `let`, a `rec` set or a
lambda parameter can shadow them, and no file of the pinned nixpkgs binds
any of the four. Matching Nix would mean a scope lookup for every arithmetic
and comparison operator at compile time.

## Nesting depth of a file

Nix reports `stack overflow (possible infinite recursion)` from its SIGSEGV
handler when the C stack runs out. iets counts the nesting and gives the
same message above 50,000 levels, so no input makes it crash. Nix accepts
more: 400,000 nested lists parse there and stop only in the printer.

## A NUL byte outside a string

Nix's lexer gives each byte that no rule takes as a token of its own
(lexer.l, the last `{ANY}` rule), and the byte 0 is the end-of-input
token of the bison parser. So a NUL byte outside a string or a comment
ends the file for Nix: `1<NUL> + garbage` parses as `1`. iets refuses the
NUL with `unexpected invalid token` at the byte itself, because matching Nix
would read part of a file in silence. Inside a string both
give the same `cannot be represented as Nix string` error, and in a
comment both ignore it.

`fuzz/divergences.toml` holds this case as `nul-outside-a-string`.

## Parallel evaluation

A helper fiber starts at call depth zero. A program that fails only because
it exceeds the maximum call depth (10000) in a sequential run can succeed
in parallel.

`--workers 1` evaluates sequentially
(`docs/explanation/evaluation.md`).

## Which of several clashes an error names

A file with two or more duplicate formal arguments, or two or more
attributes that clash when Nix merges nested sets, is refused by both,
with the same message. The name in the message can differ. Nix sorts the
formals by symbol to find a duplicate (`validateFormals`,
parser-state.hh) and merges a set's attributes in symbol order
(`addAttr`, the same file). A symbol's place in that order is the time
the evaluator first saw the name, and the names of the builtins come
first: in `{ lib, fetchurl, lib, fetchurl }: 1` Nix names `fetchurl`.
iets names a clash by its own symbol order. With one clash both name the
same attribute at the same place.

`fuzz/divergences.toml` holds this case as `which-duplicate-formal`
and `which-duplicate-attribute`.

## Which attribute's error comes first

When two attributes of a set both fail, `==`, `builtins.deepSeq` and
`--strict` report the one they force first. When one attribute differs
and another fails, `==` gives `false` or the error by the same order.
Both evaluators go through a set in symbol order, as in the entry "Which
of several clashes an error names": a name's place is the time the
evaluator first saw it.

Nix 2.34.8 creates 52 symbols before any other (`StaticEvalSymbols`,
eval.hh), among them `""`, `key`, `system`, `builder`, `args`, `meta`,
`self` and `description`. iets creates the same 52 first, in the same
order, so both report `key` here:

```nix
{ zz = throw "zz"; key = throw "key"; } == { zz = 1; key = 1; }
```

iets creates the names `success`, `revCount` and `version` when it starts,
and Nix only when it first needs them. With `success` in place of `key`,
Nix reports `zz` and iets `success`. After the 52, each evaluator creates
the names of its builtins in its own order. `builtins.toJSON` and
`builtins.attrValues` go through the names in sorted order, so they agree.

## A `%` in an `or` argument

Nix warns when `or` is an argument (`f "x" or`: `or` will become an
identifier). The warning quotes the source and passes the text to
`warn` as a format string (`ExprCall::warnIfCursedOr`, nixexpr.cc). A
`%` in the quoted text then fails in boost: `f "%" or` gives
`error: boost::bad_format_string: format-string is ill-formed`, and
`f "%d" or` gives `boost::too_few_args`. With more `%` signs a boost
assertion fails and Nix aborts. This is a bug of Nix 2.34.8.
iets parses both, as Nix does without the `%`.

`fuzz/divergences.toml` holds this case as `percent-in-a-cursed-or` and
`percent-in-a-cursed-or-abort`.

## Unforced values in lazy positions

In a list element, an attribute value, a `let` binding or a function
argument, Nix makes a thunk of every expression except a literal and an
already evaluated variable (`Expr::maybeThunk`). iets keeps a value there
instead of a thunk when it costs nothing to have it: a lambda, a folded
`builtins.x`, a forced attribute of an attrset that is already forced
(but not a function, see below), and `+` or `-` on ints that are
already forced. The results are the same; only a
printer that stops at thunks can see it. `nix-instantiate --eval` without
`--strict` prints `[ builtins.nixPath (1 + 1) (x: x) ]` as
`[ <CODE> <CODE> <CODE> ]`, and iets prints `[ [ ] 2 <LAMBDA> ]`. These are
speed shortcuts, kept on purpose; `--strict`, `--json` and `nix eval`
print the same.

`==` can see them too. In Nix, two elements that come from one slot are
equal without a look inside, and a function equals only the function in
its own slot. The two operands of `==` are fresh copies, so that shortcut
never applies to them. iets uses a faster rule at every level: two values
that are the same after forcing are equal. It makes one exception: two
functions are equal only when they were the same before forcing. So
`[ (id id) ] == [ (id id) ]` and `with foo; [ bar ] == [ bar ]` are
false, as in Nix. An attribute that is a function, or that is not
forced yet, gets a thunk. Whether its attrset is forced at that point
depends on which worker got to it first. Sharing it would make
`[ foo.bar ] == [ foo.bar ]` true on some runs only.

What stays different:

- `let x = { a = throw "a"; }; in x == x` is true, where Nix throws.
- A function reached through one of the shortcuts equals itself:
  `[ builtins.add ] == [ builtins.add ]` and
  `builtins.elem builtins.map [ builtins.map ]`.
- Two thunks that give the same function, and that are both forced in
  place, count as one slot.

The answer is the same at every worker count. Nix's exact rules would cost
about 1% of the instructions of a NixOS evaluation and make no difference
on nixpkgs. The eval fuzz target can switch them on (`eq-rule`,
`docs/reference/fuzz.md`).

## `«repeated»` for a list

Sets match Nix exactly. A list can print twice where Nix prints
`«repeated»`: with `--strict`, `let l = [ 1 2 ]; in { a = l; b = l; }`
gives `{ a = [ 1 2 ]; b = [ 1 2 ]; }` in iets and
`{ a = [ 1 2 ]; b = «repeated»; }` in Nix.

Nix spots a repeated list by the `Value` that holds it
(`print-ambiguous.cc`), because a small list lives inside its `Value`.
iets has no such `Value`, so it uses the slot. Using the list itself would
turn the difference around, because Nix prints `let x = [ x ]; in x`
as `[ [ «repeated» ] ]`, which needs two slots for one list.

## The digits of a float in `toJSON`

`builtins.toJSON` writes a float with the fewest digits that read back to
the same double. Nix writes it with nlohmann's JSON library, whose Grisu2
algorithm gives that shortest form for most doubles but not for all:
`builtins.toJSON 4.200000000001e17` is `"4.2000000000009997e+17"` in Nix
and `"4.200000000001e+17"` in iets. Both read back to the same double, but
the bytes differ, so a JSON file that holds such a float has another hash.
Matching Nix needs a port of Grisu2.

`fuzz/divergences.toml` holds this case as `json-float-digits`.

## The limits of a run

Both evaluators stop on a program that asks for too much, with the same
text and exit code but at another point.

`error: stack overflow (possible infinite recursion)`, exit 1. Nix's stack
is the thread's, so `ulimit -s` sets the depth; iets gives the demand
path 4 GiB and takes a much deeper recursion. A file of 400,000 `1 + 1`
terms is an error in Nix and an answer in iets. A recursion that passes
the call depth limit of 10000 gets `stack overflow; max-call-depth
exceeded` instead.

`error: out of memory`, exit 1. Nix prints one or more
`GC Warning: Out of Memory!` lines first, and iets adds its usual
`while calling the …` trace.

`error: too many symbols`, exit 1. Above 2^30 attribute names in one run
iets stops; Nix has no limit of its own.

## Experimental-feature builtins

Nix leaves `fetchTree` out of `builtins` unless `fetch-tree` or `flakes`
is on, `fetchClosure` unless `fetch-closure` is, and `outputOf` unless
`dynamic-derivations` is (`experimental-features` in nix.conf or
`--extra-experimental-features`). Under one of Nix's own names iets does
the same, so `builtins.attrNames builtins` is the same list on both sides
whatever the features are. Its own commands have every feature, so
`builtins ? fetchTree` is true there, and code that branches on it
(flake-compat) takes the `fetchTree` branch.

The same holds for a derivation: `outputHashMode = "text"` needs
`dynamic-derivations` in Nix, and `outputHashAlgo = "blake3"` needs
`blake3-hashes`. iets gives the drvPath of each. Only with
`dynamic-derivations` does Nix check `requiredSystemFeatures`. iets always
does. The drv-eval fuzz target meets all three
(`drv-eval-*-without-feature`). Its oracle has `ca-derivations` on,
so `__contentAddressed` is compared.

`builtins.outputOf` works. The one thing a run cannot do is read back a
producer that it did not make and that is not in the store. Nix writes
each `.drv` as it evaluates, so it always finds one. iets writes nothing.
It keeps a producer this run made in memory when its output is text
addressed, which every dynamic derivation is. Any other producer is read
from `/nix/store`, and the call fails if it is not there.

Because iets answers from the derivation it holds in memory, `outputOf`
works under `nix-eval-jobs --no-instantiate` too. Nix, without the
producer's `.drv` in the store, reports `path '…-cargo-vendor.drv.drv' is
not a valid store path` for each such attribute.

`builtins.getContext` of a string whose context is a dynamic derivation
output is an error. Nix resolves the derived path first, which means
building the producer, and evaluation does not build.

## Network fetchers

`builtins.fetchurl` and `builtins.fetchGit` work, with the differences in
this section. `builtins.fetchClosure` and `builtins.fetchMercurial` always
fail in iets's own commands. They are still in `builtins`, so
`builtins ? fetchMercurial` is true, and code that checks for one before it
calls it gets the error. In classic mode the call falls back to the real
nix. `fetchTarball` and `fetchTree` work for some inputs;
`docs/reference/builtins.md` lists which.

`builtins.fetchurl` differs from Nix in these ways:

- It needs a `sha256` in every mode. Nix needs one only in pure
  evaluation, and keeps a file without one for `tarball-ttl`. In
  classic mode an impure call without a `sha256` falls back to the real
  nix. `iets prefetch URL` prints the hash.
- A hash mismatch exits with 1. Nix exits with 102.
- A download error has iets's own text, not curl's.

`builtins.fetchGit` copies a local working tree without a `ref` or a `rev`
itself. It also reads a commit of a local repository by `rev` or `ref`
itself, without `submodules`. Every other fetch runs the real `nix`, which
must be on `PATH`. It differs from Nix in these ways:

- On a cache hit, the `Git tree '…' is dirty` warning is not printed again.
- When a fetch through nix fails, you see only nix's innermost error.
- A shallow working tree with `shallow = false` goes to nix.
- In pure evaluation, an SCP-like URL (`user@host:path`) without a `rev`
  fails.

`builtins.fetchClosure` is not supported. In classic mode with the
`fetch-closure` feature, the call falls back to the real nix.

Observe it: `iets eval -E 'builtins.fetchMercurial ./.'` fails, and
`iets eval -E '(builtins.fetchGit ./.).rev'` in a clean git checkout prints
the revision without the real nix.

## Import from derivation

iets never builds during evaluation. Reading a path whose string context
names a derivation output (`import`, `readFile`, `pathExists` and the other
builtins that read a file) fails when the output is not in the store. Nix
builds the derivation first, unless `allow-import-from-derivation` is off.
In classic mode iets falls back to the real nix, which applies that
setting (`docs/reference/settings.md`). An output that is already in the
store is read, as Nix reads it, and the `ifd-local-output` lint warns:
another machine can give another result. Only an output whose path is known
without a build counts, not a floating content-addressed one or one of a
dynamic derivation. With `--no-tracing-cache`, iets does not keep the text
of a `.drv` it made, so the output of such a derivation counts as missing
unless the `.drv` is in the store. You can still `import` a `.drv` file.

## An all-digit name in an `eval-jobs` attribute path

Nix reads an all-digit part of an attribute path as a list index
(attr-path.cc). nix-eval-jobs finds a job by that path, so a job under an
attribute named `"1"` gives `the expression selected by the selection path
'1' should be a list but is a set`. `iets eval-jobs` reads the part as an
attribute name when the value is an attrset, and gives the job. A jobset
made with `listToAttrs` from numbers builds.

Observe it with `{ "1" = derivation { name = "a"; system = "x86_64-linux";
builder = "/bin/sh"; }; }` as the file of both tools.

## Flake references (`parseFlakeRef`, `flakeRefToString`, the command line)

iets parses and renders the common schemes like Nix 2.34.8 (flakeref.cc,
url.cc and the libfetchers schemes): indirect ids,
`flake:`, `github:`, `gitlab:`, `path:` and absolute paths, `git:` and
`git+…`, `tarball+…`, `file+…` and plain `http(s)`/`file` URLs, with the
query parameters each scheme keeps. For valid input the output is Nix's.

iets accepts some flake refs Nix rejects. The checks whose only effect
is to refuse invalid input are left out, so these parse or render where
Nix fails:

- a character boost.url does not allow where it stands (`github:a/b/x y`,
  `https://x.org/é`), and a `%` followed by `+` and a hex digit;
- a branch or tag name libgit2 refuses (`a..b`, `-x`), a `rev` next to a
  `ref`, a repeated parameter, an indirect id or `host` outside Nix's
  patterns;
- an attribute or query parameter the scheme doesn't allow (`path:/a?x=1`
  keeps `x`), a `revCount` or `lastModified` that is no number (dropped), an
  attribute of the wrong type (ignored), and a `rev` or `narHash` Nix can't
  parse (kept as given);
- a `file://` URL with a host, a path component with an encoded `/`, and a
  `path` whose URL starts with `///` (Nix aborts on it);
- a string that is not UTF-8 or decodes to a NUL byte (read lossily).

Where iets errors, the wording is its own. Classic mode falls back
to Nix for each of these:

- `sourcehut:`, `hg+…` and any other scheme or type;
- a URL authority with a character that is not unreserved (an IP literal
  `[::1]`, a `%XX` escape, a sub-delim), and a port that is not 1-65535;
- a git `url` in `flakeRefToString` that is not a URL (scp-style
  `git@host:x`, a plain path).

`flakeRefToString` on a git input leaves out `keytype`, `publicKey` and
`publicKeys`, which Nix renders only with the `verified-fetches`
experimental feature on.

## A root flake pinned by `rev`

`git+file:///path?rev=SHA` is the tree of that commit, as it is in Nix
(`docs/reference/flakes.md`). Its `flake.nix` and `flake.lock`, however,
iets reads from the checkout at `/path`, not out of the commit: the lock
decides what the evaluation fetches, so it is read before any tree is
built. A checkout whose tracked `flake.nix` or `flake.lock` differs from
the commit it is pinned to therefore evaluates against the files on disk,
while Nix uses the commit's. CI pins a checkout to its own `HEAD`, where
the two are the same file.

## `--reference-lock-file`

Nix locks the flake again with the given file as the starting point, so it
can lock a missing or changed input in memory. iets takes the file as the
lock, which must be complete for the inputs `flake.nix` declares, like the
flake's own `flake.lock`. Neither writes the file. The path belongs to the
tracing-cache key (`docs/reference/classic.md`).

## A stale `flake.lock`

Nix computes the lock again and refuses one it would have to change
(`--no-update-lock-file`). iets compares the lock with the root's
`flake.nix` only: the `url` and `follows` of each input, and those it sets
for its inputs' inputs (`docs/reference/flakes.md`). It accepts a stale
lock that only a full lock computation finds:

- a `follows` that an input's own `flake.nix` no longer declares;
- a lock entry for an input `flake.nix` no longer declares;
- an input given as a path value (`url = ./sub`), as attributes
  (`type = "github"`), or as a url iets cannot parse.

nix-conformance's `flake/fail-*` cases refuse the stale locks the root's
`flake.nix` can show.

## Tarball inputs

The tree of an archive input is the git sink's
(`docs/reference/flakes.md`), member for member: a `..` in a name, a
member below a symlink and an unknown type flag give what Nix gives, not
an error. Nix's other archive path, `nix store prefetch-file --unpack`,
refuses those three, so `iets prefetch --unpack` takes archives that
command refuses. It is the rule a `narHash` in a lock file was computed
with.

Nix has no setting for these limits, so they have no counterpart there.
`fetch-max-size` bounds the spool and everything a sparse member
expands to together, so an archive of ten kilobytes cannot write two
gigabytes. A stream with over eight layers of compression is refused;
libarchive stacks its filters without a bound.

## `revCount` of a source tree

The value of `revCount` is that of Nix, and the attribute is there when
Nix gives one: a clean tree with its full history. It differs in these ways:

- **A read gives a warning.** `revCount` is deprecated in iets. The lint
  `rev-count` (`docs/reference/lints.md`) reports a read of
  `self.revCount` on stderr, at the level `warn` by default. The
  `revCount` of a locked input is silent unless `[lint.inputs.<node>]`
  gives the lint a level. Nix gives no warning. stdout, the `.drv` paths
  and the exit code do not change.
- **The count is computed when the evaluation reads it.** Nix 2.34.8
  counts the revisions of a git working tree before the evaluation
  (git.cc:getAccessorFromWorkdir). iets counts when the evaluation reads
  the attribute, as Nix 2.35 does (fetchTree.cc:emitLazyAttrThunk). So an
  error of the count (a repository that lacks a commit) comes at the read.
  An evaluation that does not read the attribute has no such error.

Observe it: in a git repository, evaluate `toString self.revCount` from a
flake with `iets eval` and with `nix eval`. stdout is the same, and only
iets prints the `rev-count` warning. `--lint rev-count=off` removes it.
`tests/e2e_flake.rs` compares the value with Nix on a clean tree, a dirty
tree and a shallow clone.

## Pure evaluation of a project with no flake

A native command evaluates purely by default, where `nix-instantiate`,
`nix-build` and `nix-shell` do not, and a pure run reads below its project:
the working tree the entry file is in, else the nearest `flake.nix`
directory, else the outermost `iets.toml` one, else the entry's own
directory (`docs/explanation/flakes-and-sources.md`). Nix has no such root.
The nearest thing it has is `restrict-eval` with `allowed-uris`, which iets does
not implement.

These differences follow, and `nix` needs `--impure` to be compared against
any of them:

| | Nix, pure | iets, pure |
|:---|:---|:---|
| `builtins.readFile ./x` with no flake | forbidden | the file, when `./x` is in the project |
| `toString ./x` under the root flake or project | a store path | a store path under a flake, the checkout path under a project |
| `"${./x}"` under the root flake or project | a path inside the tree copy | the same under a flake; one copy of that path alone under a project |

The last two keep a position, an error message and a `toString` naming the
file the user edits. A `.drv` that holds such a path does not survive
another checkout location, so
`impure-path-in-drv` and `absolute-path-in-drv` are errors by default
(`docs/reference/lints.md`).

The user setting `pure = false` restores Nix's defaults for the native
commands. A project `iets.toml` may not set it
(`docs/reference/settings.md`).

## A fetch from the local file system in pure evaluation

Nix's pure evaluation fetches from anywhere on the machine when a hash
locks the result. iets treats such a fetch as a read: its source may not
leave the root, which is the tree of the flake on the command line or the
project, and it is read through that tree
(`docs/explanation/pure-eval-and-local-sources.md`). These fail in iets
with `access to absolute path '...' is forbidden in pure evaluation mode
(use '--impure' to override)` where Nix evaluates, under every command name:

| Source outside the root | Nix, pure | iets, pure |
|:---|:---|:---|
| `fetchurl` or `fetchTarball` of a `file:` URL, with a `sha256` | the file, or its hash in a `hash mismatch` error | forbidden |
| a `tarball` input with a `file:` URL | the tree | forbidden |
| a `path` input, and `getFlake "path:DIR?narHash=H"` | the tree, or its hash in a `NAR hash mismatch` error | forbidden |
| a `git` or `hg` input, and `fetchGit` with a `rev`, of a local repository | the commit | forbidden |

A flake that locks `path:/abs`, `git+file:///abs` or `file:///abs.tar.gz`
outside its own tree therefore needs `--impure`. A relative `path:` input
that stays in the root is not affected. The check comes before `/nix/store`
and the input store are asked, so it does not depend on what a cache holds.

Inside the root two things differ:

- A source below the root flake's directory is read through the root tree,
  so the tree's filter applies. An untracked file is `not tracked by Git`,
  and a `path` input there has the `narHash` of its tracked files, where
  Nix copies the directory whole.
- A link is resolved on the text of the path, and a link that leaves the
  root is forbidden. This holds for a plain read in a project too:
  `readFile`, `readDir`, `hashFile`, `builtins.path`, `import`, the copy of
  `"${./x}"`, and `readFileType` for a link above the last component, which
  it reports as `symlink` as Nix does. `import` of a link that leaves the
  root tree of a flake is forbidden as well.

Observe it: `iets eval -E 'builtins.fetchurl { url = "file:///etc/hostname";
sha256 = ""; }'` is forbidden, and `nix eval --expr` of the same prints the
hash of the file. `tests/pure.rs` has the cases.

## Daemon protocol versions

Nix 2.34.8's client talks to a daemon of protocol 1.18 or newer
(`WorkerProto::minimum`, worker-protocol.cc). iets needs 1.35 and refuses
an older daemon at the handshake with
`the Nix daemon speaks protocol 1.N; this client needs 1.35 or newer`.
Older daemons lack the trust flag and some reply shapes this client
relies on, so the `wire` fuzz target (`docs/reference/fuzz.md`) makes
daemons of 1.35 and newer only. The `wire/connect-1.*` cases of
nix-conformance below 1.35 are in `fuzz/conformance-known`.

## Padding in a NAR from the daemon

For `NarFromPath`, Nix 2.34.8's client copies the NAR with `copyNAR`
(src/libutil/archive.cc). It parses into a `NullFileSystemObjectSink`,
which skips the contents of a file with their padding and does not check
that the padding is zero. iets reads the NAR in full and refuses
`bad NAR: non-zero padding`, as Nix does when it unpacks the same NAR
(`restorePath`). The wire fuzz target lists the case as
`nar-content-padding`.

## A store path without a dash

Nix 2.34.8 takes the name of a store path from the bytes after the 32
characters of the hash part, and does not check that the byte between them
is `-` (path.cc:StorePath::StorePath). So `/nix/store/<hash>6n-` is a
valid store path to Nix, with the name `n-`. iets refuses it with
`missing dash`. The wire fuzz target lists the case as
`store-path-without-a-dash`.

## The tracing cache in classic mode

`nix eval`, `nix build`, `nix derivation show`, `nix develop`,
`nix print-dev-env`, `nix search`, `nix fmt` and `nix-build` answer from
iets's tracing cache (`docs/explanation/caching.md`,
`docs/reference/classic.md`). Nix's
own evaluation cache for flakes is keyed on the locked fingerprint and is
off for a dirty tree, `-f` and `--expr`; iets's is keyed on the command and
validated by what the run read. stdout and the exit code of a hit are those
of the run that wrote the entry, which `tests/classic_cache.rs`
compares with Nix 2.34.8.

On stderr:

- **An invalidation is silent.** The native commands say
  `re-evaluating: PATH changed` when an entry is out of date. Nix says
  nothing when its cache misses, and classic mode prints what Nix prints,
  so under them the line does not reach stderr, in text or under
  `--log-format internal-json`. The record (level 3, code
  `cache-invalidated`) still goes to the `json-log-path` file, which is where
  to look when a classic command evaluated and you want to know why.
- **A hit prints no `trace:` and no `evaluation warning:` line.**
  `builtins.trace` and `builtins.warn` write while the evaluation runs, and a
  hit does not run one. Nix behaves the same way when its cache answers
  (`nix build`, `nix develop`). It differs for `nix eval` and `nix-build`,
  which Nix never answers from its cache, so there every run prints them and
  only iets's first run does.

Observe it: run `nix eval .#x` twice on a flake whose `x` is
`builtins.trace "evaluated" 1`. The first run prints the trace line and `1`,
the second prints `1`. Edit `flake.nix` and the trace line is back, with no
`re-evaluating:` line before it. `--option json-log-path FILE` puts the
record in `FILE`.

## Flags and settings with no effect

iets does not accept a flag or a setting in silence. A flag or setting
that has no effect falls into one of these classes:

1. **It holds by construction.** Nix's behavior is already what the flag
   asks for. No message.
2. **iets implements it**, or sends it to the daemon as a Nix client does.
3. **Classic mode falls back** to the real nix
   (`docs/reference/classic.md`).
4. **One warning per key and process**, with the reason. Nix prints nothing
   here, so each warning is a divergence on stderr. stdout and the exit code
   do not change. The record is level 1 with the code `ignored-flag` or
   `ignored-setting` (`docs/reference/errors.md`). It appears under
   `--log-format internal-json` and in the `json-log-path` file. An
   `eval-jobs` worker does not repeat the warning of its parent. A run that
   the tracing cache answers gives the warning too, because it comes from the
   command line and the settings. It can come before a fallback.

Class 4 covers these flags:

| Flag | Where | The reason in the warning |
|:---|:---|:---|
| `-v`, `--verbose` | `nix-instantiate`, `nix-build`, `nix-shell`; `-vv` and more fall back | iets has one log level, the one Nix starts at |
| `--verbose`, `--debug` | `eval-jobs` | the same |
| `--accept-flake-config` | `nix` | iets never applies a flake's `nixConfig` ("A flake's `nixConfig`") |
| `--eval-store auto`, `daemon`, `""` | `nix`; any other value falls back | iets evaluates in memory and needs no evaluation store |
| `--readonly-mode` | `iets build`, `eval-jobs`, `print-dev-env` | the command writes to the store; `eval` and `eval-jobs --no-instantiate` do not |

Class 1 covers these flags:

| Flag | Why |
|:---|:---|
| `--indirect`, `--add-drv-link` | Nix does nothing with them either (`nix-build.cc`: "obsolete"; `nix-instantiate.cc`: an empty statement). |
| `--no-gc-warning` on `nix-build`, `nix-shell` | Only `nix-instantiate` prints that warning. `nix-instantiate` implements the flag. |
| `--no-location` | It changes `--xml` output only, and `--xml` falls back. |
| `--readonly-mode` on `iets eval` | `iets eval` writes to no store. |
| `-j`, `--cores`, `-k`, `-K` on `nix eval`, `nix-instantiate`, `nix derivation show`, `eval-jobs` | These commands build nothing: iets never builds during evaluation. The building commands send the four values to the daemon. |
| `--experimental-features`, `--extra-experimental-features` | Every feature is always on (see "Experimental-feature builtins"). `parse-toml-timestamps` is the exception: `iets eval` applies it, and a classic `nix` falls back. |
| `--show-trace`, `--log-format bar` | iets always prints the full trace, and renders builds its own way; `docs/reference/classic.md` lists both. |

These flags are not ignored:

- `nix develop -j N`, `--cores N`, `-k` reach the daemon that builds the
  shell.
- `nix-instantiate --store URL` falls back when `URL` names another
  daemon; `--eval` does not need a store and stays.

### Settings

`docs/reference/settings.md` has the rules. A setting that acts in the Nix
client and that iets has nothing for falls back in classic mode and
gives one `ignored-setting` warning in iets's own commands. A store setting
given to `iets build`, `eval-jobs` or `print-dev-env` with `--option` gives
one too, because those commands send a daemon no settings.

### A flake's `nixConfig`

Nix applies the `nixConfig` attribute of the top-level flake: always for a
short list of harmless settings (`bash-prompt` and its two relatives,
`flake-registry`, `commit-lock-file-summary`), for the others after a
question, a saved answer in `~/.local/share/nix/trusted-settings.json`, or
`--accept-flake-config`. Without any of these it prints `warning: ignoring
untrusted flake configuration setting 'NAME'`.

iets never reads `nixConfig`. This is the safe side of the security
decision that `--accept-flake-config` makes: no flake can change a setting.
A flake that needs its `nixConfig` behaves differently:

- `extra-substituters` and `extra-trusted-public-keys` are not used, so
  iets builds what Nix would download.
- `allow-import-from-derivation`, `sandbox` and the other settings keep the
  value of `nix.conf`.
- `bash-prompt` does not change the prompt of `nix develop`.
- iets does not print Nix's `ignoring untrusted flake configuration
  setting` warning.

`--accept-flake-config` and `accept-flake-config = true` each give one
warning that says so when the top-level flake has a `nixConfig`. Put the
settings in `nix.conf` to get them. Without a `nixConfig` Nix does nothing
with them either, so iets says nothing. A cache hit gives the warning as
the run it replays did; `nix-eval-jobs` gives it once, not once per
worker.

Observe it: `nix eval --accept-flake-config .#x` on a flake with
`nixConfig.extra-substituters`. stdout and the exit code are Nix's, and
stderr has the `ignored-flag` warning. `tests/classic_eval.rs` has the
case.

## GC roots the shells and the builds keep

`nix-shell`, `nix develop` and `nix fmt` keep a GC root of the closure they
entered or built, and `nix-build --no-out-link` (`nix build --no-link`)
keeps one of every output it was told not to link. Nix keeps none of these:
after its run the output is collectable, which is why a second shell
rebuilds it and why nix-direnv and flake-fmt exist. The roots live in
`~/.cache/iets/roots`, with a 30-day TTL and `iets cache gc`
(`docs/explanation/caching.md`); `shell-gc-root` and `build-gc-root` turn
them off (`docs/reference/settings.md`).

`--no-out-link` is deliberate. iets reads the flag as "no symlink here", so
you can build something, use the path it printed and keep no `result` in the
tree. In Nix the output can be collected while you are still using it. The
cost is disk: the build pins its output until the TTL sweeps the link.

Observe it: `nix-build --no-out-link -E '<drv>'` prints the same path and
the same exit code as Nix, with no symlink on either side. Then
`nix-store --query --roots <path>` names a link under `~/.cache/iets/roots`
for iets's run and nothing for Nix's. `tests/classic_build.rs` compares
both runs byte for byte and checks the root in the same case.

## The outputs of `iets build --builder`

Nix has two ways to build elsewhere. With `--builders`, the build hook
copies the outputs back (`src/nix/build-remote/build-remote.cc`), and
`nix build` links `result`. With `--store URL`, the outputs stay on that
store, and `nix build` makes no link (`createOutLinksMaybe` in
`src/libcmd/command.cc`).

`iets build --builder URL` acts like `--store`: without `-o`, the outputs
stay on the builder. This is deliberate. A CI run needs the verdict, not
the bytes. With `-o PATH`, it acts like `--builders`: the closure comes
back, and `PATH` links each output with Nix's names. Classic mode
follow Nix: `nix build` and `nix-build` always bring the outputs back
(`docs/reference/classic.md`).

## Error text on stderr

iets's error messages usually give Nix's first line and a position, but less
around them. Nix prints the source lines at the position with a caret, and
for a missing attribute it suggests the closest name. iets prints neither:

```
$ nix-instantiate --eval -E '{abc=1;}.abd'
error: attribute 'abd' missing
       at «string»:1:1:
            1| {abc=1;}.abd
             | ^
       Did you mean abc?

$ iets eval -E '{abc=1;}.abd'
error: attribute 'abd' missing
       at «string»:1:1
```

The trace lines (`… while calling …`) can differ in number, order and wording.
stdout and the exit code are the same.

## Errors, exit codes and `--log-format internal-json`

`docs/reference/errors.md` has the codes and the record format. These
things differ from Nix 2.34:

- **Build exit codes.** `iets build` exits 100, 101, 102 or 104 on a
  failed build (`failingExitStatus`) through a daemon, where Nix exits 1.
  Nix computes these codes only when it builds in its own store. The
  classic mode exits 1, as Nix does as a daemon client.
- **Usage errors** exit 2. Nix exits 1, and so does
  classic mode.
- **Unsupported features** exit 3. Classic mode falls back to Nix.
- **`msg`** holds iets's plain text: no ANSI colors, no source
  excerpt. `raw_msg`, `line`, `column` and `file` are the fields to
  compare; they differ from Nix where the text differs. An error that
  Nix reports without a position and with the position in its trace,
  such as `throw`, has the position here.
- **`code` and `hint`** are extra keys; Nix has neither.
- **A failed build's record** has the position of the derivation; Nix's
  has none.
- **eval-jobs** puts out a record for each failed job; nix-eval-jobs
  prints those errors as plain text even under `internal-json`.
- **Activity records** (`start`, `stop`, `result`; see
  `docs/reference/errors.md`) have Nix's shape, and nix-output-monitor reads
  them. What differs:
  - Ids are new ids of the iets process; Nix passes the daemon's ids on.
  - iets doesn't send `QueryMissing`, so there is no `querying info about
    missing paths` activity and no `these N derivations will be built:`
    text. `iets build --dry-run` prints the plan.
  - Nix writes the `.drv` files during evaluation with no activity. iets
    uploads them before the build, under `copying N paths` activities, and
    reports paths only.
  - Under `internal-json` the build renderer's lines aren't put out; the
    records carry them.
  - With a `grpc://` builder iets makes the records itself.
  - The evaluation is an `evaluating …` activity of type 0. Nix has none.
  - A flake input iets downloads itself has a `downloading '<url>'`
    activity, and one fetched by the nix on `PATH` has a `fetching input`
    activity.
  - The `--timings` and `IETS_STATS` lines stay plain text.
- **`iets build --dry-run --json`** prints the plan (`willBuild`,
  `willSubstitute`, `unknown`, `downloadSize`, `narSize`). `nix build
  --dry-run --json` prints the installables' derived paths. The text on
  stderr is the same as Nix's.
- **`json-log-path`** applies to every command. Nix applies it to the
  `nix` command only: classic mode returns before `applyJSONLogger`
  (`src/nix/main.cc`).
- **A failed write on stdout** gives `error: write of N bytes: REASON`
  and exit 1, as it does in Nix, but `N` is the bytes of the one write
  that failed. iets writes a value and its newline together, so its
  count can be one more than Nix's. `nix-instantiate` and `nix-build`
  exit 0 on a failed write in both, because `std::cout` never reports.
