# Commands

Every `iets` command: what you type, the flags, what it prints on stdout and
stderr, and how it exits.

Other things have their own pages:

- the classic Nix names (`nix-instantiate`, `nix-build`, `nix-shell`,
  `nix-eval-jobs`, `nix`): `docs/reference/classic.md`;
- the operations for programs (`iets plumbing OP`):
  `docs/reference/plumbing.md`;
- error codes, exit codes and JSON log records: `docs/reference/errors.md`;
- where a build goes and what goes over the wire:
  `docs/reference/builders.md`;
- flake references and fetchers: `docs/reference/flakes.md`.

```
iets COMMAND [ARGS...]
```

| Command | What it does |
|:---|:---|
| `eval` | evaluate an expression, a file or a flake and print the value |
| `build` | evaluate to derivations, one or a set of them, and build them |
| `search` | print the packages that match every regular expression |
| `diff` | print what differs between two derivations |
| `prefetch` | print the hash of a URL or of a fixed-output derivation |
| `eval-jobs` | print a jobset's derivations as JSON lines |
| `print-dev-env` | print the bash of a flake's devShell |
| `lint` | print the lint registry and the levels a run would use |
| `parse` | parse Nix files and print or check the syntax tree |
| `cache` | sweep old cache entries, or print an entry's read set and its watchable paths |
| `completion` | print the tab-completion script of a shell |
| `plumbing` | the machine-first operations (`docs/reference/plumbing.md`) |
| `help`, `version` | the help of a command or a topic; the version |

`iets version` and `iets --version` print `iets VERSION` on stdout
and exit 0.

## Conventions

- **stdout is for the answer.** A value, an output path, a hash, a JSON
  object, a JSON line. Nothing else goes there.
- **stderr is for everything else.** Errors, warnings, build logs,
  progress, the `--timings` lines, the `IETS_STATS` lines, a classic
  fallback note. With `--log-format internal-json`, every stderr line is
  one `@nix ` JSON record (`docs/reference/errors.md`).
- **Progress.** On a terminal you get a status line that shows what is
  running: eval, fetches, substitutions, uploads, builds. On a pipe you get a
  `⏵` line after 5 s and then every 30 s. A transfer with no new bytes for
  10 s says `no data for N`. `--log-format bar` and `raw` let you choose.
  `IETS_PROGRESS=false` draws neither, for a log that must not depend on
  how fast a run is.
  Under a Nix name, a pipe gets Nix's `copying path …...` lines instead.
- **Exit codes** are in `docs/reference/errors.md`. In short: 0 success,
  1 any error, 2 a usage error of a native command, 100/101/102/104 a
  failed build under `iets build`. `iets diff` follows `diff(1)` instead.
- **Settings.** Every command takes `--iets-option NAME VALUE` for an
  `iets.toml` setting and `--option NAME VALUE` for a `nix.conf` setting
  (`docs/reference/settings.md`).

### The shared evaluation flags

`iets eval`, `iets build`, `iets prefetch` and every `iets plumbing` op
that evaluates take these flags. `iets diff`, `iets search`,
`iets eval-jobs` and `iets print-dev-env` take the subset that their own
section lists.

| Flag | Effect |
|:---|:---|
| `-E EXPR`, `--expr EXPR` | evaluate `EXPR` instead of a file |
| `FILE` | evaluate `FILE`; a directory resolves to its `default.nix` |
| `-f FILE`, `--file FILE` | the same as the positional `FILE`; native commands only |
| `FLAKEREF[#ATTR]` | evaluate a locked local flake |
| `-A PATH`, `--attr PATH` | select the attribute `PATH`; the last one wins |
| `--arg N EXPR`, `--argstr N STR` | call a top-level function with `N` |
| `-I P=PATH`, `--include P=PATH`, `-IP=PATH` | make `<P>` resolve to `PATH` |
| `--impure`, `--no-pure-eval` | turn pure evaluation off; it is on by default, and the `pure` key in the user `iets.toml` changes that (`docs/reference/settings.md`) |
| `--pure-eval` | turn pure evaluation on |
| `--option NAME VALUE` | set a nix.conf setting. The evaluation and fetch settings apply (`docs/reference/settings.md`). A store setting gives one `ignored-setting` warning: these commands send a daemon no settings |
| `--json-log-path PATH` | also write every log record to `PATH` |
| `--log-format F` | `internal-json`, `raw`, `raw-with-logs`, `bar`, `bar-with-logs` |
| `--show-trace` | put the trace into the JSON error records too |
| `--workers N` | evaluator threads; `N >= 1` |
| `--timings` | report each phase and the cache on stderr |
| `--no-tracing-cache` | do not read or write the tracing cache |
| `--no-ir-cache` | do not read or write the compiled-file cache |
| `--no-cache` | do not read or write either of them, as `IETS_CACHE=0` |
| `--cache-dir DIR` | keep the caches in `DIR/trace`, `DIR/ir` and `DIR/inputs` |
| `--strict` | evaluate the whole value |
| `--json` | print the value as JSON |
| `--raw` | print a string as it is |
| `--dump-drvs DIR` | write the ATerm of each derivation this run made to `DIR` |
| `--lint SELECTOR=LEVEL` | set a lint to `off`, `warn` or `error` |
| `--lint-root DIR` | report lint sites under `DIR` too |
| `--lint-scope root\|all` | report sites in the lint roots only, or everywhere |
| `--lint-absolute-path-literals MODE`, `--lint-short-path-literals MODE`, `--lint-url-literals MODE` | Nix's three lint settings |
| `--readonly-mode` | holds for `eval`, which writes to no store; one `ignored-flag` warning on a command that writes to the store |
| `--experimental-features F`, `--extra-experimental-features F` | every feature is always on but `parse-toml-timestamps`, which this turns on |

