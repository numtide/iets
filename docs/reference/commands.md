# Commands

Every user-facing `iets` command: its synopsis, its flags, what it writes
to stdout and to stderr, and the codes it exits with.

The classic Nix names (`nix-instantiate`, `nix-build`, `nix-shell`,
`nix-eval-jobs`, `nix`) are in
`docs/reference/classic.md`. The machine-first operations
(`iets plumbing OP`) are in `docs/reference/plumbing.md`. The error codes,
the exit codes and the JSON log records are in `docs/reference/errors.md`.
Where a build goes and what is sent over the wire:
`docs/reference/builders.md`. Flake references and the fetchers:
`docs/reference/flakes.md`.

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
| `plumbing` | the machine-first operations (`docs/reference/plumbing.md`) |
| `help`, `version` | the help of a command or a topic; the version |

`iets version` and `iets --version` print `iets VERSION (COMMIT)` on
stdout and exit 0.

## Conventions

- **stdout carries the answer.** A value, an output path, a hash, a JSON
  object, a JSON line. Nothing else goes there.
- **stderr carries everything else.** Errors, warnings, build logs,
  progress, the `--timings` lines, the `IETS_STATS` lines, a classic
  fallback note. With `--log-format internal-json` every stderr line is
  one `@nix ` JSON record (`docs/reference/errors.md`).
- **Progress.** A terminal gets a status line with what runs now: eval,
  fetches, substitutions, uploads, builds. A pipe gets a `⏵` line after
  5 s and every 30 s; a transfer without new bytes for 10 s says `no data
  for N`. `--log-format bar` and `raw` choose. Under a Nix name a pipe
  gets Nix's `copying path …...` lines instead.
- **Exit codes** are in `docs/reference/errors.md`. In short: 0 success,
  1 any error, 2 a usage error of a native command, 100/101/102/104 a
  failed build under `iets build`. `iets diff` follows `diff(1)`
  instead.
- **Settings.** Every command takes `--iets-option NAME VALUE` for an
  `iets.toml` setting and `--option NAME VALUE` for a `nix.conf` setting
  (`docs/reference/settings.md`).

### The shared evaluation flags

`iets eval`, `iets build`, `iets prefetch` and every `iets plumbing`
op that evaluates share one parser (`src/cli/mod.rs:parse_eval_args`).
`iets diff`, `iets search`, `iets eval-jobs` and
`iets print-dev-env` take the subset each one lists below.

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
| `--strict` | evaluate the whole value, not the top level only |
| `--json` | print the value as JSON |
| `--raw` | print a string as it is |
| `--dump-drvs DIR` | write the ATerm of each derivation this run made to `DIR` |
| `--lint SELECTOR=LEVEL` | set a lint to `off`, `warn` or `error` |
| `--lint-root DIR` | report lint sites under `DIR` too |
| `--lint-scope root\|all` | report sites in the lint roots only, or everywhere |
| `--lint-absolute-path-literals MODE`, `--lint-short-path-literals MODE`, `--lint-url-literals MODE` | Nix's three lint settings |
| `--readonly-mode` | holds for `eval`, which writes to no store; one `ignored-flag` warning on a command that writes to the store |
| `--experimental-features F`, `--extra-experimental-features F` | every feature is always on but `parse-toml-timestamps`, which this turns on |

`-E` and a `FILE` together is the usage error `-E and FILE cannot be
combined`. The last input wins otherwise. `-f`/`--file` resolves exactly
as the positional argument does, and it is a native flag only: under a
classic name the argv stays Nix's, where `nix-eval-jobs -f` is an unknown
flag.

`-A PATH` and a flake fragment walk the value with
`attr-path.cc:findAlongAttrPath`, which auto-calls at every step. A file
whose top level is a function — nixpkgs' own `default.nix` — is therefore
called with the `--arg`/`--argstr` set before an attribute is taken from
it, so `iets eval $nixpkgs -A hello.name` prints `"hello-2.12.3"`. A
formal with a default that is not supplied is fine; a required formal
that is not supplied is `cannot evaluate a function that has an argument
without a value ('NAME')`; formals that end in `...` are handed every
`--arg`, named or not. Without an attribute path the top level is
called only when there is an `--arg` to call it with, which is what `nix
eval -f FILE` does. The classic names, `search` and every native command
share the one walk.

The lint flags are in `docs/reference/lints.md`. Pure evaluation is in
`docs/explanation/evaluation.md`.

## `eval`

```
iets eval [FLAGS] (-E EXPR | FILE | FLAKEREF#attr)
```

Evaluate and print the value. Flags: the shared set above.

- Without `--json` or `--raw` the value is printed with iets's own
  printer, which prints a derivation, a lambda and a thunk unambiguously.
- `--json` prints `printValueAsJSON`'s shape; `--strict` forces the whole
  value first.
- `--raw` coerces the value to a string and prints the bytes with no
  quotes and no trailing newline.

stdout: the value, one trailing newline except under `--raw`.
stderr: warnings, lint lines, `--timings`.
Exit: 0, 1 on an evaluation error, 2 on a usage error.

## `build`

```
iets build [FLAGS] (-E EXPR | FILE | FLAKEREF[#ATTR])
iets build [FLAGS] [--source STOREPATH=LOCAL]... DRV
```

Evaluate to derivation roots, send the closure the builder lacks from
memory, build, and print the output paths.

**The roots.** A derivation, a string with derivation context, or a list
of those, is what it names. Any other attribute set is a tree, and every
derivation in it is a root: `iets build .#checks` builds the checks.
The walk is `nix-build`'s (get-drvs.cc): names in order, a nested set
only with `recurseForDerivations = true`, a list always. One rule comes
before it: a set with a member named after the current system is entered
there, because that is how a flake lays out `checks`, `packages` and
`devShells`. Name another system in full to build it:
`.#checks.aarch64-linux`. Evaluation is strict: an attribute that does
not evaluate stops the run before anything builds.

