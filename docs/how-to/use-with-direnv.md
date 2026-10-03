# How to use iets with direnv

`iets print-dev-env` prints the bash that `nix print-dev-env` prints, byte
for byte. It replaces nix-direnv's `use flake` in an `.envrc`, and it
answers from the cache in tens of milliseconds instead of seconds.

The flags are in [reference/commands.md](../reference/commands.md).

## Write the `.envrc`

Add this to your `.envrc`:

```bash
use_iets() {
  local root entry script
  local -a paths
  root="$(direnv_layout_dir)/iets-env"
  entry="$(direnv_layout_dir)/iets-entry"
  watch_file flake.nix flake.lock
  script="$(iets print-dev-env --builder daemon --gc-root "$root" --entry-to "$entry" "${1:-.}")" || true
  if [[ -s $entry && -e $(<"$entry") ]]; then
    mapfile -d '' -t paths < <(iets cache paths "$(<"$entry")" 2>/dev/null)
    ((${#paths[@]})) && watch_file "${paths[@]}"
  fi
  eval "$script"
}
use_iets
```

Then `direnv allow`.

Four parts matter:

- `--builder daemon` builds or substitutes the shell's inputs on the local
  daemon. The shell runs here, so its inputs belong in the local store.
- `--gc-root PATH` makes `PATH` a symlink to the environment's store path
  and registers it as an indirect garbage-collection root. That is what
  nix-direnv's `--profile` does with a profile. Without it, the next
  `nix-collect-garbage` deletes the environment and the following `direnv
  reload` rebuilds it.
- `watch_file flake.nix flake.lock` is the floor. It is what nix-direnv
  watches, and it is all direnv has when there is no cache entry yet.
- `--entry-to FILE` names the cache entry the run read, and `iets cache
  paths ENTRY` prints the paths a watcher has to watch, NUL-separated. The
  `mapfile` turns them into one `watch_file` call: iets prints data, direnv's
  calling convention stays in your `.envrc`. Do it before `eval "$script"`,
  so that a shell hook that fails does not lose the watches.
- **One `watch_file` call, not one per path.** The stdlib's `watch_file`
  forks the direnv binary every time it is called, and it takes as many
  paths as you give it. For 500 paths that is 25 ms in one call against
  7.3 s one at a time. The NUL separator is what lets `mapfile` build the
  array without splitting on whitespace in a path.
- Passing the entry, rather than deriving its key again, is what lets the
  two calls disagree about flags without silently finding nothing.

The printed bash is what `nix develop` sources: variable assignments,
exports, `declare -a` and `declare -A`, function definitions, the `PATH`
and `XDG_DATA_DIRS` prepend, `NIX_BUILD_TOP` from `mktemp`, and
`eval "${shellHook:-}"`. There is no `exit`, no `set -e` and no `exec`. On
an error nothing goes to stdout and the status is non-zero, so `eval` of an
empty string leaves your environment alone.

## What direnv reloads on

nix-direnv watches `flake.nix` and `flake.lock` and leaves the rest to
`direnv reload`. iets does not have to guess. The tracing cache records
every observation the evaluation made
([explanation/caching.md](../explanation/caching.md)), and `iets cache paths`
turns that read set into direnv watches:

| the evaluation | direnv watches |
|---|---|
| read a file: `import ./nix/shell.nix`, `readFile`, `importJSON`, `flake.nix`, `flake.lock`, a file of a `path:` input | the file |
| listed a directory: `readDir ./conf.d` | the directory, whose mtime changes when an entry comes or goes |
| asked for the kind of a path that is **missing**: `pathExists ./local.nix`, the nixpkgs `config.nix` probes | the path, because direnv sees it appear |
| asked for the kind of a path that **exists** | nothing: existence was the observation, and an mtime is not that (a git index refresh moves `.git`) |
| copied a path into the shell: `"${./hook.sh}"`, `./scripts` | the path and everything the copy took below it |
| asked the source control system about a working tree | the file that the system names; for git, the index, which `git add`, a commit and a checkout rewrite |

In a git flake an untracked file is not part of the flake. `git add` makes
it one, the index changes, and direnv reloads.

Some things are left out on purpose:

- Everything under `/nix/store` and in iets's own stores. Those paths do
  not change.
