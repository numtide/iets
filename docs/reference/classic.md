# Classic mode

iets can stand in for Nix's own commands. Call it `nix-instantiate`,
`nix-build`, `nix-shell`, `nix-eval-jobs` or `nix`, and it handles what it
implements. For everything else it runs the real Nix, so a real Nix has to be
installed too. This page lists the flags each name handles and what makes it
fall back. "Fallback rules" says how iets finds the real Nix.

The classic package is Nix with these five commands handled by iets:

```
nix build .#classic
```

Without the classic package, use subcommands: `iets nix-instantiate ...`,
`iets nix-build ...`.

## Where the builds go

Classic mode has no `--builder` flag. They build on the `builder`
setting in your user `iets.toml`, else on nix.conf's `store`. `--store URL`
and `--option store URL` win over both. `--iets-option name value` sets any
iets setting for one run, and iets removes it before a fallback runs the
real Nix (`docs/reference/settings.md`).

Some things stay local whatever `builder` says:

- `nix-instantiate` registers the drvs and their sources on the local
  daemon, and `--add-root` links them there. It builds nothing, so the
  builder does not matter.
- `nix-build`, `nix build` and `nix fmt` build on the builder. When that is
  not the local daemon, iets copies the outputs' closure back to your local
  store before it makes `result`. So the link names a path your machine
  has, and the link is a GC root.
- `nix-shell`, `nix develop` and `nix print-dev-env` ignore `builder`. The
  shell runs on your machine, so the local daemon builds or substitutes its
  inputs. `--store` still wins.

## The NixOS module