`-E` and a `FILE` together are the usage error
`-E and FILE cannot be combined`. Otherwise the last input wins. In
classic mode the arguments stay Nix's, so `-f` is native only:
`nix-eval-jobs -f` is an unknown flag.

`-A PATH` and a flake fragment call functions as they walk, the way Nix
does. So if a file's top level is a function, as nixpkgs' own
`default.nix` is, iets calls it with your `--arg`s before it takes the
attribute:

```
$ iets eval --impure $nixpkgs -A hello.name
"hello-2.12.3"
```

- An argument with a default can be left out.
- A required argument that you leave out is the error
  `cannot evaluate a function that has an argument without a value ('NAME')`.
- A function whose arguments end in `...` gets every `--arg`, named or not.

Without an attribute path, the top level is called only when you give an
`--arg`, as `nix eval -f FILE` does. Classic mode, `search` and every
native command walk the same way.

The lint flags are in `docs/reference/lints.md`. Pure evaluation is in
`docs/explanation/evaluation.md`.

## `eval`

```
iets eval [FLAGS] (-E EXPR | FILE | FLAKEREF#attr)
```

Evaluate and print the value. It takes the shared flags.

- Without `--json` or `--raw`, iets prints the value with its own printer.
  A derivation, a lambda and a thunk each look different.
- `--json` prints the value as JSON, the same shape as Nix. The whole value
  is forced first, `--strict` or not, as `nix eval --json` does.
- `--raw` turns the value into a string and prints the bytes with no
  quotes and no newline at the end.

Streams and exit codes:

- stdout: the value, with one newline at the end except under `--raw`.
- stderr: warnings, lint lines, `--timings`.
- Exit: 0, 1 on an evaluation error, 2 on a usage error.

## `build`

```
iets build [FLAGS] (-E EXPR | FILE | FLAKEREF[#ATTR])
iets build [FLAGS] [--source STOREPATH=LOCAL]... DRV
```

Evaluate to derivations, send the builder the parts of the closure it does
not have, build, and print the output paths.

**What gets built.** A derivation, a string with derivation context, or a
list of those, builds what it names. Any other attribute set is a tree, and
every derivation in it gets built. So `iets build .#checks` builds all the
checks. The walk is `nix-build`'s: names in order, a nested set only with
`recurseForDerivations = true`, and a list always.

In a set that has a member named after your system, iets goes into that
member first. That is how a flake lays out `checks`,
`packages` and `devShells`. To build for another system, name it in full:
`.#checks.aarch64-linux`. Evaluation is strict, so an attribute that fails
to evaluate stops the run before anything builds.

| Flag | Effect |
|:---|:---|
| `--builder URL` | `daemon`, `unix:///path`, `ssh-ng://[user@]host` or `grpc://host:port`; default: the `builder` setting |
| `--no-build` | upload the closure, build nothing |
| `--dry-run` | upload the closure, print the plan, build nothing |
| `--json` | print the outcomes as one JSON object instead of the paths |
| `-k`, `--keep-going` | with one root: build everything a failure does not block |
| `--out NAME` | build the output `NAME` only; repeatable |
| `--fetch-to DIR` | copy the outputs from the builder into `DIR` |
| `-o`, `--out-link PATH` | bring the outputs into the local store and link them at `PATH`, `PATH-1`, ... |
| `--source SP=LOCAL` | with a `DRV` argument: the local tree of the input source `SP` |

It also takes the shared evaluation flags.

- stdout: one output path per line, in root order. With `--json`, the
  outcomes as one JSON object instead. With `--dry-run --json`, the plan.
- stderr: `building '<drv>'…` for each build, the daemon's log lines, the
  `--dry-run` plan text, and after several roots one last line,
  `N built, M failed: ATTR, …`. Nothing the daemon sends reaches stdout.
- Exit: 0; 1 on an evaluation or daemon error; 100, 101, 102 or 104 on a
  failed build (`docs/reference/errors.md`); 2 on a usage error.

**Several roots.** Each root gets its own verdict. The build keeps going
past a failure, and the exit code combines the failures the way Nix does.
With one root, the first failure is the answer, as in Nix; `-k` asks for
the rest. What the builder already has is not built again, and what it can
substitute is substituted, with no flag needed.

The `--json` object is `{"outcomes":[{attr, drv, success, outputs, reason,
logTail}]}`, the same object that `iets plumbing build` prints
(`docs/reference/plumbing.md`). Under GitHub, Gitea or Forgejo Actions, the
log of a build that succeeds is folded into a `::group::`.

**The stdout contract.** When a run exits 0, it has printed a store path
for every root. The only exceptions are `--no-build` and `--dry-run`: they
build nothing, exit 0 and print nothing on stdout (`--dry-run --json`
prints the plan there instead). In every other case, a root without a path
is the error code `no-output` on stderr and a non-zero exit. That covers:

- an expression with no derivation in it;
- a builder that returns no result for a target;
- a build the builder calls a success but has no output path for;
- an `--out NAME` the derivation does not have.

A script that captures stdout gets a path or a failure, never an empty
success.

The `--dry-run` text is the same as Nix's. The `--dry-run --json` object is
`{willBuild, willSubstitute, unknown, downloadSize, narSize}`, and
`iets plumbing missing` prints the same object. A `grpc://` builder cannot
answer this question, so `--dry-run` is a usage error there.