| Flag | Effect |
|:---|:---|
| `--builder URL` | `daemon`, `unix:///path`, `ssh-ng://[user@]host` or `grpc://host:port`; default: the `builder` setting |
| `--no-build` | upload the closure, build nothing |
| `--dry-run` | upload the closure, print the plan, build nothing |
| `--json` | print the outcomes as one JSON object instead of the paths |
| `-k`, `--keep-going` | with one root: build everything a failure does not block |
| `--out NAME` | build the output `NAME` only; repeatable |
| `--fetch-to DIR` | copy the outputs from the builder into `DIR` |
| `--source SP=LOCAL` | with a `DRV` argument: the local tree of the input source `SP` |

Plus the shared evaluation flags.

stdout: one output path per line, in root order; with `--json` the
outcomes, and with `--dry-run --json` the plan, as one JSON object
instead.
stderr: `building '<drv>'…` per build, the daemon's log lines, the
`--dry-run` plan text, and after several roots one closing line,
`N built, M failed: ATTR, …`. Nothing the daemon sends reaches stdout.
Exit: 0; 1 on an evaluation or daemon error; 100, 101, 102 or 104 on a
failed build (`docs/reference/errors.md`); 2 on a usage error.

**Several roots.** Each root is its own question, so each one gets a
verdict: the build keeps going past a failure, and the exit code is
Nix's combination of the failures. One root is one question, and its
first failure answers it, as in Nix; `-k` asks for the rest. What the
builder already has is not built again, and what it can substitute is
substituted: that is the store's own rule, and it needs no flag. The
`--json` object is `{"outcomes":[{attr, drv, success, outputs, reason,
logTail}]}`, the object `iets plumbing build` prints
(`docs/reference/plumbing.md`). Under GitHub, Gitea or Forgejo Actions
the log of a build that succeeds is folded into a `::group::`.

**The stdout contract.** A run that exits 0 has printed a store path for
every root. `--no-build` and `--dry-run` build nothing, and are the only
exceptions: both exit 0 with nothing on stdout, and `--dry-run --json`
puts the plan there instead. Everything else that leaves a root without
a path is the error code `no-output` on stderr and a non-zero exit: an
expression with no derivation in it, a builder that returns no build
result for a target, a build the builder calls a success but has no
output path for, and an `--out NAME` the derivation does not have. A
caller that captures stdout, as a deploy script does, gets a path or a
failure, never silence and success.

The `--dry-run` text is Nix's `printMissing`. The `--dry-run --json`
object is `{willBuild, willSubstitute, unknown, downloadSize, narSize}`;
`iets plumbing missing` prints the same object. A `grpc://` builder has
no `QueryMissing`, so `--dry-run` is a usage error there.

**The paths outlive the run.** A caller reads those paths and uses them
afterwards, so a `nix collect-garbage` in between must not take them:
every printed output gets a link in iets's roots store, keyed on the
working directory, the installable and the output, so the same command
replaces its own roots in place (`docs/explanation/caching.md`).
`build-gc-root = false` keeps none. Two cases keep none anyway:
`--fetch-to DIR` already has the bytes outside the store, and a remote
builder's outputs are not in the local store, which is the only store
that takes a root.

`fix-hashes` rewrites the hash of a fixed-output build that failed on it
(`docs/reference/settings.md`). Where the builds go and how the closure
travels: `docs/how-to/build-remotely.md`.

## `search`

```
iets search [INSTALLABLE] [FLAGS] REGEX...
```

Print the packages of an installable whose attribute path, name or
description match every `REGEX`. The output is Nix 2.34.8's, byte for
byte. `^` matches every package.

| Flag | Effect |
|:---|:---|
| `-e REGEX`, `--exclude REGEX` | hide a package this matches; repeatable |
| `--json` | one object per package, keyed by attribute path |
| `--pretty`, `--no-pretty` | indent the JSON, or do not; default: a terminal on stdout |
| `-f FILE`, `--file FILE` | search a file instead |
| `--expr EXPR` | search an expression instead |
| `--arg N EXPR`, `--argstr N STR`, `-I P=PATH` | as above |
| `--impure`, `--pure-eval`, `--option N V` | as above |
| `--timings` | report the index cache on stderr; native name only |
| `--workers N` | evaluator threads; native name only |
| `--no-tracing-cache` | do not use the index, which the tracing cache holds; native name only |
| `--no-ir-cache` | do not use the compiled-file cache; native name only |
| `--no-cache` | do not use either of them; native name only |
| `--cache-dir DIR` | keep the stores in `DIR`; native name only |
| `--log-format F`, `--show-trace` | as above |

The regular expressions are POSIX extended and case insensitive, as
`std::regex::extended | std::regex::icase` in `search.cc`. No regex at
all is the usage error `Must provide at least one regex! To match all
packages, use 'nix search <installable> ^'.`

**The installable.** `iets search` takes the first argument as the
installable only when it names one: a flake reference, `<name>`, or a
path that exists. Everything else is a regex. The default installable is
`<nixpkgs>` from `-I` or `NIX_PATH`, evaluated impurely as `--file` is.
Under the classic name `nix search` the installable is Nix's: the first
argument always.

```
iets search hello                     # <nixpkgs>, impure
iets search ./pkgs.nix '^'            # a file
iets search path:/flake ripgrep       # a locked flake
iets search --expr 'import <nixpkgs> {}' git gui
```