`nix/module.nix` installs `packages.default`. On a host with direnv, it also
hooks iets into direnv ([Use iets with direnv](../how-to/use-with-direnv.md#nixos-module)).
Set `iets.classic.enable` (default `false`) to also install the classic mode commands:

```nix
{
  imports = [ iets.nixosModules.default ];
  iets.classic.enable = true;
}
```

This sets `nix.package` to the classic package, so `nixos-rebuild` uses the
classic mode too. Choose the Nix underneath with `iets.classic.nix` instead
of `nix.package`:

```nix
{ pkgs, ... }:
{
  iets.classic.nix = pkgs.nixVersions.latest;
}
```

## What each name handles

### `nix-instantiate`

Flags:

- `[FILE...]`: default `./default.nix`. A directory means its
  `default.nix`, and `<name>` goes through the search path.
- `-E`/`--expr`, `--arg`/`--argstr`, `-I`/`--include`.
- `-A`/`--attr`: repeatable, with `"quoted"` components and list indices.
- `--eval` with `--strict`, `--json`, `--raw`.
- `--add-root PATH` (`--indirect` is accepted), `--readonly-mode`,
  `--read-write-mode`, `--dry-run`.
- `--show-trace`, `--no-gc-warning`, `--option`, `--store`,
  `-j`/`--max-jobs`, `--cores`, `-k`, `-K`, `-Q`, `--quiet`, `-v`.

By default it evaluates, finds the derivations in the value, registers
their closure with the daemon, and prints one root per line. A root is the
drv path, plus `!<output>` when the derivation's `outputName` is not `out`.

With `--add-root`, it makes the symlink, registers it as a GC root, and
prints the link instead. The second and later roots get `-2`, `-3`, … at
the end. `--add-root ""` means no root, as in Nix: you get the drv path and
the GC warning. `--readonly-mode`, and `--eval` without `--read-write-mode`,
register nothing.

Falls back on: `--parse`, `--parse-only`, `--xml`, `--find-file`, `-`
(stdin), `--add-root` with a read-only store, a `-j`/`--cores` value that
is not the setting's, `-IP=PATH` (Nix reads that as several short flags and
refuses it), and any other flag.

### `nix-build`

Flags: `[FILE...]` (default `.`), `-E`, `-A`, `--arg`/`--argstr`, `-I`,
`-o`/`--out-link`, `--no-out-link`/`--no-link`, `-j`/`--max-jobs`,
`--cores`, `-k`, `-K`, `-Q`/`--no-build-output`, `--quiet`, `-v`,
`--show-trace`, `--option`, `--store`, and `--add-drv-link` and
`--indirect` (these two do nothing in Nix either). `-v` has no effect and
gives one warning (`docs/reference/divergences.md`, **Flags and settings
with no effect**).

It uploads the closure once and builds every root in one go. Then it makes
the links, as Nix does:

- `result` for the first drv's `out`;
- `result-<output>` for another output of the same drv;
- `result-<n>` for the n-th distinct drv, from 2 on.

Each link is a GC root. It prints the output paths in root order. stderr
shows `building '<drv>'...` for each build, and the daemon's log lines.

The exit code is 0, or 1 for an evaluation error, a daemon error or a build
failure. The real `nix-build` gives the same codes when its daemon hands
builds to a remote builder.

A run with one `FILE` or one `-E` uses the tracing cache (**The tracing
cache**, below). A run with more inputs evaluates each time.

Falls back on: `--dry-run`, `--drv-link`, `--run-env`, `--command`,
`--run`, `--repair`, `--check`, `--pure`, `--exclude`, `--keep`,
`-p`/`--packages`, `-`, and any other flag.

### `nix-shell`

Flags: `[FILE]`, `-E`/`--expr`, `-A`, `--arg`/`--argstr`, `-I`,
`--run CMD`, `--command CMD`, `--pure`, `--impure`, `--keep NAME`,
`--option`, `--store`, `-j`/`--max-jobs`, `--cores`, `-k`, `-K`,
`-Q`/`--no-build-output`, `--quiet`, `-v`, `--show-trace`, and
`--add-drv-link`, `--indirect` and `--no-gc-warning` (these three do
nothing in Nix's `nix-shell` either). With no `FILE`, or a directory, it
looks for `shell.nix` and then `default.nix`. `<name>` goes through the
search path. `-v` gives one warning, as for `nix-build`.

Nothing builds a `-env` derivation, as `print-dev-env` does. Instead:

1. It evaluates to exactly one derivation. If not, you get `nix-shell
   requires a single derivation`. A function with an `inNixShell` argument
   gets `inNixShell = true`.
2. It builds or fetches the derivation's **inputs**, through the daemon:
   every `inputDrvs` output and every `inputSrcs` path. It never builds the
   derivation itself.
3. It makes the environment: your current one (with `--pure`, only the
   variables Nix keeps), Nix's build variables, then every `drv.env` pair. A
   name in `passAsFile` goes to a file, and `<name>Path` points at it.
4. It writes the same rc file as Nix: the structured attributes,
   `source $stdenv/setup`, `shellHook`, and then the `--run` or `--command`
   command.
5. It runs `bash --rcfile <rc>` when the shell is interactive (stdin and
   stderr are a tty, and there is no `--run`). Else it runs `bash <rc>`.

The bash is `$NIX_BUILD_SHELL`. Else it is
`(import <nixpkgs> {}).bashInteractive`, built through the daemon. If that
does not evaluate, it is `bash` from your environment, with Nix's
`uses bash from your environment` notice.

Before it starts bash, iets links what it built in its roots store, so a
`nix collect-garbage` between two shells doesn't make the second one rebuild
everything. `shell-gc-root = false` turns this off
(`docs/explanation/caching.md`).

Falls back on: `-p`/`--packages`, a `#!` script (the first argument is a
file that starts with `#!`), `--exclude`, `--dry-run`, `--check`,
`--repair`, `-o`/`--out-link`, `--no-out-link`/`--no-link`, `-` (stdin), a
derivation that Nix would have to resolve first (an output that is not
input-addressed), `exportReferencesGraph` in a structured-attributes
derivation, and any other flag.

### `nix-eval-jobs`

This is `iets eval-jobs`, which matches nix-eval-jobs 2.35 flag for flag
(`docs/reference/commands.md`). Under this name it prints no summary line on
stderr, because nix-eval-jobs prints none.

### `nix`

iets handles these subcommands. Every other one runs the real `nix`.

Every subcommand takes the global flags: `--impure`, `--pure-eval`,
`--no-pure-eval`, `--accept-flake-config`, `--show-trace`, `--log-format`,
`--experimental-features` and `--extra-experimental-features`, `--option`,
`-I`/`--include`, `--arg`/`--argstr`, `--store`, `--flake-registry`,
`--eval-store`, `-j`/`--max-jobs`, `--cores`, `-k`/`--keep-going`,
`-K`/`--keep-failed`, `--quiet` and `-v`/`--verbose`.

#### `nix eval [INSTALLABLE]`

Flags: `--json`, `--raw`, `--expr E`, `-f`/`--file FILE`, `--impure`,
`--pure-eval`, `--no-pure-eval`, `-I`, `--arg`/`--argstr`, `--option`,
`--store`, `--show-trace`, `--experimental-features` and
`--extra-experimental-features`, `--accept-flake-config`, `--flake-registry`,
`-L`/`--print-build-logs`.

- `-L` has no effect: an evaluation that would build falls back.
- A search path entry that is a URL (`flake:nixpkgs`, `channel:nixos-unstable`,
  `https://…`) is split as Nix splits it, and `<name>` falls back when it
  reaches that entry: iets fetches no search path entry.

- `nix` evaluates purely unless you say otherwise, and `-f` is impure
  (`docs/explanation/evaluation.md`).
- The experimental features decide whether `builtins` has `fetchTree`,
  `fetchClosure` and `outputOf`, as in Nix. `parse-toml-timestamps` falls
  back.
- `--accept-flake-config` has no effect. It gives one warning when the
  flake on the command line has a `nixConfig`, as iets never applies one
  (`docs/reference/divergences.md`).

The output is `nix eval`'s, which is not the same as
`nix-instantiate --eval`'s:

- every value is forced;
- a derivation prints as `«derivation /nix/store/….drv»`;
- a function prints as `«lambda f @ FILE:L:C»`, `«primop add»` or
  `«partially applied primop add»` (a leading `__` is dropped);
- a value seen twice prints as `«repeated»`;
- a value that fails prints as `«error: …»`. Only an error at the top level
  is reported, with exit code 1.

`--json` forces everything and prints a derivation as its output path.
`--raw` prints the string without quotes.

An INSTALLABLE is `FLAKEREF[#ATTR]` on a locked local flake (`.`, `path:`,
or a directory in a git repository; see
`docs/explanation/flakes-and-sources.md`). With `-f` or `--expr`, it is an
attribute path into that value, and `.` is the whole value. For a flake,
iets tries these attribute paths, in order:

| fragment | candidates, in order |
|---|---|
| none | `packages.<system>.default`, `defaultPackage.<system>` |
| `.foo` | `foo` |
| `foo` | `packages.<system>.foo`, `legacyPackages.<system>.foo`, `foo` |

#### `nix build [INSTALLABLE...]`

Default `.`. Flags: `-o`/`--out-link PATH`, `--no-link`,
`--print-out-paths`, `-L`/`--print-build-logs`, `-f`, `--expr`, and the
global flags of `nix eval`.

It evaluates like `nix eval`, then builds through the daemon like
`nix-build`. It installs these outputs: the one named by `outputSpecified`,
else `meta.outputsToInstall`, else every output for `-f`/`--expr` and `out`
for a flake. The links are `result`, then `result-<n>` for the n-th
installable from 1 on, with `-<output>` after it for an output other than
`out`. stdout is empty without `--print-out-paths`. The exit code is 1 on an
evaluation, daemon or build error.

`--store grpc://host:port[?…]` builds on a nix-grpc-store service, with no
plugin and no `nix` on the other side. `--eval-store auto` (or `daemon`, or
empty) has no effect and gives one `ignored-flag` warning, because iets
evaluates in memory. So the command that the `nix-farm` wrapper runs works
as it is:

```
nix build --store 'grpc://farm.numtide.com:50051?token-file=…' --eval-store auto .#pkg
```

Any other `--eval-store` value falls back.

A build farm puts its outputs in a binary cache, so before it makes the
out-link, iets brings them in: your local daemon substitutes what it can,
and iets fetches the rest with their signatures. With `--store` set,
`--no-link` skips all of that. Without `--store`, the `builder` setting acts
like a build hook, so the outputs come back even with `--no-link`
(`docs/how-to/build-remotely.md`).

#### `nix derivation show [INSTALLABLE...]`

Default `.`. Flags: `-r`/`--recursive`, `--pretty`, `--no-pretty`,
`-f`/`--file`, `--expr`, and the global flags. There is no `--json`,
because the output is JSON, the same as Nix's byte for byte:

```json
{"derivations":{"<hash>-<name>.drv":{"args":[…],"builder":"…","env":{…},
"inputs":{"drvs":{"<hash>-<dep>.drv":{"dynamicOutputs":{},"outputs":["out"]}},
"srcs":["<hash>-<src>"]},"name":"…","outputs":{"out":{"path":"<hash>-<name>"}},
"structuredAttrs":{…},"system":"…","version":4}},"version":4}
```

- Every key is sorted, and every store path is its base name.
- `structuredAttrs` appears only when the derivation has `__json`, and then
  `env` leaves it out.
- An output is `{"path":…}` when it is input-addressed,
  `{"hash":…,"method":…}` when it is fixed (`flat` or `nar`; Nix prints no
  path here either), `{"method":…,"hashAlgo":…}` when it is floating, and
  `{}` when it is deferred.
- `--pretty` indents by two spaces. It is the default on a terminal.

`-r` adds every input derivation. iets takes them from the evaluation, and
reads `/nix/store` only for one the evaluation did not make, such as one
pinned with `builtins.storePath`. Nothing is written to the store ("Known
divergences").

#### `nix search INSTALLABLE REGEX...`

Flags: `-e`/`--exclude REGEX`, `--json`, `--pretty`, `--no-pretty`,
`-f`/`--file`, `--expr`, and the global flags. The output is Nix's, byte for
byte, escape codes too. iets caches the index of the package set, so a
second search over the same set answers without evaluating
(`docs/reference/commands.md`). An installable that iets cannot evaluate,
such as the registry name `nixpkgs` or an unlocked flake, falls back. So
does any other flag.

#### `nix print-dev-env [FLAKEREF[#attr]] [--json]`

This is `iets print-dev-env` (`docs/reference/commands.md`).

#### `nix develop [INSTALLABLE]`

Flags: `-c`/`--command CMD ARGS...` (it takes every argument after it, like
Nix), `-i`/`--ignore-env`/`--ignore-environment`,
`-k`/`--keep`/`--keep-env-var NAME`, `-u`/`--unset`/`--unset-env-var NAME`,
and the global flags.

The environment is the one `nix print-dev-env` prints, so the shell's
`-env` derivation is built through the daemon. Then iets adds what Nix adds:
the `~/.bashrc` wrapper for an interactive shell, `exec` for `--command`,
the `bash-prompt` settings as `PS1` lines, and `SHELL` and the bash
directory at the front of `PATH`.

`NIX_GCROOT` holds the `-env` output while the shell runs, as in Nix. iets
also links it in its roots store, so the shell survives a garbage
collection between two runs. `shell-gc-root = false` turns this off
(`docs/explanation/caching.md`).

`--ignore-env` clears the environment except the `--keep` names, and
`--unset` removes names. `NIX_GCROOT` and `SHELL` are set after that, so
they always survive.

The attribute paths it tries are those of `iets print-dev-env`
(`docs/reference/commands.md`).

Falls back on: `--phase`, `--unpack`, `--configure`, `--build`, `--check`,
`--install`, `--installcheck`, `--profile`, `--redirect`, more than one
installable, and any other flag.

#### `nix fmt [ARG...]`

`nix fmt` is Nix's short name for `nix formatter run`. It evaluates
`formatter.<system>` of the flake in your working directory, builds it, and
runs it. The installable is always `.`.

The program is `<output>/bin/<main>`. `main` is `meta.mainProgram`, else
`pname`, else `name` without its version. Every argument goes to the
formatter. The first `--` is dropped, and everything after it is an
argument, flags too. That is how you pass a flag to the formatter.
`PRJ_ROOT` is the flake directory, so a formatter that you run from a
subdirectory still sees the whole tree.

A flake with no `formatter` output fails with Nix's own message:

```
error: flake 'path:/home/you/proj' does not provide attribute 'formatter.x86_64-linux'
```

The formatter runs on your machine, whatever `builder` built it, so its
closure comes to your local store. iets links it in its roots store, so it
isn't deleted and rebuilt between runs. If the daemon refuses the root, you
get a warning and the formatter still runs (`docs/explanation/caching.md`).

A second `nix fmt` with nothing changed neither evaluates nor builds, even
when the formatter rewrote a file to the same bytes, because the cache looks
at content.

Falls back on: `--json`, `nix formatter build`, `nix formatter run`, a
working directory with no `flake.nix`, and any other flag.

#### `nix flake prefetch-inputs [FLAKEREF]`

Default `.`, with the global flags. It fetches every locked input that the
root of `flake.lock` reaches, on up to 25 threads (the default of
`http-connections`), each found the way an evaluation finds it
(`docs/explanation/flakes-and-sources.md`). A tarball-shaped input lands in
the input store, outside `/nix/store`. A `path:` input is read in place. If an
input fails, iets prints its error, goes on with the others, and exits 1.

Falls back on: a missing or incomplete `flake.lock` (the real one locks the
flake first), an input of another type, a flake reference that is not
local, and any other flag.

#### What `nix` falls back on

- `nix eval --apply`, `--write-to`, or more than one installable;
- `nix build --rebuild`, `--dry-run` or `--json`;
- `nix derivation show --stdin`;
- an installable that is a store path (`/nix/store/…`, or with an
  `^outputs` suffix);
- a flake reference that is not local, such as `nixpkgs#hello`;
- an installable that is not a derivation, under `nix build` or
  `nix derivation show`;
- `--arg`/`--argstr` with a flake (Nix refuses that);
- every `nix flake …` command except `prefetch-inputs`;
- `nix run`, `nix shell`, `nix profile`, `nix store`, `nix copy`,
  `nix repl`, `nix log`, `nix why-depends`, and every other subcommand;
- any flag not listed in this page.

`nix store prefetch-file` and `nix-prefetch-url` always fall back. Both add
the file to the store and print its path, and iets has no store to add it
to. `iets prefetch` prints the same hash and nothing else
(`docs/reference/commands.md`).

#### The tracing cache

Every `nix` subcommand that evaluates, and `nix-build`, uses the tracing cache
(`docs/explanation/caching.md`). Run the same command again with nothing
changed, and iets checks what the first run read and evaluates nothing. A
hit skips the evaluation and nothing else:

| subcommand | the entry holds | a hit still does |
|---|---|---|
| `nix eval` | the stdout bytes | nothing: `nix eval` registers no derivation here |
| `nix build` | each installable's derivation and outputs, every derivation and every source | the upload, the build, the out-links, the `--no-link` roots, `--print-out-paths` |
| `nix-build` | each root's derivation and output, every derivation and every source; every `-A` is in the key, in order | the upload, the build, the links, the `--no-out-link` roots, the printing |
| `nix derivation show` | the stdout bytes; `-r` and the indentation are in the key | nothing |
| `nix develop` | the `-env` output path and the shell's builder | reads the environment out of the output, keeps the root, runs bash; an output that is gone is a miss |
| `nix print-dev-env` | the script, the `-env` output path and the files the script names | checks that they exist |
| `nix search` | the index of the package set; the regexes are not in the key | the matching and the printing |
| `nix fmt` | the formatter's output path and program | keeps the root, runs the formatter; a program that is gone is a miss |

The key is made of the subcommand, the installables (a flake by its text
and resolved directory, never by content), `--expr` or `--file`, the
`--arg`s, `-I` and `NIX_PATH`, `pure-eval`, `--json`, `--raw`, the `system`
setting, the working directory and `--reference-lock-file`. Everything the
evaluation read is in the read set, so an edit, a `nix flake update` or a
changed variable makes it evaluate again. `--override-input`,
`--override-flake` and the flags that write a lock file fall back before
iets looks in the cache, and so does a flake with no complete `flake.lock`.

Unlike Nix's evaluation cache, it also answers for a dirty tree, a `path:`
flake, `--expr` and `-f` (`docs/reference/divergences.md`).

To turn it off, use `--iets-option cache false`, `IETS_CACHE=0`, or
`cache = false` in `iets.toml`. The cache flags of the native commands
(`--no-tracing-cache`, `--cache-dir`) are not Nix flags, so under `nix` they
fall back.

## Fallback rules

A name falls back in these cases:

1. **A flag, subcommand or setting iets does not handle,** seen before any
   work starts. A setting counts when it is a Nix client setting that iets
   has nothing for, with a value that is not Nix's default
   (`docs/reference/settings.md`, class 4). So does
   `nix-instantiate --store` with a store other than the local daemon.
2. **An evaluation that calls something iets does not support:**
   - `fetchMercurial` and `fetchClosure`;
   - `fetchGit` of an SCP-like URL (`user@host:path`) without a `rev` in
     pure evaluation;
   - `fetchTarball` and `fetchurl` without a `sha256`;
   - `fetchTree` with a URL string, with an input type other than `path`,
     `github`, `gitlab`, `sourcehut`, `git`, `tarball` or `hg`, or without
     a `narHash` for a type other than `path`, or, in pure evaluation,
     without a `rev` for a type other than `path` and `tarball`;
   - `getFlake` on anything but a local path, or in pure evaluation on a
     reference with a `rev`, or with a `narHash` and more than `path:`.
     `path:DIR?narHash=H` evaluates when the tree's NAR hash is `H`;
   - a flake reference iets does not parse (`hg+…`, `sourcehut:`, an
     IP-literal host; `docs/reference/divergences.md`);
   - `getContext` on the output of a dynamic derivation;
   - import from derivation: `import`, `scopedImport`, `readFile`,
     `readDir`, `pathExists`, `readFileType` or `hashFile` on a derivation
     output that is not in the store;
   - a second copy of a `toFile` text or `.drv`;
   - a read of a `.drv` this run made under `--no-tracing-cache`;
   - a flake that declares inputs and whose `flake.lock` is missing or
     incomplete.

   Any other evaluation error is reported, with exit code 1. The real Nix
   would fail the same way.

A fallback prints one line on stderr and then runs the real binary with the
same arguments:

```
iets: falling back to nix-instantiate: '--parse' is not implemented
```

| variable | effect |
|---|---|
| `IETS_NIX_BIN_DIR` | the directory the real binary is taken from |
| `IETS_NIX_FALLBACK=never` (`nix-fallback = "never"` in `iets.toml`) | print the reason and exit 1 instead of exec'ing |
| `IETS_NIX_FALLBACK=always` | run the real binary before iets reads anything else, the settings file included, so a broken setting does not stop it |

iets looks for the real binary in this order:

1. `IETS_NIX_BIN_DIR/<name>`, when it exists;
2. the first `<name>` on `PATH` that is not iets. When an entry is one of
   iets's classic mode commands, iets uses the Nix that the classic package was built
   for, which it names in `share/iets/real-nix-bin`;
3. the `<name>` next to the first `nix-store` on `PATH`. The classic
   package has no `nix-store`, so this finds the Nix installation even
   without that file.

If it finds nothing, you get
`iets: cannot fall back to <name>: not found on PATH`, and exit code 1.

A candidate counts as iets when it resolves to the running binary, **or**
when its file name is `iets`. A machine can have two builds of iets, each
classic package pointing at its own `bin/iets`. Without the file-name rule,
the other build would look like a real Nix, and a flake input fetch would
stop with `iets: recursive fetch ...`
(`docs/explanation/flakes-and-sources.md`).

## Config

Classic mode reads nix.conf as Nix does. `docs/reference/settings.md`
says what reaches the daemon, and which settings give a warning because they
have no effect. On the command line, `--option name value` and the short
forms (`--store`, `-j`/`--max-jobs`, `--cores`, `-k`, `-K`) apply in
command-line order.

`store = auto`, empty, or `daemon` means the daemon socket. iets handles
`unix://PATH`, `ssh-ng://HOST` and `grpc://HOST:PORT[?…]`. Any other store
falls back.

## Known divergences

`docs/reference/divergences.md` covers the evaluator. Classic mode adds
these:

- `store = auto` is the daemon socket. The real Nix uses the local store
  directly when it can write to it.
- `-Q`/`--no-build-output` drops the daemon's log lines. In Nix 2.34 it only
  switches the log format to `raw`. `--quiet` does the same here. In Nix it
  lowers the log level by one.
- A flag or setting that has no effect gives one warning on stderr, where
  Nix prints nothing: `-v`/`--verbose`, `--eval-store auto` and `log-lines`, and
  `--accept-flake-config` and `accept-flake-config = true` when the flake
  has a `nixConfig`. Any other client setting
  that iets has nothing for (`restrict-eval`, `eval-system`, …) falls back.
  stdout and the exit code stay Nix's. `docs/reference/divergences.md`,
  **Flags and settings with no effect**, has the table and says what iets
  does with a flake's `nixConfig`: nothing.
- `--show-trace` only adds the trace to the `--log-format internal-json`
  error records (`docs/reference/errors.md`). iets always prints the full
  trace it has.
- `--no-out-link` (`nix build --no-link`) makes no symlink, but iets keeps a
  GC root in its roots store. `build-gc-root = false` gives you Nix's
  behavior (`docs/reference/divergences.md`, "GC roots the shells and the
  builds keep").
- Finding derivations in a value walks derivations, attrsets (in name
  order, with `recurseForDerivations`) and lists. `_combineChannels` and
  `ignoreAssertionFailures` are not implemented.
- `--add-root` replaces a link that already exists and points outside the
  store. Nix refuses.
- A missing attribute in an installable's path is one line. Nix adds a
  `Did you mean …?` line, which iets does not have. The `error:` line is the
  same.
- The `nix` subcommands and `nix-build` answer from the tracing cache.
  stdout and the exit code are the same, and
  `docs/reference/divergences.md`, "The tracing cache under the classic
  names", says what differs on stderr. `nix-instantiate` and `nix-shell` do
  not use it.
- `nix eval` and `nix derivation show` do not register the derivations they
  evaluate. The real ones write the `.drv` files as they go, so a real
  `nix derivation show` leaves its whole input closure in the store. iets
  answers from memory and leaves the store as it was. If you relied on that
  side effect (`nix derivation show X`, then `nix-store --realise X`),
  instantiate first, with `nix-instantiate` or `iets build --no-build`.
- `nix build` prints log lines only with `-L`. The real one shows the last
  line in the progress bar, and the whole log when a build fails.
- Two installables that name the same derivation print one output path
  each. The real `nix build` prints twice as many.
- An evaluation error inside a value that `nix eval` prints becomes
  `«error: …»`. The exception is a call iets does not support: that ends the
  run, and the real Nix takes over.
- **`nix develop` takes bash from the shell derivation's own builder.** Nix
  builds `nixpkgs#bashInteractive` from the flake's `nixpkgs` input or the
  registry, and that needs a fetcher iets does not have. The builder of a
  derivation that `nix develop` accepts is a `bin/bash` in the store. In a
  nixpkgs dev shell it is the same store path Nix picks, so `SHELL` and the
  `PATH` prefix come out the same. If there is no such file, iets takes
  `bash` from `PATH` and leaves out the `PATH` prefix line, as Nix does when
  `bashInteractive` fails.
- `nix develop` sets no profile, so `--profile` falls back.
- `nix-shell --pure` keeps the lower-case proxy variables (`http_proxy` …)
  and drops the upper-case ones, as Nix does through a bug.
- `nix-shell` takes `NIX_BUILD_CORES` from the `cores` setting, else from
  the cgroup2 `cpu.max` of its own cgroup, else from the CPU count. Unlike
  Nix, it does not check `/proc/mounts`.