**The paths outlive the run.** A `nix collect-garbage` between the run and
your use of the printed paths must not delete them, so every printed
output gets a link in iets's roots store, keyed on the working directory,
the installable and the output. The same command replaces its own roots
(`docs/explanation/caching.md`). `build-gc-root = false` keeps none. Two
cases never keep a root: `--fetch-to DIR`, which already has the
bytes outside the store, and a remote builder, whose outputs are not in the
local store.

**Outputs from a remote builder.** Without `-o`, the outputs stay on the
builder, and the printed paths are not valid locally. This suits CI, which
needs only the verdict. At a terminal, use `-o result`. iets then brings
the closure into the local store and links each output, as `nix build`
does. The local daemon first substitutes what its caches have. The rest
comes from the builder (`docs/reference/builders.md`). Each link is a GC
root, so no roots-store link is kept.

The `fix-hashes` setting rewrites the hash of a fixed-output build that
failed on it (`docs/reference/settings.md`). Where builds go and how the
closure travels is in `docs/how-to/build-remotely.md`.

## `search`

```
iets search [INSTALLABLE] [FLAGS] REGEX...
```

Print the packages whose attribute path, name or description match every
`REGEX`. The output is Nix 2.34.8's, byte for byte. `^` matches every
package.

```
iets search hello                     # <nixpkgs>, impure
iets search ./pkgs.nix '^'            # a file
iets search path:/flake ripgrep       # a locked flake
iets search --expr 'import <nixpkgs> {}' git gui
```

| Flag | Effect |
|:---|:---|
| `-e REGEX`, `--exclude REGEX` | hide a package this matches; repeatable |
| `--json` | one object per package, keyed by attribute path |
| `--pretty`, `--no-pretty` | indent the JSON, or do not; default: a terminal on stdout |
| `-f FILE`, `--file FILE` | search a file instead |
| `--expr EXPR` | search an expression instead |
| `--arg N EXPR`, `--argstr N STR`, `-I P=PATH` | as for `eval` |
| `--impure`, `--pure-eval`, `--option N V` | as for `eval` |
| `--timings`, `--workers N`, `--no-tracing-cache`, `--no-ir-cache`, `--no-cache`, `--cache-dir DIR` | as for `eval`; native name only |
| `--log-format F`, `--show-trace` | as for `eval` |

The regular expressions are POSIX extended and ignore case, as in Nix. With
no regex at all you get the usage error `Must provide at least one regex! To
match all packages, use 'nix search <installable> ^'.`

**The installable.** `iets search` takes the first argument as the
installable only when it names one: a flake reference, `<name>`, or a path
that exists. Anything else is a regex. The default installable is
`<nixpkgs>` from `-I` or `NIX_PATH`, evaluated impurely, as `--file` is.
In classic mode, the first argument of `nix search` is always the argument is always the
installable, as in Nix.

**What it searches.** iets walks the attributes in name order.

- A derivation (an attribute set whose `type` is `"derivation"`) is one
  result. It has its attribute path, its `name` split into pname and
  version, and `meta.description` with newlines turned into spaces. The
  name ends at the first `-` that is not followed by a letter. A missing
  `meta`, a missing `meta.description`, or a `meta` that is not an
  attribute set means no description.
- iets goes into the value the installable names, into the two levels
  under `packages` and `legacyPackages`, and into any attribute set with
  `recurseForDerivations = true`. `nix search` follows
  `recurseForDerivations` only under `legacyPackages`; `iets search`
  follows it anywhere. Nothing else is opened, so under `--file` or
  `--expr`, `nix search` searches the top level only.
- An evaluation error skips that attribute. `nix search` skips only under
  `legacyPackages` and stops everywhere else; `iets search` skips
  anywhere. A feature iets does not have is not an evaluation error: under
  classic mode, it falls back to the real `nix`.
- `iets search` calls the top level with your `--arg`s, so a package set
  that is a function, such as nixpkgs' own `default.nix`, works as it is.
  `nix search` does not do that.

A flake installable searches `packages.<system>` and
`legacyPackages.<system>`. A fragment (`FLAKE#gnome3`) searches
`packages.<system>.gnome3`, `legacyPackages.<system>.gnome3` and `gnome3`,
each one that exists.

**Output.** One block per result, with an empty line between blocks:

```
* legacyPackages.x86_64-linux.hello (2.12.3)
  Program that produces a familiar, friendly greeting
```

The matched text is highlighted. Nix writes the color codes to stdout
even when stdout is not a terminal, so iets does too: a pipe keeps the
colors. `--json` prints one object per result, keyed by attribute path,
with `pname`, `version` and `description`. The keys are sorted, indented
with `--pretty`, and on one line with `--no-pretty`.

iets builds the whole index first, because the cache needs it, and prints
after. Nix prints each result as it finds it.

- stdout: the blocks, or the JSON object.
- stderr: warnings; the `--timings` line.
- Exit: 0; 1 when nothing matches and there is no `--json` (error code
  `no-results`, hint `use fewer or shorter regexes`); with `--json`, no
  match is `{}` and exit 0; 2 on a usage error.

**The index cache.** The index is the list of `(attribute path, pname,
version, description)` that the walk found. The tracing cache keeps it,
keyed on the installable, the `--arg`s, the search path, `pure-eval` and
the build id. The regexes are not in the key, so any search over a package
set that did not change is a hit. When last measured on a pinned nixpkgs,
a hit took 0.08 s where an evaluation took 6.5 s, and the index was 4 MB.

`--timings` reports the hit or the miss. `IETS_CACHE=0` and
`IETS_CACHE_DIR` (`docs/reference/settings.md`) work under both names. The
classic `nix search` takes none of the cache flags.

Nix caches only a flake installable. iets caches `--expr` and `--file` too.
The `^ --json` output is the same on both sides, byte for byte.