**The walk** follows `src/nix/search.cc`, attribute by attribute, in name
order.

- A derivation (an attribute set whose `type` is `"derivation"`) is one
  result: its attribute path, `name` split into pname and version
  (`names.cc:DrvName`: the name runs up to the first `-` that is not
  followed by a letter), and `meta.description` with its newlines turned
  into spaces. A missing `meta`, a missing `meta.description` or a `meta`
  that is not an attribute set means no description.
- The walk descends into the value the installable named, into the two
  levels under `packages` and `legacyPackages`, and into an attribute set
  with `recurseForDerivations = true`. `nix search` honours
  `recurseForDerivations` under `legacyPackages` only; `iets search`
  honours it anywhere. Nothing else is opened: under a `--file` or
  `--expr` installable `nix search` searches the top level only.
- An evaluation error skips that attribute, as `catch (EvalError &)` does
  in `search.cc`. `nix search` skips it under `legacyPackages` only and
  stops everywhere else; `iets search` skips it anywhere. A feature
  iets does not have is not an evaluation error: under the classic name
  it falls back to the real `nix`.
- `iets search` auto-calls the root value with the `--arg`s, so a
  package set that is a function — nixpkgs' own `default.nix` — is
  searched as it stands. `nix search` does not.

Over the pinned nixpkgs the two walks find the same 112,215 packages,
with the same pnames, versions and descriptions.

A flake installable searches `packages.<system>` and
`legacyPackages.<system>`; a fragment (`FLAKE#gnome3`) searches
`packages.<system>.gnome3`, `legacyPackages.<system>.gnome3` and
`gnome3`, each of them that exists.

**Output.** Text, one block per result, separated by empty lines:

```
* legacyPackages.x86_64-linux.hello (2.12.3)
  Program that produces a familiar, friendly greeting
```

The matched substrings are highlighted. Nix writes the escape sequences
to stdout whether or not stdout is a terminal (`logger->cout` does not
filter them, only `log()` does), so iets does the same: piping the
output keeps the colours. `--json` prints one object per result, keyed by
attribute path, with `pname`, `version` and `description`, as
`nlohmann::json` dumps it: keys sorted, indented with `--pretty`, on one
line with `--no-pretty`.

Nix prints each result as it finds it and an `evaluating '<attr>'` line
on stderr while it walks. iets builds the whole index first — the cache
needs it — and prints after, without the progress lines.

stdout: the blocks, or the JSON object.
stderr: warnings; the `--timings` line.
Exit: 0; 1 with no result and no `--json` (error code `no-results`, hint
`use fewer or shorter regexes`); with `--json` no result is `{}` and
exit 0; 2 on a usage error.

**The index cache.** The index is the list of `(attribute path, pname,
version, description)` the walk produced. It is the `result` of a
tracing-cache entry, keyed on the installable, the `--arg`s, the search
path, `pure-eval` and the build id — not on the regexes, so any search
over an unchanged package set is a hit. A hit validates the read set and
answers from the index: for the pinned nixpkgs that is 56,170
observations in 0.08 s against 6.5 s of evaluation, and an index of 4 MB.
`iets search` takes the same cache and worker flags as `iets eval`:
`--no-tracing-cache` evaluates again, `--no-ir-cache` compiles every import
from source, `--no-cache` does both, `--cache-dir DIR` moves the stores,
`--workers N` sets the evaluator threads, and `--timings` reports the hit
or the miss.
`IETS_CACHE=0` and `IETS_CACHE_DIR` (`docs/reference/settings.md`) are
the same switches as settings. The classic `nix search` takes none of
these flags — its command line is Nix's — and the environment variables
work under both names.

**Measured.** The pinned nixpkgs
(`/nix/store/ah3pxz4xa52hd6vhbx17vcsci0b8bmbr-source`, 30,000 packages)
as `{ legacyPackages.${builtins.currentSystem} = import <pin> {}; }`, on
a 16-core EPYC shared with other work, release build, 2026-09-23. Wall
time, median of three:

| run | `^hello$` | all packages (`^`) |
|---|---|---|
| iets, no index yet | 6.5 s | 6.8 s |
| iets, index cached | 0.15 s | 0.47 s |
| real `nix search`, the same installable | 21 s | 21 s |
| real `nix search`, the same tree as a flake, its cache warm | 1.8 s | 2.6 s |
| real `nix search`, the same tree as a flake, its cache cold | 37 s | — |

Real nix keeps its own eval cache for a flake installable only; with
`--expr` or `--file` it evaluates every time. iets caches both alike.
The whole `^ --json` output, 18.8 MB, is byte for byte the same on both
sides.

Code: `src/cli/search.rs`.

## `diff`

```
iets diff [FLAGS] A B
iets diff [FLAGS] -A X -A Y (-E EXPR | FILE | FLAKEREF)
```

Print why two derivations are not the same, in place of `nix-diff`.
Nothing is built and nothing is written to the store. A derivation that
an evaluation produced is read from that evaluation's memory; every other
derivation is read from `/nix/store`.

The first form takes two installables. Each one is a `.drv` store path, a
flake reference (`.#hello`, `nixpkgs#hello`) or a Nix file, and each must
evaluate to exactly one derivation. The second form evaluates one
expression, file or flake twice and selects a different attribute each
time.

| Flag | Effect |
|:---|:---|
| `--depth N` | walk `N` levels of input derivations; default 2 |
| `--all` | walk every level |
| `--json` | print one JSON object instead of text |
| `--closure` | compare the output closures, not the derivations |
| `--builder URL` | the store `--closure` reads; default `daemon` |
| `-f FILE`, `--file FILE` | the same as the positional side |
| `-I P=PATH`, `--arg N E`, `--argstr N S`, `--option N V`, `--impure` | as for `eval`; they apply to both sides |
| `--log-format F`, `--show-trace` | as for `eval` |