- The flake's source tree as a whole. Every edit of every file changes it,
  and few shells depend on it, so a watch on it would cost an evaluation for
  each saved source file. direnv watches the files the evaluation read out
  of the tree and the paths the shell copied, and no more. A shell that uses
  `self` or `./.` needs `direnv reload`, or a `watch_file` of your own.
- Environment variables (`getEnv` under `--impure`). direnv has no watch for
  a variable, and it does not run `.envrc` again when one changes. The cache
  still checks them, so the next reload for any other reason evaluates again.
- More than 500 paths. direnv stats every watched path on every prompt, so
  the list has a bound: 500 watches add about 3 ms to a prompt. iets keeps
  the first 500 in the order the evaluation read them, and says so on
  stderr. A usual flake shell has 10 to 20.

`iets cache paths` validates nothing and evaluates nothing: it reads one
cache entry and prints. If the evaluation fails, the entry of the last good
run is still there, so direnv still watches the file you have to fix.

A first load that never evaluated writes no entry, so `--entry-to` leaves
the file empty and the `[[ -s $entry ]]` guard skips the loop. direnv then
has `flake.nix` and `flake.lock` and nothing else, which is the floor and is
correct: nothing is known about a shell that never evaluated. The real error
is on stderr; fix that one.

## Use the NixOS module instead

If you run NixOS, import `nixosModules.default`. It installs iets and
replaces nix-direnv's `use flake` with the repository's own `direnvrc`, for
local flake references with an optional `#attr`. Keep `use flake` in your
`.envrc` and change nothing else.

## Pick a different shell

Without a fragment, `print-dev-env` takes the first of
`devShells.<system>.default`, `devShell.<system>`,
`packages.<system>.default` and `defaultPackage.<system>`:

```sh
iets print-dev-env .
```

With `#x` it takes the first of `devShells.<system>.x`,
`packages.<system>.x`, `legacyPackages.<system>.x` and `x`:

```sh
iets print-dev-env .#ci
```

## Check what it costs

`--timings` reports the cache and the total on stderr, leaving stdout for
the bash:

```sh
iets print-dev-env --timings . > /dev/null
```

A cold run on this repository:

```
total: 0.703s
```

The same command again:

```
cache: hit, 2106 entries validated, 0 rehashed, load 0.001s, validate 0.017s
total: 0.027s
```

A hit validates the read set, stats the output, reads the garbage-collection
root's link, and prints. Nothing talks to the daemon unless the root has to
be created again. If the environment was garbage collected, the run falls
back to the full path and rebuilds it.

For scale, on a NixOS flake with 31 locked inputs and a devShell:
`nix print-dev-env --option eval-cache false` took 3.48 s, a cold
`iets print-dev-env` took 0.67-0.83 s, and a cached one 21-35 ms.

## Inspect the environment as JSON

```sh
iets print-dev-env --json .
```

This is what `nix print-dev-env --json` prints.

## What it does under the hood

Five steps, following Nix's own `develop.cc`, so you can predict when it
will be slow:

1. Evaluate the shell derivation through the normal flake path, so the
   tracing cache applies.
2. Derive the `-env` derivation in memory, as `getDerivationEnvironment`
   does: same inputs, the arguments replaced by Nix's own `get-env.sh`
   (embedded verbatim and added to the store as a text path), the same
   `bash` builder, `-env` appended to the name, the output checks removed,
   and the output paths recomputed. The result is the same `.drv` Nix
   writes, which is checked by a unit test, so a shell `nix develop`
   already built is not built again.
3. Ask the daemon whether it has the environment's output. If not, plan the
   closure from the in-memory derivation table, upload what the daemon
   lacks, and build.
4. Read the first non-empty output — from disk for a local daemon, over the
   protocol for `ssh-ng://` — and parse it.
5. Print the same script Nix prints: the same header, the same ignored
   variables, the same footer, and the same rewrite of every output path to
   `<cwd>/outputs/<name>`. A `__structuredAttrs` shell gets its `.attrs.sh`
   and `.attrs.json` written next to the garbage-collection root, or under
   a temporary directory when there is none. Nix writes them to a temporary
   directory it deletes on exit.

## What is not supported

- A shell whose builder is not `bash`, and a content-addressed shell. Both
  are errors.
- `--gc-root` together with `ssh-ng://`: the root would name a path this
  machine has not got.
- `nix develop`'s `--redirect`, `--profile`, `--command` and `--phase`.
- The `nix develop` interactive extras (`bashInteractive`, prompt
  settings).