## `diff`

```
iets diff [FLAGS] A B
iets diff [FLAGS] -A X -A Y (-E EXPR | FILE | FLAKEREF)
```

Print why two derivations are not the same. Use it instead of `nix-diff`.
It builds nothing and writes nothing to the store. A derivation that an
evaluation made is read from memory. Any other derivation is read from
`/nix/store`.

The first form takes two installables. Each is a `.drv` store path, a local
flake reference (`.#hello`) or a Nix file, and each must evaluate to exactly
one derivation. The second form evaluates one
expression, file or flake twice, and takes a different attribute each
time.

| Flag | Effect |
|:---|:---|
| `--depth N` | walk `N` levels of input derivations; default 2 |
| `--all` | walk every level |
| `--json` | print one JSON object instead of text |
| `--closure` | compare the output closures |
| `--builder URL` | the store `--closure` reads; default `daemon` |
| `-f FILE`, `--file FILE` | the same as the positional side |
| `-I P=PATH`, `--arg N E`, `--argstr N S`, `--option N V`, `--impure` | as for `eval`; they apply to both sides |
| `--log-format F`, `--show-trace` | as for `eval` |

**Exit codes.** 0 when the two are equal, 1 when they differ, and 2 for a
usage error, a failed evaluation, a derivation that cannot be read, or any
other failure. This follows `diff(1)`. Every error has its code from
`docs/reference/errors.md`, and its record under
`--log-format internal-json`.

**What it compares**, for each pair of derivations:

- the derivation name, the platform (`system`), the builder and its args;
- the outputs one side has and the other has not. For a fixed-output or
  content-addressed output, also its hash or method. An input-addressed
  output path is not compared: it changes whenever anything else changes,
  so it tells you nothing;