**Exit codes.** 0 the two are equal, 1 they differ, 2 a usage error, a
failed evaluation, a derivation that cannot be read, or any other
failure. This follows `diff(1)`. Every error still goes through the
reporter of `docs/reference/errors.md`, with its code and, under
`--log-format internal-json`, its record.

**What it compares**, at each pair of derivations:

- the derivation name, the platform (`system`), the builder and its args;
- the output names one side has and the other has not, and, for a
  fixed-output or content-addressed output, its hash or method. An
  input-addressed output path is not compared: it changes whenever
  anything else does, so it says nothing;
- the environment: the variables one side has and the other has not, and
  for a variable both sides set, its value. A value of several lines (a
  build script) is shown as a compact line diff, with the common head and
  tail left out. The variables named after an output hold that output's
  path, and `name` holds the derivation's own name; both repeat something
  the lines above already said, so they are left out. `name` still shows
  when either side sets it to something other than the derivation name;
- the structured attributes (`__json`), as one value;
- the input sources added and removed;
- the input derivations. The ones both sides share are skipped. The rest
  are paired by derivation name, so `hello` on one side pairs with
  `hello` on the other, and the pair is compared the same way, one level
  deeper. What is left over is added or removed. When the same input path
  is on both sides but the wanted outputs differ, the pair carries a
  `wanted` field.

The walk stops at `--depth` levels and at a pair it has already shown.
Each stop prints the input's name and `(stopped: …; use --all)`.

**The text.** `-` is the first side, `+` the second. The lines under a
pair are its own differences; an indented `- …`/`+ …` pair is an input
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

**The JSON.** `--json` prints one object. Every difference is one entry
of `changes`, whatever part it is in, so a program filters on `what` and
never has to know a shape per part.

| Key | Value |
| --- | --- |
| `a`, `b` | the two derivation paths |
| `equal` | top level only: true when nothing differs |
| `changes` | every local difference, in order |
| `inputDrvs` | the input derivations that differ, each the same object again, one level deeper; absent when none differs |
| `cut` | why the walk stopped here: `depth` or `shown above`; absent when it did not stop |

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

**`--closure`** compares what the two derivations' outputs refer to, as
`nvd` does: the closure of every output of each side, grouped by package
name and version. The name ends at the first dash before a digit, as
Nix's `parseDrvName` splits it. The outputs must be in the store: the
daemon answers with `QueryPathInfo`, and a path it has not got is an
error that names the `iets build` which makes it.

```
$ iets diff --closure ./a.drv ./b.drv
removed libfoo 1.2
added   libbar 0.9
changed hello 2.12 -> 2.12.1
```

`--closure --json` prints `{a, b, equal, packages}`. Each entry of
`packages` is `{change, name, a, b}`, with `change` one of `added`,
`removed` or `changed`, and `a` and `b` the versions of that name on each
side.

Code: `src/cli/diff.rs`, `src/store/drvdiff.rs`.

## `prefetch`

```
iets prefetch [FLAGS] URL
iets prefetch [FLAGS] (-E EXPR | FILE -A ATTR | FLAKEREF#ATTR)
```

Print the hash of something that is not fetched yet, in one line, and
write nothing to any store.

| Flag | Effect |
|:---|:---|
| `--unpack` | hash the NAR of the unpacked tarball, as `fetchzip` does |
| `--hash-type ALGO` | `md5`, `sha1`, `sha256` (the default) or `sha512` |
| `--json` | print one object instead of the bare hash |

Plus the shared evaluation flags, for the expression form.

**The two forms.** A positional that starts with `https://`, `http://` or
`file://` is a URL; anything else is an installable.

- `iets prefetch URL` downloads the URL and hashes it. Without
  `--unpack` this is the flat hash of the bytes the URL serves, as `nix
  store prefetch-file` hashes them: nothing is decompressed. With
  `--unpack` it is the NAR hash of the unpacked tarball, which is what
  `fetchTarball`, `fetchzip` and `fetchFromGitHub` want. The archive may
  be compressed with gzip, xz, zstd or bzip2. Zip files are not
  supported.
- `iets prefetch -E EXPR`, `iets prefetch FILE -A ATTR` and
  `iets prefetch FLAKEREF#ATTR` evaluate to one fixed-output derivation
  and build it. The hash the expression names is ignored, so `hash = ""`,
  `lib.fakeHash` and last month's hash all work. There is no `-f FILE`:
  the file is a positional, as it is for `iets eval` and `iets build`.

```
$ iets prefetch https://github.com/numtide/treefmt/archive/v2.0.0.tar.gz
sha256-/Sb+qZOLHhmr1D63ANxTxkhA4uTOpn0IcUO2UmQyRuo=

$ iets prefetch --unpack https://github.com/numtide/treefmt/archive/v2.0.0.tar.gz
sha256-Ckvpb2at7lg7AB0XNtIxeo8lWXX+04MVHHKoUs876dg=

$ iets prefetch -E '(import <nixpkgs> {}).hello.src'
sha256-DV9gFUOC/uELEUocNOeF2LH0kgc64tOm97FHaHs2aqA=

$ iets prefetch default.nix -A mypkg.cargoDeps   # a cargoHash or vendorHash
```

