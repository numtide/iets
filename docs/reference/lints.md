# Lints

A lint tells you about something in your Nix code that only an evaluator
can see, such as a read of a value that changes with every commit:

```
$ iets eval --raw .#version             # version = "1.${toString self.revCount}";
warning: [rev-count] 'self.revCount' is deprecated: the count of all revisions is expensive to compute and absent on a shallow clone, and iets plans to remove it; use 'lastModifiedDate' and 'shortRev' for a version
  at /repo/app/flake.nix:4:29
```

Each lint is `off`, `warn` or `error`. This page lists every lint, says how
a level is chosen and which files a lint looks at, and shows how to silence
one line with `# iets: allow`.

iets has no opinion about how code looks. `docs/explanation/lint-scope.md`
says why, and `docs/how-to/write-lint-rules.md` shows how to write rules of
your own with `iets parse --json`.

## Levels and resolution

| level | effect |
|---|---|
| `off` | nothing |
| `warn` | one line per (lint, position) site on stderr, `warning: [name] message` then `  at file:line:col`; deduplicated for the run |
| `error` | an evaluation error of kind `Lint`, which `tryEval` does not catch |

Nix's spellings `ignore` and `fatal` are accepted for `off` and `error`.

A finding of `rev-count` is one for each source tree in a run. Its site is the
selection that reads `revCount`. A read that is not a selection gives a
finding without a site, which has no `at` line ("The tree that a value comes
from").

With `--log-format internal-json`, or in the `json-log-path` file, a
finding is a record with the code `lint`, the byte span of the site
(`start`, `end`) and, when the lint has one, the text that corrects it
(`replacement`). A finding without a site has no span, and `file`, `line`
and `column` are null (`docs/reference/errors.md`, "A finding and its
edit").

The level of a lint comes from these sources, and a later one wins:

1. the built-in default (the severity table in this page);
2. the `[lint]` table of the user's `iets.toml`
   (`docs/reference/settings.md`);
3. the `[lint]` table of the project's `iets.toml`: the nearest one found
   by walking up from the entry file's directory, the flake directory, or
   cwd for `-E` (each `[lint.inputs.<node>]` table is a source of its own,
   see Scope); the walk stops at the project root
   (`docs/explanation/flakes-and-sources.md`); any key but `[lint]` in this
   file is an error (`docs/reference/settings.md`);
4. `IETS_LINT="name=level,name=level"`;
5. `--lint SELECTOR=LEVEL`, repeatable.

A selector is a lint name, `all`, `family:NAME` (`path`, `coerce`,
`legacy`, `purity`, `meta`), or one of Nix's setting names
(`lint-absolute-path-literals`, `lint-short-path-literals`,
`lint-url-literals`), which count as lint names. Nix's flags
`--lint-absolute-path-literals MODE`, `--lint-short-path-literals MODE`
and `--lint-url-literals MODE` work too: each is `--lint <nix
name>=MODE`.

Inside one source the narrower selector wins, whatever the order: a lint
name beats `family:NAME`, which beats `all`. A TOML table has no order, so
no other rule can hold there:

```toml
[lint]
"family:coerce" = "error"
dir-copy-flake = "warn"     # warn, in either order
```

The `--lint` flags of one command line are one source, and so are the pairs
of one `IETS_LINT`. Among settings of the same specificity the later one
wins, but a name given earlier still beats a family or `all` given later:
`--lint all=error --lint fod=off` and `--lint fod=off --lint all=error`
both give `fod=off` with everything else at `error`. Between sources the
specificity doesn't count: `all = "off"` in `IETS_LINT` turns off a lint
`iets.toml` names.

A selector that the registry does not have is handled per source:

| source | effect |
|---|---|
| an `iets.toml`, `IETS_LINT` | one warning for each name, with the code `unknown-lint`; iets ignores the name and the run continues |
| `--lint` | a usage error, exit code 2 |

```
warning: /repo/iets.toml: iets does not know the lint 'nope' and ignores it; 'iets help lints' has the list (docs/reference/lints.md)
```

A file gets a warning because an error would stop every command in that
repository, including the one that installs the correction. The command line
gets an error, because you can correct it at once. An invalid level
(`fod = "loud"`) is an error in every source.

`iets lint list` prints the registry with the levels a run gets
(`docs/reference/commands.md`).

## Scope

A level says whether a lint runs; the scope says where its sites are
reported. The default scope is `root`: a site is reported only when it
lies inside a lint root. The first root is the entry's directory: the
flake directory when the entry is a flake reference (its tracked tree,
which the evaluation also serves as a virtual copy under `/nix/store`),
else the directory of the entry file, or cwd for `-E` (the expression
itself is in root). Everything else under `/nix/store` is silent: the
locked inputs, nixpkgs, corepkgs, `«derivation-internal»`. So `--lint
all=warn` on a NixOS configuration reports the sites in that repository
and nothing from its inputs.

- `--lint-root DIR` (repeatable) adds a root; `roots = ["dir", ...]` in
  `iets.toml` does the same, relative to the file.
- `--lint-scope all`, or `scope = "all"`, reports every site.
- `[lint.inputs.<node>]`, with the same `name = level` keys (selectors
  included), gives the tree of that locked input its own levels; `<node>`
  is the lock file's node name. A lint not named there follows the scope.

A site's file is "in root" when its path, resolved through the virtual
copies to the local tree it stands for, lies under a root directory. So
`/nix/store/<hash>-source/modules/x.nix` of the root flake is in root, a
`path:` input checked out beside the repository is not, and one inside
the flake directory is (containment wins). A relative `path:` input lives
under its parent's tree and follows it.

The scope is chosen like the levels, and `--lint-scope` and `--lint-root`
come last. Both caches key on the scope, the roots and the input tables, so
a run with another scope never gets the cached warnings of another.

For a compile-time lint the site is the node's position; for a runtime
lint it is the call's position. The derivation-attribute lints are the
exception: nixpkgs's `mkDerivation` and `fetchurl` bind the attributes
of every derivation they make, so the attribute's position is in nixpkgs
even when the value came from the user's file. The value lints and the
encoding lints each have a rule, and `tree-meta-in-drv` has its own:

- The value lints (`impure-path-in-drv`, `absolute-path-in-drv`, `fod`)
  report the user's value whatever the wrapper. When the attribute's
  position is out of scope the site falls back to the derivation's own
  position: the `derivationStrict` call when it is in a root (a direct
  call), else the `name` attribute, else the first attribute of the set
  (in the set's order) whose position is in a root, which `mkDerivation`
  keeps for the attributes it passes through (`pname`, `src`, ...). One
  site per (lint, position). The evaluator keeps no call trace: a
  derivation whose attribute set holds no position from a root, such as
  a nixpkgs fetcher call (`fetchurl { url; hash }`), stays silent under
  `scope = "root"`; `[lint.inputs.nixpkgs] fod = "warn"` reports those
  at nixpkgs's site.
- The encoding lints (`drv-bool-null`, `drv-list`) report a `derivation`
  call the scope sees, and nothing else: a list or Boolean passed to
  `mkDerivation` is nixpkgs's contract, and the `__structuredAttrs =
  false` it injects is nixpkgs's binding. `derivation` is a wrapper in
  `«derivation-internal»`, so the call is known by the `derivationStrict`
  call position when that is in scope (a direct `builtins.derivationStrict`
  call), else by the position of the `builder` attribute, which every
  wrapper binds itself. The site is the attribute's own position when
  the scope sees it, else the position the call was known by.

### The tree that a value comes from

Two lints do not take their level from the site: `tree-meta-in-drv` and
`rev-count`. The tree that the value comes from gives the level:

| the value comes from | level |
|---|---|
| the root tree (`self.rev`, `self.revCount`) | the level of the lint: `warn` by default |
| a locked input (`inputs.nixpkgs.rev`) that has a `[lint.inputs.<node>]` table with this lint | the level in that table |
| a different locked input | `off` with `scope = "root"`, the level of the lint with `scope = "all"` |

The metadata of the root tree changes with each commit. That of a locked
input changes only with the lock file, when a rebuild is correct, so an input
is silent by default.

```toml
[lint.inputs.nixpkgs]
tree-meta-in-drv = "warn"   # report nixpkgs's shortRev and lastModifiedDate
rev-count = "warn"          # report a read of nixpkgs's revCount
```

A tree has a name only in a flake evaluation: `self` for the root tree,
`inputs.<node>` for an input that the lock file names. A tree from a
direct `builtins.fetchTree` call has no name, and the two lints are silent
for it.

The site of `tree-meta-in-drv` follows the rule of the value lints. A
derivation that has no position in a root is not silent, because the value is
the user's: the finding goes to the position of the attribute, in the file
that binds it:

```
$ iets eval --raw .#wrapped.drvPath     # wrapped = dep.mk self.narHash;
warning: [tree-meta-in-drv] derivation 'wrapped': attribute 'v' holds 'self.narHash', a value that changes with every commit or edit, so this derivation and all that depend on it are built again each time; keep the value out of the build, or read it at run time
  at /repo/dep/flake.nix:2:79
```

The site of `rev-count` is the selection that reads the attribute:

| the code reads the count with | site |
|---|---|
| `self.revCount`, `self.revCount or 0`, `self.sourceInfo.revCount`, `inputs.<node>.revCount` | the selection |
| a selection through a binding or an argument (`let s = self; in s.revCount`, `{ self, ... }: self.revCount`) | the selection |
| `inherit (self) revCount;` | the name in the `inherit` |
| `builtins.getAttr "revCount" self`, with a computed name too | the call |
| `with self; revCount`, a function pattern `{ revCount, ... }:` | none |
| `self.${name}` with a computed name | none |
| `builtins.attrValues`, `builtins.toJSON`, `builtins.mapAttrs` on the attribute set of the tree | none |

The selection reports when the evaluation needs its value. A binding that
the evaluation does not use (`let n = self.revCount; in 1`) gives no finding
and starts no count. A selection in the code of an input still reports a
read of the root tree, because the value is the user's.

One run gives one finding for each tree: that of the first read. A site that
a `# iets: allow` comment covers is not a finding, so the next read of that
tree reports.

A read that is not a selection (`with self; revCount`) has no site, because
iets can't tell which expression asked for the value. The message names the
tree. At the level `error`, the trace has the position of the nearest call
that has one.

## Silencing one site

A comment `# iets: allow NAME[, NAME...]` silences the named lints for
one source line, whatever their level (`warn` or `error`). The names
are selectors: lint names, Nix's setting names, `family:NAME`, or
`all`; commas or spaces separate them.

```nix
# iets: allow url-literal
src = http://example.com/src.tar.gz;

x == "a" # iets: allow eq-type-mismatch
```

- On a line of its own the comment covers the next line that is neither
  blank nor a comment. After code it covers its own line. Both forms
  on one line add up.
- The covered line is a set of positions: every site whose position
  falls on it is silent, a compile-time lint (`url-literal`) and a
  runtime lint (`eq-type-mismatch`, `fod`) alike. A site is the position
  where its expression starts: a string that opens on the covered line
  and spans ten more is silent; one that opens on the line after is not.
- A name the registry does not know is the `unknown-allow` lint (family
  `meta`, default `warn`), reported at the word's position, so a typo
  does not silently silence nothing. The known names of the same
  comment still count. `allow` is the only directive; another word
  after `# iets:` is `unknown-allow` too.
- The comment must start with exactly `# iets:`; `#iets:` and `/* iets:
  */` are plain comments.
- An allow that silences nothing is not reported: a runtime lint fires
  only when the value is computed, so an unused allow on one run may be
  the one that matters on the next.

## Configuration file

```toml
# iets.toml, in any parent directory of the file being evaluated
[lint]
path-copy = "warn"
unhashed-fetch = "error"
large-source-copy = { level = "warn", max-bytes = 1048576 }
scope = "root"          # or "all"
roots = ["../shared"]   # relative to this file

[lint.inputs.nixpkgs]   # levels inside that locked input's tree
fod = "warn"
```

A value is a level string, or a table with `level` and the lint's knobs.
The only knob is `large-source-copy.max-bytes` (default 8 MiB).

A strict repository sets everything to `error` and opens what it needs:

```toml
[lint]
all = "error"
# a fixed-output derivation is how a source is fetched
fod = "off"
# a tracked directory copied whole is worth a look but shouldn't fail the run
dir-copy-flake = "warn"
```

## Catalog

Hook: `parser` and `compiler` lints run once per file, so their levels
are part of the IR cache key and their warnings are stored in the entry;
`evaluator` lints fire when the value is computed.

| name | family | hook | default | trigger |
|---|---|---|---|---|
| `absolute-path-literal` | path | compiler | off | a path literal that starts with `/` or `~/` (Nix's `lint-absolute-path-literals`) |
| `short-path-literal` | path | compiler | off | a relative path literal without a leading `./` (Nix's `lint-short-path-literals`) |
| `url-literal` | legacy | parser | off | an unquoted URL (Nix's `lint-url-literals`) |
| `path-copy` | coerce | evaluator | off | a path coerced to a string with a copy to the store: interpolation, `+`, a derivation attribute; not `toString`. Under a flake root or a locked input `"${./file}"` is the normal reference: silent for a regular file, and a directory is `dir-copy-flake` |
| `dir-copy-flake` | coerce | evaluator | off | a directory under a flake root or a locked input coerced to a string: the copy is the tracked file set the flake machinery already filtered, so the message suggests `lib.fileset` or `builtins.path { filter }` only to narrow it further; its own lint so `family:coerce = "error"` can keep it at `warn` |
| `store-path-recopy` | coerce | evaluator | off | a path or `builtins.path`/`filterSource` argument already under the store dir copied again (`"${<nixpkgs>}"`, a `builtins.storePath` or pre-existing store path), which doubles the hash in the name; not for the flake root's or a locked input's virtual copy, whose paths already are store paths by construction |
| `read-through-copy` | coerce | evaluator | off | readFile, readDir, pathExists, readFileType, hashFile, import, `builtins.path` or `filterSource` on a store copy this evaluation made by coercion, `builtins.path` or `filterSource` (flake roots and inputs excepted) |
| `drv-bool-null` | coerce | evaluator | off | a Boolean or null coerced into a derivation attribute (`"1"`, `""`) by a `derivation` call the scope sees (Scope); null only outside `__ignoreNulls`; not with `__structuredAttrs`; never for Nix's own Boolean attributes (`__structuredAttrs`, `__ignoreNulls`, `__impureHostDeps`, `__propagatedImpureHostDeps`, `__sandboxProfile`, `__noChroot`, `__darwinAllowLocalNetworking`, `allowSubstitutes`, `preferLocalBuild`), whose encoding is the protocol |
| `drv-list` | coerce | evaluator | off | a list space-joined into a derivation attribute by a `derivation` call the scope sees (Scope); `args` and `outputs` excepted |
| `eq-type-mismatch` | coerce | evaluator | off | `==` or `!=` on values of different types, int and float excepted; a `null` on either side is the test for absence and is exempt |
| `eq-function` | coerce | evaluator | off | `==` or `!=` with a function on either side |
| `or-non-attrs` | coerce | evaluator | off | `x.a or d` where the subject of some element is not an attrset |
| `impure-builtin` | purity | evaluator, compiler | off | a `getEnv` call (evaluator); a reference to `currentTime` (compiler: the value is a thunk of the globals, without a position of its own) |
| `unhashed-fetch` | purity | evaluator | off | `fetchurl`, `fetchTarball`, `fetchGit` or `fetchTree` called with a URL string, or an attrset without `sha256`, `hash`, `narHash`, `outputHash` or `rev`; fires before the call errors |
| `ifd` | purity | evaluator | off | `import` of a string whose context names a derivation |
| `ifd-local-output` | purity | evaluator | **warn** | `import`, `readFile`, `readDir`, `pathExists`, `readFileType`, `hashFile` or another read of a derivation output that is already in this machine's store. The result depends on that store: where the output is missing, classic mode falls back to the real nix, which builds it, and a native command fails. The message names the output path |
| `fod` | purity | evaluator | off | `derivationStrict` producing a fixed-output derivation; the message names the derivation, the hash mode and algorithm |
| `large-source-copy` | purity | evaluator | off | a recursive path copy whose NAR (after filters) exceeds `max-bytes` |
| `git-in-source` | purity | evaluator | **warn** | a recursive path copy of a directory whose top level keeps a `.git` entry |
| `dir-copy-unfiltered` | purity | evaluator | **warn** | a directory outside the store copied whole by a string coercion: no filter, and not a tree the flake machinery already turned into a tracked file set |
| `impure-path-in-drv` | purity | evaluator | **error** | a derivation attribute (env, `args`, structured attrs) holds the text of a path outside the store and outside any project root (`toString ./x`), so the .drv depends on the checkout location |
| `absolute-path-in-drv` | purity | evaluator | **error** | the same, for a path under a project root: an absolute path a flake evaluation would have copied |
| `tree-meta-in-drv` | purity | evaluator | **warn** | a derivation attribute (env, `args`, structured attrs) holds metadata of a source tree as a string: `rev`, `shortRev`, `dirtyRev`, `dirtyShortRev`, `lastModifiedDate` or `narHash`, so each commit gives a new .drv and a rebuild. The tree that the value comes from gives the level (Scope): the root tree by default, a locked input only with `[lint.inputs.<node>]`. Not for the integers `lastModified` and `revCount` (Limits); `rev-count` reports a read of `revCount` |
| `rev-count` | purity | evaluator | **warn** | the evaluation reads `revCount` of a source tree that has a name (`self.revCount`, `inputs.<node>.revCount`). The attribute is deprecated: the count of all revisions is expensive to compute, a shallow clone has none, and each commit changes it. `self ? revCount` and `builtins.attrNames self` are not a read. The tree gives the level (Scope): the root tree by default, a locked input only with `[lint.inputs.<node>]`. The site is the selection that reads the attribute; a read that is not a selection has no site (Scope) |
| `unknown-allow` | meta | parser | **warn** | a word of a `# iets: allow` comment that is not a lint name, a family or `all`, or a `# iets:` directive other than `allow` (Silencing one site) |

`impure-path-in-drv` follows a path the way Nix follows store references.
When you turn a path outside the store into text (`toString ./x`,
`"/x" + ./y`), the string carries a hidden mark. It survives concatenation,
interpolation, `concatStringsSep`, `replaceStrings` and the coercion of an
attribute set, `unsafeDiscardStringContext` removes it, and it never reaches
a `.drv` file. The lint looks for the mark in each attribute of a
derivation. The project root of a path is the one that
`docs/explanation/flakes-and-sources.md` defines.

`impure-path-in-drv` and `absolute-path-in-drv` are **errors** by default. A
pure evaluation in iets reads your checkout in place
(`docs/explanation/flakes-and-sources.md`), so the one thing that breaks when
the checkout moves is a derivation attribute that holds such a path. The
message gives the fix: interpolate the path, and it is copied to the store.
For the finding without the failure, set `impure-path-in-drv = "warn"` in
your project's `iets.toml`.

`dir-copy-unfiltered` is `warn` for the other side of that trade. Outside a
flake, nothing filters a directory copy by the files git tracks, so
`"${./.}"` takes your untracked and ignored files with it.

`tree-meta-in-drv` uses the same kind of mark. `fetchTree` and the root
tree of a flake put it on each metadata string of a tree, with the name of
the value (`self.rev`, `inputs.<node>.rev`), only when the lint is on for
that tree. The mark changes nothing that Nix sees: `.drv` files,
`getContext`, `hasContext`, `==` and `toJSON` give the same result with and
without it.

```nix
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }: {
    packages.x86_64-linux.default = nixpkgs.legacyPackages.x86_64-linux.hello.overrideAttrs {
      BUILD_REV = self.shortRev or self.dirtyShortRev;
    };
  };
}
```

```
$ iets eval --raw .#packages.x86_64-linux.default.drvPath
warning: [tree-meta-in-drv] derivation 'hello-2.12.3': attribute 'BUILD_REV' holds 'self.shortRev', a value that changes with every commit or edit, so this derivation and all that depend on it are built again each time; keep the value out of the build, or read it at run time
  at /repo/flake.nix:5:7
```

If the rebuild is small and you want the value in the build, do one of
these:

- Put `# iets: allow tree-meta-in-drv` on the line before the site.
- Remove the mark where you read the value:
  `builtins.unsafeDiscardStringContext self.rev`. Use this when the site
  is in an input, such as `system.configurationRevision` of NixOS, which
  nixpkgs writes into the derivation `nixos-version`. Nix gives the same
  string.

### `rev-count`: the deprecation of `revCount`

The attribute stays and its value is that of Nix, but a read of it gives a
warning, for these reasons:

- The count is expensive. `git rev-list --count` took 1 s on a nixpkgs
  checkout with a million commits, when last measured.
- The count needs the full history. A shallow clone has no `revCount`, so
  an expression that reads it fails there.
- The count changes with each commit, as `rev` does.

Use `lastModifiedDate` and `shortRev` for a version string, as nixpkgs
does for the version of NixOS:

```nix
version = "1.0.${builtins.substring 0 8 self.lastModifiedDate}.${self.shortRev or "dirty"}";
```

To silence one read, put the comment at the selection:

```nix
version = "1.${toString self.revCount}"; # iets: allow rev-count
```

A read that is not a selection has no site, so no comment covers it
("The tree that a value comes from"). To turn the lint off, set its level:

```toml
[lint]
rev-count = "off"
```

iets computes the count only when the evaluation reads it
(`docs/explanation/flakes-and-sources.md`), so the lint costs a flake that
doesn't read the attribute nothing.

## Defaults

A lint at `off` or `warn` never changes stdout. A lint that fires on
nixpkgs or on the language tests defaults to `off`, so only a lint that
fires nowhere may default to `warn`. `url-literal`, `absolute-path-literal`
and `short-path-literal` keep Nix's default, `off`. An `off` lint is one byte compare at the site.

## Caches and parallelism

- A cache hit prints the same warnings as the run that wrote the entry,
  before the result (`docs/explanation/caching.md`). The lint levels, the
  scope and the roots are part of the cache key, so another configuration
  is another entry. A lint at `error` fails the run, and a failed run is
  never cached.
- A file loaded from the compiled-file cache reports its compile-time
  warnings again, and its `# iets: allow` comments apply.
- A warning comes out before any `builtins.trace` or `builtins.warn` line
  and any error that follows it, and at the latest when the command ends.
- With more than one worker, iets can force list elements that a
  sequential `any`, `all` or `elem` would skip. So a run with several
  workers can report more sites than a run with one. When several values
  share one site, the message shows the value that got there first.
- The sources iets carries inside itself (`derivation.nix`,
  `fetchurl.nix`, `call-flake.nix`) are never linted.

## Limits

- The `currentTime` half of `impure-builtin` sees what the source names:
  `builtins.currentTime`, `currentTime`, `__currentTime`. `with builtins;
  currentTime` (a `with` variable) and `builtins."currentTime"` are not
  seen.
- `read-through-copy` only knows the copies this evaluation made through
  a coercion, `builtins.path` or `filterSource`; a flake root or input is
  a tracked tree and reading through it is the normal way.
- `dir-copy-unfiltered`, `large-source-copy` and `git-in-source` look at
  the copied tree after the filters of the tree it lies in.
- `impure-path-in-drv` sees the text of a path only through the marker:
  a string rebuilt from `substring` or `match` loses it, like store
  context. A structured-attrs value keeps the marker through
  `builtins.toJSON`.
- `tree-meta-in-drv` does not see the integers `lastModified` and
  `revCount`, because a string context can't go on an integer. `rev-count`
  reports the read of `revCount` itself, and `lastModified` has no finding.
- `tree-meta-in-drv` sees a value only through the mark, and the builtins
  that drop the store context drop the mark too: `match`, `split`,
  `hashString`, `fromJSON`, an attribute name. `builtins.toFile` writes the
  value into a store file and gives no finding.
- `unhashed-fetch` treats any of `sha256`, `hash`, `narHash`,
  `outputHash` or `rev` as pinning; it does not check the value.
- One warning per (lint, position): `fetchurl`'s `name` is one position for
  every fixed-output derivation it makes, so the message names the first
  derivation that reached it.
- `eval-jobs` with worker processes (`--max-memory-size`, the default)
  deduplicates per process.
- `# iets: allow` covers one line, so a derivation attribute lint needs
  the comment at its site (the `name` binding or the `derivationStrict` call).