- the environment: the variables one side has and the other has not, and
  the value of each variable both sides set. A value of several lines,
  such as a build script, shows as a short line diff without the common
  start and end. The variables named after an output (they hold that
  output's path) and `name` repeat what the other lines already say, so
  they are left out. `name` still shows when either side sets it to
  something other than the derivation name;
- the structured attributes (`__json`), as one value;
- the input sources added and removed;
- the input derivations. The ones both sides share are skipped. The rest
  are paired by name, so `hello` on one side pairs with `hello` on the
  other, and each pair is compared the same way, one level deeper. What is
  left over is added or removed. When both sides have the same input but
  want different outputs of it, the pair gets a `wanted` field.

The walk stops after `--depth` levels, and at a pair it has already shown,
printing `(stopped: …; use --all)`.

**The text.** `-` is the first side and `+` the second. The lines under a
pair are its own differences. An indented `- …`/`+ …` pair is an input
derivation, with its differences under it.

```
$ iets diff -A a -A b ./demo.nix
- /nix/store/v6h4…-demo.drv
+ /nix/store/2c7p…-demo.drv
  input source removed: /nix/store/3kji…-one.txt
  input source added: /nix/store/n3da…-two.txt
  env GREETING:
    - hello
    + goodbye
  input drv changed: dep
    - /nix/store/qz5r…-dep.drv
    + /nix/store/8n1w…-dep.drv
      args:
        @@ line 2
        - "echo one > $out"
        + "echo two > $out"
```

Two equal derivations print `A and B are the same derivation`.

**The JSON.** `--json` prints one object. Every difference is one entry in
`changes`, so a program filters on `what`.

| Key | Value |
| --- | --- |
| `a`, `b` | the two derivation paths |
| `equal` | top level only: true when nothing differs |
| `changes` | every local difference, in order |
| `inputDrvs` | the input derivations that differ, each the same object again, one level deeper; absent when none differs |
| `cut` | why the walk stopped here: `depth` or `shown earlier`; absent when it did not stop |

An entry of `changes` is `{what, key, a, b}`:

| Key | Value |
| --- | --- |
| `what` | `name`, `platform`, `builder`, `args`, `structured attrs`, `output`, `env`, `input source`, `input drv` or `wanted` |
| `key` | the entry inside that part (the variable, the output name), or `""` |
| `a`, `b` | the value on each side; `null` is a side that has not got it |

`args` is one quoted argument per line, so a long command line diffs line
by line. The keys of the object are sorted.

```json
{"a":"/nix/store/v6h4…-demo.drv","b":"/nix/store/2c7p…-demo.drv","equal":false,
 "changes":[{"what":"env","key":"GREETING","a":"hello","b":"goodbye"}],
 "inputDrvs":[{"a":"/nix/store/qz5r…-dep.drv","b":"/nix/store/8n1w…-dep.drv",
   "changes":[{"what":"args","key":"","a":"\"-c\"\n\"echo one > $out\"\n",
                                    "b":"\"-c\"\n\"echo two > $out\"\n"}]}]}
```

**`--closure`** compares what the outputs of the two derivations refer to,
as `nvd` does. It takes the closure of every output on each side and groups
it by package name and version. The name ends at the first dash before a
digit, as Nix's `parseDrvName` splits it. The outputs must be in the store.
If the daemon does not have one, the error names the `iets build` command
that makes it.

```
$ iets diff --closure ./a.drv ./b.drv
removed libfoo 1.2
added   libbar 0.9
changed hello 2.12 -> 2.12.1
```

`--closure --json` prints `{a, b, equal, packages}`. Each entry of
`packages` is `{change, name, a, b}`: `change` is `added`, `removed` or
`changed`, and `a` and `b` are the versions of that name on each side.

## `prefetch`

```
iets prefetch [FLAGS] URL
iets prefetch [FLAGS] (-E EXPR | FILE -A ATTR | FLAKEREF#ATTR)
```

Print the hash of something you have not fetched yet, on one line. It
writes nothing to any store.

```
$ iets prefetch https://github.com/numtide/treefmt/archive/v2.0.0.tar.gz
sha256-/Sb+qZOLHhmr1D63ANxTxkhA4uTOpn0IcUO2UmQyRuo=

$ iets prefetch --unpack https://github.com/numtide/treefmt/archive/v2.0.0.tar.gz
sha256-Ckvpb2at7lg7AB0XNtIxeo8lWXX+04MVHHKoUs876dg=

$ iets prefetch -E '(import <nixpkgs> {}).hello.src'
sha256-DV9gFUOC/uELEUocNOeF2LH0kgc64tOm97FHaHs2aqA=

$ iets prefetch default.nix -A mypkg.cargoDeps   # a cargoHash or vendorHash
```

| Flag | Effect |
|:---|:---|
| `--unpack` | hash the NAR of the unpacked tarball, as `fetchzip` does |
| `--hash-type ALGO` | `md5`, `sha1`, `sha256` (the default) or `sha512` |
| `--json` | print one object instead of the bare hash |

The expression form also takes the shared evaluation flags.

**The forms.** An argument that starts with `https://`, `http://` or
`file://` is a URL. Anything else is an installable.

- `iets prefetch URL` downloads the URL and hashes it. Without `--unpack`,
  you get the flat hash of the bytes the URL serves, as
  `nix store prefetch-file` gives you. Nothing is decompressed. With
  `--unpack`, you get the NAR hash of the unpacked tarball, which is what
  `fetchTarball`, `fetchzip` and `fetchFromGitHub` want. The archive can be
  compressed with gzip, xz, zstd or bzip2. Zip files are not supported.
- `iets prefetch -E EXPR`, `iets prefetch FILE -A ATTR` and
  `iets prefetch FLAKEREF#ATTR` evaluate to one fixed-output derivation and
  build it. The hash in the expression does not matter, so `hash = ""`,
  `lib.fakeHash` and an out-of-date hash all work. There is no `-f FILE`: the
  file is a positional argument, as for `iets eval` and `iets build`.

**How the expression form works.** iets leaves your file alone. It replaces the derivation's fixed output hash with the all-zero
hash of the same algorithm, and builds that on the `builder` setting
(`docs/reference/settings.md`). The build then fails with:

```
hash mismatch in fixed-output derivation '…':
  specified: sha256-AAAA…
     got:    sha256-GzabkpW8…
```

The `got` hash is the answer. The fetcher runs as it would, so this is the
hash the real build will check. iets expects the mismatch and never reports
it as an error. An `outputHash` that the builder reads itself keeps the
value from your expression. `__structuredAttrs` works too.

**Output.** Without `--json`, the SRI hash, as one line on stdout. With
`--json`, one object:

| Form | Object |
| --- | --- |
| URL | `{"hash": "sha256-…", "url": "https://…"}` |
| expression | `{"hash": "sha256-…", "drvPath": "/nix/store/….drv"}` |

`drvPath` is the changed derivation, the one that was built. The one your
expression evaluates to has the hash your file names.

- Exit: 0 with a hash, 1 when the download or the build failed, 2 for a bad
  command line.

**How it differs from Nix.** `nix store prefetch-file` and
`nix-prefetch-url` add the file to the store and print its path. iets has no
store of its own, so it prints the hash and writes nothing, and the classic
names fall back to the real Nix (`docs/reference/classic.md`). The hash is
the one Nix gives. With `--unpack`, a tarball that holds a single file is
hashed by Nix as that file, and by iets as the directory that holds it. iets
builds the tree with git's rules, which is what `fetchTarball` hashes
(`docs/explanation/flakes-and-sources.md`).

`iets plumbing fetch` is the same operation, and always prints the object.

## `eval-jobs`

```
iets eval-jobs [FLAGS] (-E EXPR | FILE | --flake FLAKEREF[#attr])
```

Walk a jobset and print one JSON line per derivation, as nix-eval-jobs
2.35.2 does: the same walk and the same lines. Newer nix-eval-jobs
releases differ: 2.35.4 adds a `stats` key to every line, prints
`requiredSystemFeatures` without `--show-input-drvs`, and quotes an `attr`
element that is not an identifier (`names."7zip"`, where 2.35.2 writes
`names.7zip`).

| Flag | Status | Notes |
|---|---|---|
| `FILE`, `-E EXPR`, `--expr EXPR` | identical | The argument is a file unless you give `--flake`. The top level is called with the `--arg`s. A file evaluates purely here, where nix-eval-jobs follows nix.conf, because this is a native command (`docs/reference/settings.md`). As `nix-eval-jobs` in classic mode, nix-eval-jobs's default holds. |
| `--arg N E`, `--argstr N S`, `--arg-from-file N F`, `--arg-from-stdin N` | identical | |
| `-I`, `--include` | identical | |
| `--flake REF[#attr]` | identical for locked local flakes | `REF` is a directory (`.`, `./x`, `path:/x`) or a local checkout pinned by rev (`git+file:///p?rev=SHA`). The fragment is looked up in `outputs` as it is. Without a fragment, the root is the whole flake (`_type`, `inputs`, `outputs`, `sourceInfo`), for `--select`. Registries and unlocked inputs are not supported. Evaluation is pure unless `--impure`. |
| `--force-recurse`, `--meta`, `--show-input-drvs` | identical | |
| instantiation (the default) | identical result | Every job's `.drv` is in the store before its line is printed. See **Instantiation** below. |
| `--no-instantiate` | identical | `system` from the `system` attribute (`unknown` without one), no `requiredSystemFeatures`, no `inputDrvs`, no roots. Refused with `--show-input-drvs`, `--check-cache-status` or `--constituents`, with the same message. |
| `--eval-store URL` | supported | `daemon`, `unix:///path`, `ssh-ng://host`. Cache status and constituents read the `.drv`s back from `/nix/store`, so they need the local daemon, as nix-eval-jobs needs a local store. |
| `--gc-roots-dir DIR` | identical | A `DIR/<drv basename>` link, registered as a GC root, for every job and every rewritten aggregate. Skipped when the link exists. Nothing with `--no-instantiate`. iets creates `DIR`; nix-eval-jobs gives an error per job when it is missing. |
| `--check-cache-status` | identical | `isCached`, `cacheStatus`, `neededBuilds`, `neededSubstitutes`. See **Cache status** below. |
| `--constituents` | identical | Named, derivation and glob (`_hydraGlobConstituents`) constituents, dependency cycles, missing and failed constituents, rewritten `drvPath`/`outputs`, `namedConstituents` emptied, roots. Not across `--chunks`: a name in another chunk "does not exist". |
| `--apply EXPR` | identical | `extraValue`. A result that is not an attrset is the job's error. |
| `--select EXPR` | identical | Applied to the top level after it is called. A result that is not an attrset is fatal. |
| `--workers N` | different meaning | Worker processes (default: the cores, at most 4; nix-eval-jobs: 1), or fibers of one process with `--max-memory-size 0`. |
| `--max-memory-size MB` | identical default | Worker processes restart above `MB`, 4096 like nix-eval-jobs. `0` is iets's own: one process with no limit. |
| `--chunks M --chunk N`, `--chunk-depth D` | iets only | Evaluate part `N` (0 to M-1) of `M` parts of the tree, split at depth `D` (default 2). |
| `--impure` | identical | Turns off the pure evaluation that `--flake` implies. |
| `--show-trace` | accepted | The trace is always in the error text. With `--log-format internal-json` it also goes into the error records, as in Nix. |
| `--option k v` | as for `eval` | `allow-import-from-derivation` and `eval-cache` hold on their own: iets never builds during evaluation (`docs/reference/settings.md`) and has no Nix evaluation cache. `restrict-eval`, the other client settings iets has nothing for, and every store setting (substituters) give one `ignored-setting` warning. |
| `--quiet`, `--verbose`, `--debug` | `--quiet` only | `--quiet` drops the stderr summary. `--verbose` and `--debug` give one `ignored-flag` warning: iets has one log level. |
| `--log-format F` | accepted | `internal-json`: stderr is Nix's `@nix {...}` records, with a `code` and a `hint` on each error. `raw`, `raw-with-logs`, `bar`, `bar-with-logs`: plain lines. |
| `--max-jobs N` | holds | It limits the builds of an import from derivation, and iets never builds during evaluation. |
| `--provenance` | iets only | Adds a `pos` key to each job: the `file:line:col` of the derivation, mapped back to your local tree (`docs/explanation/provenance.md`). nix-eval-jobs 2.35 has no such key, so the flag is off by default. |
| `--reference-lock-file PATH` | supported, no merging | The root flake's lock is read from `PATH` instead of `DIR/flake.lock`. Nix locks the flake again, starting from `PATH`; iets takes `PATH` as the lock, so it must be complete for the inputs `flake.nix` declares. Nothing is written either way. |
| `--override-input`, `--override-flake` | error | iets evaluates a flake from a complete lock file only. |
| `--repair` | error | iets never repairs store paths. |

- stdout: one JSON line per job or error.
- stderr: the summary line, unless `--quiet`.
- Exit: 0 when only jobs fail (their errors are in the job lines); 1 on a
  fatal error, such as a stack overflow.

### The line schema

The keys are sorted by bytes and the JSON is compact, as in
nix-eval-jobs. A job:

```json
{"attr":"e.f","attrPath":["e","f"],"constituents":[],"drvPath":"/nix/store/...-f.drv",
 "globConstituents":false,"meta":null,"name":"f","namedConstituents":[],
 "outputs":{"out":"/nix/store/...-f"},"requiredSystemFeatures":["big-parallel","kvm"],
 "storeDir":"/nix/store","system":"x86_64-linux"}
```

| Key | Value |
|:---|:---|
| `attr` | the path elements joined with `.`; an element that contains a dot is wrapped in `"` |
| `attrPath` | the same elements as a list |
| `constituents`, `namedConstituents`, `globConstituents` | always present; filled only with `--constituents` on `_hydraAggregate` jobs |
| `drvPath` | the derivation |
| `outputs` | name to path, sorted; `null` for a content-addressed output |
| `name`, `storeDir` | the derivation name, and the store directory |
| `system`, `requiredSystemFeatures` | from the `.drv` file, present whenever the store is local and not `--no-instantiate`. The features are sorted, from `requiredSystemFeatures` in the env or the structured attributes, plus `ca-derivations` when an output path is not known in advance |
| `meta` | with `--meta`: every `meta` attribute that holds only ints, bools, strings, floats, and lists and sets of those, with no set that has an `outPath` (so `null`, functions and paths are dropped), as strict JSON. `null` when there is no `meta` or nothing passes. A `meta` that is not an attrset is an error |
| `inputDrvs` | with `--show-input-drvs`: derivation path to sorted output names |
| `isCached`, `cacheStatus`, `neededBuilds`, `neededSubstitutes` | with `--check-cache-status`; `cacheStatus` is `local`, `cached` or `notBuilt` |
| `extraValue` | with `--apply` |
| `pos` | with `--provenance`: `file:line:col` of the derivation. iets only |
| `stats` | from worker processes, as nix-eval-jobs 2.35.3 and later write it: `wallMs`, the time to evaluate the attribute, and `allocBytes`, the arena bytes that took. Nix counts its garbage collector's bytes instead. Work that attributes share counts toward the first one that needs it. Not present with `--max-memory-size 0` |

An error line:

```json
{"attr":"d","attrPath":["d"],"error":"error:\n       … while calling the 'throw' builtin\n ...","fatal":false}
```

The error text has no color codes. It is also printed on stderr. An error
line from a worker process has `stats` too.

### Instantiation

iets puts each job's `.drv` and its input closure into the store through
the daemon, in batches of 256 jobs. It prints a batch's lines only after the
batch is registered and its GC roots exist. So a printed `drvPath` is
always valid in the store, as with nix-eval-jobs. Registration runs while
the evaluation goes on. The stderr summary has an `instantiate:` line with
the batches, the paths queried and added, the bytes, the roots and the
time.

### Cache status

`--check-cache-status` works as in nix-eval-jobs 2.35:

The status is `local` when all outputs are valid, `cached` when the missing
ones can be substituted (they are in `neededSubstitutes`), and `notBuilt`
otherwise. For `notBuilt`, iets walks the input derivations and lists each
one with an unsubstitutable missing output in `neededBuilds`. A failed
substituter query counts as a miss.

### Constituents

`--constituents` works as in nix-eval-jobs. A job with
`_hydraAggregate = true` gets `constituents` (the `.drv`s in the string
context of its `constituents` attribute, sorted) and `namedConstituents`
(its strings). An aggregate with names is printed after the walk, resolved
against the other jobs:

- A missing name is `does not exist`. With `_hydraGlobConstituents`, a name
  is a shell glob over every other job, and
  `constituent glob pattern had no matches` when nothing matches.
- A constituent that failed adds its error text.
- Aggregates that name other aggregates are sorted on those names. A cycle
  prints the two jobs with `Dependency cycle: a <-> b`, and the rest with
  `Skipping aggregate because of a dependency cycle`.

An aggregate with no broken constituent gets a new `.drv`, with each
constituent as an input. iets writes it through the daemon and roots it, and
`drvPath` and `outputs` change to match. A broken constituent makes the line
`error: "name: why\n..."`.

### The memory limit

iets cannot free what a process has evaluated, so the only way to limit
memory is to start a new process. `--max-memory-size MB` is 4096 by
default, as in nix-eval-jobs.

The main process runs `--workers` worker processes and shares the
attribute paths among them. When a worker's peak memory goes above the
limit, it registers its open batch and exits, and a new worker takes its
place. A worker that dies is reported as nix-eval-jobs reports it.

Cache status and constituents run in the main process, from the lines.
`--max-memory-size 0` is one process with no limit, which cannot finish a
jobset larger than the machine.

iets differs from nix-eval-jobs in these ways:

- **Order** is completion order, as in nix-eval-jobs, and depth-first in
  name order with `--max-memory-size 0`. Sort by `attrPath` to compare.
- **Error text** is iets's own (`error: msg`, `at file:line:col`, `… trace`
  lines). The same attributes fail as in Nix; the text differs.