**How the expression form works.** iets rewrites the derivation, not the
file that defines it. The evaluated `.drv` gets its fixed output hash
replaced by the all-zero hash of the same algorithm, its output path
recomputed from that hash, and its own path recomputed from the ATerm.
Nothing else moves: the `outputHash` the builder itself may read keeps
the value the expression gave it, since the daemon checks the output
against the derivation and not against that variable. The rewritten
derivation is built on the `builder` setting
(`docs/reference/settings.md`) like any other, the daemon reports

```
hash mismatch in fixed-output derivation '…':
  specified: sha256-AAAA…
     got:    sha256-GzabkpW8…
```

and the `got` hash is the answer. The rest of the closure is untouched,
the fetcher runs exactly as it would have, and the result is the hash the
real build will check. The mismatch is expected, so it is never reported
as an error, on stderr or in the `internal-json` stream.

A derivation with input derivations is built as a resolved copy of
itself, and the daemon then reports the mismatch in the build log instead
of in the result. iets reads it from there when the log names a
derivation of the same name. `__structuredAttrs` is fine: Nix fills the
output paths in when it builds, so nothing in the JSON goes stale.

**Output.** Without `--json`, the SRI hash, one line on stdout, and
nothing else on stdout. With `--json`, one object:

| Form | Object |
| --- | --- |
| URL | `{"hash": "sha256-…", "url": "https://…"}` |
| expression | `{"hash": "sha256-…", "drvPath": "/nix/store/….drv"}` |

`drvPath` is the rewritten derivation, the one that was built; the one
the expression evaluated to has the hash the file still names.

Exit: 0 with a hash, 1 when the download or the build failed, 2 for a bad
command line.

**What differs from Nix.** `nix store prefetch-file` and
`nix-prefetch-url` add the file to the store and print its path. iets is
a client with no store of its own: it prints hashes and writes nothing,
which is why it also has no `--name`, `--executable` or
`--expected-hash`, and why the classic names keep falling back to the
real nix (`docs/reference/classic.md`). The hash itself is the one Nix
prints: for the URL form it is the `hash` field of `nix store
prefetch-file --json`, in SRI, and for the expression form it is the
`got` hash of the build. Two more differences of `--unpack`:

- A tarball whose single member is a file: Nix hashes that file, iets
  hashes the directory that holds it. A single directory is dereferenced
  by both.
- The tree is built from the archive with git-tree rules — directories,
  symlinks, and files with one bit of mode — which is the tree
  `fetchTarball` hashes (`docs/explanation/flakes-and-sources.md`).

`iets plumbing fetch` is the same operation with the object always on
stdout. This command is that op plus the `--json` choice.

Code: `src/cli/prefetch.rs`.

## `eval-jobs`

```
iets eval-jobs [FLAGS] (-E EXPR | FILE | --flake FLAKEREF[#attr])
```

Walk a jobset and print one JSON line per derivation, as nix-eval-jobs
2.35 does: the same walk, the same lines.

| Flag | Status | Notes |
|---|---|---|
| `FILE`, `-E EXPR`, `--expr EXPR` | identical | The argument is a file unless `--flake`; the top level is auto-called with the `--arg`s, like `releaseExprTopLevelValue`. A file evaluates purely here, where nix-eval-jobs follows nix.conf: this is a native command (`docs/reference/settings.md`). `nix-eval-jobs`, the classic name, keeps nix-eval-jobs's default. |
| `--arg N E`, `--argstr N S`, `--arg-from-file N F`, `--arg-from-stdin N` | identical | |
| `-I`, `--include` | identical | |
| `--flake REF[#attr]` | identical for locked local flakes | `REF` is a directory (`.`, `./x`, `path:/x`); the fragment is looked up in `outputs` verbatim. Without a fragment the root is the `callFlake` result (`_type`, `inputs`, `outputs`, `sourceInfo`), to be `--select`ed. Registries and unlocked inputs are out of scope. Evaluation is pure unless `--impure`. |
| `--force-recurse`, `--meta`, `--show-input-drvs` | identical | |
| instantiation (the default) | identical result | nix-eval-jobs writes every `.drv` inside `derivationStrict`; iets registers the job drvs and their closures in batches on a thread beside the walk. |
| `--no-instantiate` | identical | `system` from the `system` attribute (`unknown` without one), no `requiredSystemFeatures`, no `inputDrvs`, no roots; refused together with `--show-input-drvs`, `--check-cache-status` or `--constituents`, with the same message. |
| `--eval-store URL` | supported | `daemon`, `unix:///path`, `ssh-ng://host`. Cache status and constituents read the `.drv`s back from `/nix/store`, so they need the local daemon, as nix-eval-jobs needs a `LocalFSStore`. |
| `--gc-roots-dir DIR` | identical | `DIR/<drv basename>` symlink plus `AddIndirectRoot` for every job and every rewritten aggregate, skipped when the link exists; nothing with `--no-instantiate`. iets creates `DIR`; nix-eval-jobs errors per job when it is missing. |
| `--check-cache-status` | identical | `isCached`, `cacheStatus`, `neededBuilds`, `neededSubstitutes`, from the cache-status-resolver.cc algorithm. |
| `--constituents` | identical | Named, derivation and glob (`_hydraGlobConstituents`) constituents, dependency cycles, missing and failed constituents, rewritten `drvPath`/`outputs`, `namedConstituents` erased, roots. Not across `--chunks`: a name in another chunk is "does not exist". |
| `--apply EXPR` | identical | `extraValue`; a non-attrset result is the job's error. |
| `--select EXPR` | identical | Applied to the auto-called root; a non-attrset result is fatal. |
| `--workers N` | different meaning | Worker processes (default 4; nix-eval-jobs: 1), or fibers of one process with `--max-memory-size 0`. |
| `--max-memory-size MB` | identical default | Worker processes restarted above `MB`, 4096 like nix-eval-jobs. `0` is iets's own: one process with no ceiling. |
| `--chunks M --chunk N`, `--chunk-depth D` | iets only | Evaluate part `N` (0 to M-1) of `M` parts of the tree, split at depth `D` (default 2). |
| `--impure` | identical | Turns off the pure evaluation `--flake` implies. |
| `--show-trace` | accepted | The trace is always in the error text; with `--log-format internal-json` it also goes into the error records, as in Nix. |
| `--option k v` | as for `eval` | `allow-import-from-derivation` and `eval-cache` hold by construction: iets never builds during evaluation and has no Nix evaluation cache. `restrict-eval` and the other client settings iets has nothing for, and every store setting (substituters), give one `ignored-setting` warning. |
| `--quiet`, `--verbose`, `--debug` | `--quiet` only | `--quiet` drops the stderr summary. `--verbose` and `--debug` give one `ignored-flag` warning: iets has one log level. |
| `--log-format F` | accepted | `internal-json`: stderr is Nix's `@nix {...}` records, with a `code` and a `hint` on each error. `raw`, `raw-with-logs`, `bar`, `bar-with-logs`: plain lines. |
| `--max-jobs N` | holds | It caps the builds of an import from derivation, and iets never builds during evaluation. |
| `--provenance` | iets only | Adds a `pos` key per job: `file:line:col` of the derivation, mapped back to the local tree (`docs/explanation/provenance.md`). nix-eval-jobs 2.35 has no such key, so the flag is off by default. |
| `--reference-lock-file PATH` | supported, no merging | The root flake's lock is read from `PATH` instead of `DIR/flake.lock`. Nix locks the flake again with `PATH` as its starting point; iets takes `PATH` as the lock, so it must be complete for the inputs `flake.nix` declares. Nothing is written either way. |
| `--override-input`, `--override-flake` | error | iets evaluates a flake from a complete lock file only. |
| `--repair` | error | iets never repairs store paths. |

stdout: one JSON line per job or error.
stderr: the summary line, unless `--quiet`.
Exit: 0 when only jobs fail — their errors are in the job lines; 1 on a
fatal error such as a stack overflow.

### The line schema (drv.cc, response.cc)

nlohmann `json` objects are `std::map`s, so the keys are byte-sorted and
`dump()` is compact. A job:

```json
{"attr":"e.f","attrPath":["e","f"],"constituents":[],"drvPath":"/nix/store/...-f.drv",
 "globConstituents":false,"meta":null,"name":"f","namedConstituents":[],
 "outputs":{"out":"/nix/store/...-f"},"requiredSystemFeatures":["big-parallel","kvm"],
 "storeDir":"/nix/store","system":"x86_64-linux"}
```

| Key | Value |
|:---|:---|
| `attr` | the path elements joined with `.`, an element containing a dot wrapped in `"` (`attrPathJoin`) |
| `attrPath` | the same elements as a list |
| `constituents`, `namedConstituents`, `globConstituents` | always present; filled only with `--constituents` on `_hydraAggregate` jobs |
| `drvPath` | the derivation |
| `outputs` | name to path, sorted; `null` for a content-addressed output |
| `name`, `storeDir` | the derivation name, and the store directory |
| `system`, `requiredSystemFeatures` | from the `.drv` file, present whenever the store is local and not `--no-instantiate`. The features are sorted, from `requiredSystemFeatures` in the env or the structured attributes, plus `ca-derivations` when an output path is not known statically |
| `meta` | with `--meta`: every `meta` attribute that passes `checkMeta` (int, bool, string, float, lists and sets of those, no set with an `outPath`; so `null`, functions and paths are dropped), as `printValueAsJSON` strict. `null` when there is no `meta` or nothing passes. A non-attrset `meta` is an error |
| `inputDrvs` | with `--show-input-drvs`: derivation path to sorted output names |
| `isCached`, `cacheStatus`, `neededBuilds`, `neededSubstitutes` | with `--check-cache-status`; `cacheStatus` is `local`, `cached` or `notBuilt` |
| `extraValue` | with `--apply` |
| `pos` | with `--provenance`: `file:line:col` of the derivation. iets only |

An error line:

```json
{"attr":"d","attrPath":["d"],"error":"error:\n       … while calling the 'throw' builtin\n ...","fatal":false}
```

The error text is `showErrorInfo` with the ANSI escapes stripped, and is
also printed to stderr.

### Instantiation

`src/jobs/instantiate.rs`. The walk hands every job or error line to a
thread that holds one daemon connection. Every 256 jobs, or at the end, it
computes the closure of the batch's `drvPath`s from the evaluator's
derivation table (ATerm and references; an imported `.drv` is read from
`/nix/store`) and the sources they reference (origins in `ev.known`: local
trees and `toFile` texts, hashed on the way), stops at paths already known
valid, asks `QueryValidPaths` once, sends what is missing in one
`AddMultipleToStore` (sources first, derivations in post-order, the same
`ValidPathInfo` and NAR framing as `iets build`), registers the GC roots,
and only then passes the batch's lines on. So a printed `drvPath` is
always valid in the store, as after nix-eval-jobs, and the daemon round
trips overlap the evaluation. The stderr summary gets an `instantiate:`
line with batches, paths queried and added, bytes, roots and thread time.

### Cache status

`src/jobs/cache.rs` mirrors cache-status-resolver.cc. Nix 2.35 no longer
calls `queryMissing`: it probes with `queryValidPaths` and
`querySubstitutablePathInfos` and walks the input derivations itself.

1. Every output of the job is probed for validity. All valid is `local`.
2. The missing ones all substitutable is `cached`, with them as
   `neededSubstitutes`.