- **`system`** comes from the `.drv` in memory, or from `/nix/store` for an
  imported `.drv`. nix-eval-jobs reads it back from the store after it
  writes it.

## `print-dev-env`

```
iets print-dev-env [FLAGS] [FLAKEREF[#attr]]
```

Print the bash that `nix print-dev-env --option eval-cache false FLAKEREF`
prints, byte for byte.

| Flag | Effect |
|---|---|
| `--builder URL` | the Nix daemon: `daemon` (default), `unix:///path`, `ssh-ng://[user@]host` |
| `--gc-root PATH` | link `PATH` to the environment's store path and register it as an indirect GC root |
| `--json` | print what `nix print-dev-env --json` prints |
| `--entry-to FILE` | write the path of this run's cache entry to FILE (described after this table) |
| `--workers N`, `--timings`, `--cache-dir DIR` | as for `eval` |
| `--no-tracing-cache`, `--no-ir-cache`, `--no-cache` | as for `eval` |
| `--log-format F`, `--show-trace` | as for `eval` |

`FLAKEREF` defaults to `.`. Without `#attr`, the shell is the first of
`devShells.<system>.default`, `devShell.<system>`,
`packages.<system>.default` and `defaultPackage.<system>`. With `#x`, it
is the first of `devShells.<system>.x`, `packages.<system>.x`,
`legacyPackages.<system>.x` and `x`.