3. Otherwise the input graph is walked from the job's `.drv`, read back
   from `/nix/store` and cached. A derivation whose wanted outputs are
   missing and not all substitutable is a needed build (post-order,
   `inputDrvs` in store-path order) and its inputs are visited; one whose
   missing outputs are substitutable contributes them to
   `neededSubstitutes` and is not descended.

Probes are batched: one `QueryValidPaths` and one
`QuerySubstitutablePathInfos` (worker op 30) per pass over the batch of
jobs that is still unresolved; a failing substituter query counts as a
miss. `neededSubstitutes` is sorted by name then path, like the C++. In
one process the resolver runs on the instantiation thread after each
batch; with `--max-memory-size` it runs on its own thread, fed by the
collectors.

### Constituents

`src/jobs/constituents.rs` mirrors constituents.cc. At job time, with the
flag, `_hydraAggregate = true` jobs yield `constituents` (the `.drv`s in
the string context of `constituents`, sorted) and `namedConstituents` (its
strings); an aggregate with names is withheld by the emitter. After the
walk every withheld aggregate is resolved against the job table:

- a missing name is `does not exist`, or, with `_hydraGlobConstituents`,
  an fnmatch pattern over every other job, and
  `constituent glob pattern had no matches` when nothing matches;
- a constituent that errored contributes its error text;
- aggregates are topologically sorted on the names that are aggregates
  themselves; a cycle prints the two jobs with `Dependency cycle: a <-> b`
  and the rest with `Skipping aggregate because of a dependency cycle`.

An aggregate with no broken constituent gets each constituent's `.drv` as
an `inputDrvs` entry with the child's first output, its input-addressed
outputs reset to deferred and recomputed
(`finalize_input_addressed_outputs`, hashes read from `/nix/store`), the
new `.drv` written through the daemon and rooted, and `drvPath` and
`outputs` replaced. `namedConstituents` is erased, and a broken one
becomes `error: "name: why\n..."` on the job line.

### The memory limit

`src/jobs/collector.rs`. iets has one arena and no garbage collector, so
a process cannot shed what it has evaluated; the only ceiling is a process
boundary. `--max-memory-size MB` is therefore 4096 by default, like
nix-eval-jobs (`eval-args.hh:18`).

The main process runs `--workers` collector threads, each with a worker
process (`iets eval-jobs --worker` and the same arguments, one fiber
thread, its own instantiation thread) and the shared `todo` set of
attribute paths starting at `[]`. A worker gets `do ["a","b"]`, finds the
value along the path — auto-calling at every step — and answers
`{"attrs":[...]}` for a set to recurse into, `ok` for a skipped node or a
job (the job line follows once its instantiation batch is registered, so
lines arrive at any time), or an error line. After every answer it
compares `VmHWM` with the limit and, above it, flushes its batch, says
`restart` and exits, and the collector spawns a new one. A worker that
dies is reported like nix-eval-jobs's `handleBrokenWorkerPipe`.

Cache status and constituents run in the main process from the lines, as
the `.drv`s are in the store by then. Output order is completion order.
The chunk rule still applies inside a worker, so `--chunks` and
`--max-memory-size` combine. `--max-memory-size 0` is one process with no
ceiling, which no jobset larger than the box can finish.

Three differences that stay:

- **Order** is completion order, like nix-eval-jobs, and lexicographic
  depth-first with `--max-memory-size 0`. Sort by `attrPath` to compare.
- **Error text** is iets's (`error: msg`, `at file:line:col`, `… trace`
  lines), not Nix's rendering. The set of attributes that error is the
  same; the text is not.
- **`system`** comes from the in-memory `.drv`, or from `/nix/store` for
  an imported `.drv`; nix-eval-jobs reads it back from the store after
  writing.

`scripts/evaljobs-flags.nu` runs nix-eval-jobs 2.35.2 and iets with
every flag combination above on the jobsets in `tests/evaljobs/` and
diffs the sorted output: 48 cases, all identical, exit codes included.
2.35.2 is the version the flake pins, and the suite needs that one: 2.35.4
adds a `stats` key to every line, prints `requiredSystemFeatures` without
`--show-input-drvs`, and quotes an `attr` element that is not an identifier
(`names."7zip"`, where 2.35.2 writes `names.7zip`). Bumping the pin means
following all three.

## `print-dev-env`

```
iets print-dev-env [FLAGS] [FLAKEREF[#attr]]
```

Print the bash that `nix print-dev-env --option eval-cache false
FLAKEREF` prints, byte for byte.

| Flag | Effect |
|---|---|
| `--builder URL` | the Nix daemon: `daemon` (default), `unix:///path`, `ssh-ng://[user@]host` |
| `--gc-root PATH` | link `PATH` to the environment's store path and register it as an indirect GC root |
| `--json` | print what `nix print-dev-env --json` prints |
| `--entry-to FILE` | write the path of this run's cache entry to FILE; see below |
| `--workers N`, `--timings`, `--cache-dir DIR` | as for `eval` |
| `--no-tracing-cache`, `--no-ir-cache`, `--no-cache` | as for `eval` |
| `--log-format F`, `--show-trace` | as for `eval` |

`FLAKEREF` defaults to `.`. Without `#attr` the shell is the first of
`devShells.<system>.default`, `devShell.<system>`,
`packages.<system>.default`, `defaultPackage.<system>`; with `#x` the
first of `devShells.<system>.x`, `packages.<system>.x`,
`legacyPackages.<system>.x`, `x` (develop.cc, installable-flake.cc).