The daemon substitutes or builds the shell's inputs. `--gc-root` with
`ssh-ng://` is an error, because the root would name a remote path.
Without `--gc-root`, this command keeps no root: you choose the path here.
`nix develop` has no such flag, so it keeps a link in iets's roots store
instead (`docs/explanation/caching.md`).

- stdout: the bash. Variable assignments, `export`s, `declare -a` and
  `declare -A`, function definitions, the `PATH` and `XDG_DATA_DIRS`
  prepends, `NIX_BUILD_TOP` from `mktemp`, and `eval "${shellHook:-}"`. No
  `exit`, no `set -e`, no `exec`.
- stderr: errors, and the build log lines with an `iets: ` prefix on a
  failure, or as they come with `--timings`.
- Exit: 0, or non-zero on any error. Then stdout is empty, so an `eval` of
  the output leaves your environment alone.

`--entry-to FILE` writes the path of the tracing-cache entry this run
reads to `FILE`, whether the run hit it or wrote it. Pass that path to
`iets cache paths` to get the paths a watcher must watch
(`docs/how-to/use-with-direnv.md`).

Not supported: a shell whose builder is not `bash`, a content-addressed
shell, and the `--redirect`, `--profile`, `--command` and `--phase` flags
of `nix develop`.

## `lint`

```
iets lint list [FLAGS] [PATH]
```

Print the lint registry with the levels a run from `PATH` (default: the
current directory) would use, then the scope and its roots.

| Flag | Effect |
|---|---|
| `--lint NAME=LEVEL` | set a lint to `off`, `warn` or `error` |
| `--lint-root DIR` | add a lint root |
| `--lint-scope root\|all` | report sites in the lint roots only, or everywhere |

- stdout: one line per lint (name, level, family, hook, trigger), then one
  `scope: …` line.
- Exit: 0, 2 on a usage error. `list` is the only subcommand.

The registry and the scope rules are in `docs/reference/lints.md`.

## `parse`

```
iets parse [-q | --json] [--log-format F] FILE...
```

Parse each `FILE` and print its syntax tree. With `FILE` as `-`, iets reads
the list of files from stdin.

| Flag | Effect |
|---|---|
| `-q` | print only the parse errors and the totals |
| `--json` | print each tree as one line of JSON (`docs/reference/syntax-tree.md`) |
| `--log-format F` | `internal-json`: stderr is Nix's `@nix {...}` records |

- stdout: the tree of each file as S-expressions. With `--json`, one line of
  JSON for each file, with the kind and the byte span of each node. With
  `-q`, nothing; stderr gets one summary line,
  `N files, N bytes, N errors, T s`.
- Exit: 0, 1 when a file failed to parse, 2 on a usage error.

## `cache`

```
iets cache gc [--ttl DAYS] [--dry-run] [--cache-dir DIR]
iets cache dump ENTRY
iets cache paths ENTRY [--json]
```

- `gc` removes the cache entries that nobody read for `DAYS` days, and the
  GC roots iets owns that nobody used for as long.
- `dump` prints the read set of a tracing-cache entry.
- `paths` prints the part of that read set that a watcher must watch to
  know when the entry's answer is out of date.

| Flag | Effect |
|---|---|
| `--ttl DAYS` | the age limit in days |
| `-n`, `--dry-run` | count what `gc` would remove, remove nothing |
| `--cache-dir DIR` | sweep `DIR` instead of the default cache |
| `--json` | `paths`: one object per path, with its `kind` |

stdout for `gc`: one line per directory it sweeps (the tracing cache,
`ir`, `inputs` and `roots`), with the number of entries removed out of the
entries there, and the bytes. With `-n`, `(dry run)`. A root takes no space
of its own, so `roots` always reports 0.0 MiB. What it frees is what the
next `nix collect-garbage` can then delete.

`paths` prints one path per NUL byte, in the order the run read them. With
`--json`, it prints a JSON array of `{path, kind}`. `kind` is one of:

- `content`: a file the run read;
- `listing`: a directory it listed;
- `absent`: a path it found missing;
- `scm`: the file a source-control query depends on;
- `member`: a file under a tree the run copied.

Paths under `/nix/store` or under iets's own stores are left out. So is a
path whose kind was seen while it existed: the run only checked that it
existed, and an mtime cannot answer that. The list stops at 500 paths,
with a note on stderr. A command names its own entry with
`--entry-to FILE`.

- Exit: 0, 2 on a usage error, 1 when `ENTRY` is not a cache entry. `gc`,
  `dump` and `paths` are the only subcommands.

`docs/explanation/caching.md` explains the caches.

## `completion`

```
iets completion bash|zsh|fish|nushell|elvish|powershell
```

Prints the script that completes `iets` in that shell.

| Shell | Load it with |
|---|---|
| bash | `source <(iets completion bash)` |
| zsh | `source <(iets completion zsh)` after `compinit`, or save it as `_iets` in `$fpath` |
| fish | `iets completion fish \| source` |
| nushell | `iets completion nushell \| save -f FILE`, then `source FILE` in `config.nu` |
| elvish | `eval (iets completion elvish \| slurp)` |
| powershell | `iets completion powershell \| Out-String \| Invoke-Expression` |

- Exit: 0, 2 for a shell this does not know.

## `help` and the topics

```
iets help [COMMAND | TOPIC]
iets COMMAND --help
```

`iets help` alone prints the commands, then the topics. `iets help COMMAND`
prints that command's flags. `iets help TOPIC` prints a documentation page.

If a name is both a command and a topic, you get the command's help and a
note that the page exists. Ask for `reference/NAME` to get the page.

| Topic | Page |
|---|---|
| `commands` | this page |
| `plumbing` | `docs/reference/plumbing.md` |
| `classic` | `docs/reference/classic.md` |
| `builders` | `docs/reference/builders.md` |
| `flakes` | `docs/reference/flakes.md` |
| `settings` | `docs/reference/settings.md` |
| `events` | `docs/reference/events.md` |
| `profile` | `docs/reference/profile.md` |
| `errors` | `docs/reference/errors.md` |
| `lints` | `docs/reference/lints.md` |
| `syntax-tree` | `docs/reference/syntax-tree.md` |
| `builtins` | `docs/reference/builtins.md` |
| `divergences` | `docs/reference/divergences.md` |
| `fuzz` | `docs/reference/fuzz.md` |
| `library` | `docs/reference/library.md` |

The binary carries the reference pages only. `iets help` names the
tutorial, the how-to guides and the explanations, which are under `docs/`.

- stdout: the overview, the command's help, or the page.
- Exit: 0; 2 for a name that is neither a command nor a topic, with a
  suggestion when it is close to one.
  