The daemon substitutes or builds the shell's inputs. `--gc-root` with
`ssh-ng://` is an error: the root would name a remote path. Without
`--gc-root` this command keeps no root: the path is the caller's to choose
here, unlike in `nix develop`, which has no flag and takes a link in
iets's roots store instead (`docs/explanation/caching.md`).

stdout: the bash. Variable assignments, `export`s, `declare -a` and
`declare -A`, function definitions, the `PATH` and `XDG_DATA_DIRS`
prepend, `NIX_BUILD_TOP` from `mktemp`, and `eval "${shellHook:-}"`. No
`exit`, no `set -e`, no `exec`.
stderr: errors, and the build log lines with an `iets: ` prefix on a
failure, or streamed with `--timings`.
Exit: 0, non-zero on any error, and then stdout is empty, so `eval` of an
empty string leaves the environment alone.

`--entry-to FILE` writes the path of the tracing-cache entry this run reads
to `FILE`, whether the run hit or wrote it. Hand that to `iets cache paths`
for the paths a watcher has to watch, instead of deriving the key a second
time from a matching set of flags. `docs/how-to/use-with-direnv.md` has the
`.envrc`.

A `__structuredAttrs` shell gets its `.attrs.sh` and `.attrs.json`
written next to the GC root (`PATH-attrs/`), or under
`$TMPDIR/iets-dev-env-<pid>/` without one.

Not supported: a shell whose builder is not `bash`, a content-addressed
shell, and the `--redirect`, `--profile`, `--command` and `--phase` flags
of `nix develop`. Setting it up for direnv:
`docs/how-to/use-with-direnv.md`.

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

stdout: one line per lint — name, level, family, hook, trigger — then one
`scope: …` line.
Exit: 0, 2 on a usage error. `list` is the only subcommand.

The registry, the levels and the scope rules are in
`docs/reference/lints.md`.

## `parse`

```
iets parse [-q | --json] [--log-format F] FILE...
```

Parse each `FILE` and print its syntax tree. `FILE` `-` reads the list of
files from stdin.

| Flag | Effect |
|---|---|
| `-q` | print only the parse errors and the totals |
| `--json` | print each tree as one line of JSON (`docs/reference/syntax-tree.md`) |
| `--log-format F` | `internal-json`: stderr is Nix's `@nix {...}` records |

stdout: the tree of each file as S-expressions. With `--json`: one line
of JSON for each file, with the kind and the byte span of each node. With
`-q`: nothing; stderr gets one summary line, `N files, N bytes, N errors,
T s`.
Exit: 0, 1 when a file failed to parse, 2 on a usage error.

## `cache`

```
iets cache gc [--ttl DAYS] [--dry-run] [--cache-dir DIR]
iets cache dump ENTRY
iets cache paths ENTRY [--json]
```

`gc` removes the cache entries nobody read for `DAYS` days, and the GC roots
iets owns that nobody has used for as long. `dump` prints the read set of a
tracing-cache entry. `paths` prints the part of that read set a watcher has
to watch for the entry's answer to go stale.

| Flag | Effect |
|---|---|
| `--ttl DAYS` | the age limit in days |
| `-n`, `--dry-run` | count what `gc` would remove, remove nothing |
| `--cache-dir DIR` | sweep `DIR` instead of the default cache |
| `--json` | `paths`: one object per path, with its `kind` |

stdout: one line per swept directory — the tracing cache, `ir`, `inputs` and
`roots` — with the entries removed of the entries there and the bytes;
`(dry run)` with `-n`. A swept root frees no bytes of its own, so `roots`
always reports 0.0 MiB; what it frees is what the next `nix collect-garbage`
can then take. `dump` prints the read set.

`paths` prints one path per NUL byte, in the order the run read them, or a
JSON array of `{path, kind}` under `--json`. `kind` is `content` a file the
run read, `listing` a directory it listed, `absent` a path it found missing,
`scm` the file a source-control query depends on, or `member` a file under a
tree the run copied. A path under `/nix/store` or under iets's own stores is
left out, and so is a path whose kind was observed while it existed: that
observation was existence, which an mtime does not answer. The list stops at
500 paths, with a note on stderr. A command names its own entry with
`--entry-to FILE`.
Exit: 0, 2 on a usage error, 1 when `ENTRY` is not a cache entry. `gc`,
`dump` and `paths` are the only subcommands.

The caches themselves are in `docs/explanation/caching.md`.

## `help` and the topics

```
iets help [COMMAND | TOPIC]
iets COMMAND --help
```

`iets help` alone prints the overview: the commands, and the topics
below. `iets help COMMAND` prints that command's flags.
`iets help TOPIC` prints a documentation page, which the binary carries:
no `docs/` directory has to be installed or found.

A name that is both a command and a topic prints the command's help and
says the page is there. Address the page as `reference/NAME` to get it
instead.

| Topic | Page |
|---|---|
| `commands` | this page |
| `plumbing` | `docs/reference/plumbing.md` |
| `classic` | `docs/reference/classic.md` |
| `builders` | `docs/reference/builders.md` |
| `flakes` | `docs/reference/flakes.md` |
| `settings` | `docs/reference/settings.md` |
| `errors` | `docs/reference/errors.md` |
| `lints` | `docs/reference/lints.md` |
| `syntax-tree` | `docs/reference/syntax-tree.md` |
| `builtins` | `docs/reference/builtins.md` |
| `divergences` | `docs/reference/divergences.md` |
| `library` | `docs/reference/library.md` |

The binary embeds the reference quadrant only. The tutorial, the how-to
guides and the explanations are in the repository, under `docs/`;
`iets help` names each of them and the path it lives at.

stdout: the overview, the command's help, or the page.
Exit: 0; 2 for a name that is neither a command nor a topic, with a
suggestion when it is close to one.
