# Use iets with direnv

If you use nix-direnv, you know the wait after every `flake.nix` edit and the
cold start in a fresh checkout. `iets print-dev-env` prints the same bash as
`nix print-dev-env`, byte for byte, so you can put it in your `.envrc` in
place of `use flake`. A cached run takes tens of milliseconds.

The flags are in [Commands](../reference/commands.md). How iets makes the
shell is in [How `print-dev-env` makes a shell](../explanation/dev-shells.md).

## The `.envrc`

Add this to your `.envrc`:

```bash
use_iets() {
  local root entry script
  local -a paths
  root="$(direnv_layout_dir)/iets-env"
  entry="$(direnv_layout_dir)/iets-entry"
  mkdir -p "$(direnv_layout_dir)"
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

Then run `direnv allow`.

What each part does:

- `mkdir -p` creates direnv's layout directory. A new project doesn't have
  one, and `--gc-root` and `--entry-to` can't write there.
- `--builder daemon` builds or substitutes the shell's inputs on your local
  daemon. The shell runs here, so its inputs belong in the local store.
- `--gc-root PATH` makes `PATH` a symlink to the environment and registers
  it as a garbage-collection root, like nix-direnv's `--profile`. Without
  it, the next `nix-collect-garbage` deletes the environment and the next
  `direnv reload` builds it again.
- `watch_file flake.nix flake.lock` is the minimum. nix-direnv watches the
  same two files, and until the first cache entry exists they are all direnv
  has.
- `--entry-to FILE` writes the name of the cache entry the run used.
  `iets cache paths ENTRY` prints the paths to watch, separated by NUL
  bytes, and `mapfile` turns them into one `watch_file` call. Passing the
  entry name means the two commands never need matching flags. Do this
  before `eval "$script"`, so a failing shell hook can't lose the watches.
- **Call `watch_file` once for all paths.** Each call
  starts the direnv binary again. For 500 paths that's 25 ms in one call
  against 7.3 s one at a time. The NUL separator lets `mapfile` keep a path
  with spaces in one piece.

If evaluation or the build fails, nothing goes to stdout and the status is
non-zero, so `eval` gets an empty string and your environment stays as it was.

## Reloading

nix-direnv watches `flake.nix` and `flake.lock` and leaves the rest to you
and `direnv reload`. iets can do better, because its cache records everything
the evaluation looked at
([How caching works](../explanation/caching.md)).
`iets cache paths` turns that record into direnv watches:

| the evaluation | direnv watches |
|---|---|
| read a file: `import ./nix/shell.nix`, `readFile`, `importJSON`, `flake.nix`, `flake.lock`, a file of a `path:` input | the file |
| listed a directory: `readDir ./conf.d` | the directory, whose mtime changes when an entry comes or goes |
| asked for the kind of a path that is **missing**: `pathExists ./local.nix`, the nixpkgs `config.nix` probes | the path, because direnv sees it appear |
| asked for the kind of a path that **exists** | nothing: existence was the observation, and an mtime is not that (a git index refresh moves `.git`) |
| copied a path into the shell: `"${./hook.sh}"`, `./scripts` | the path and everything the copy took below it |
| asked the source control system about a working tree | the file that the system names; for git, the index, which `git add`, a commit and a checkout rewrite |

In a git flake, an untracked file is not part of the flake. `git add` makes
it one, the index changes, and direnv reloads.

iets leaves some things out on purpose:

- Everything under `/nix/store` and in iets's own stores. Those paths never
  change.
- The flake's source tree as a whole. Any edit to any file changes it, and
  few shells depend on it, so watching it would cost an evaluation on every
  save. direnv watches the files the evaluation read and the paths the shell
  copied, and no more. If your shell uses `self` or `./.`, run
  `direnv reload` yourself or add your own `watch_file`.
- Environment variables (`getEnv` under `--impure`). direnv can't watch a
  variable. The cache does check them, so the next reload for any other
  reason evaluates again.
- Paths beyond the first 500. direnv checks every watched path at every
  prompt, and 500 watches add about 3 ms. iets keeps the first 500, in the
  order the evaluation read them, and tells you on stderr. A typical flake
  shell has 10 to 20.

`iets cache paths` evaluates nothing: it reads one cache entry and prints. If
a later evaluation fails, the entry of the last good run is still there, so
direnv keeps watching the file you need to fix.

If the very first load fails, there is no entry. `--entry-to` leaves the
file empty, and the `[[ -s $entry ]]` check skips the watches. direnv then
watches only `flake.nix` and `flake.lock`, because iets knows nothing about
a shell that never evaluated. Fix the error on stderr and reload.

## NixOS module

On NixOS you can skip `use_iets`: import `nixosModules.default` on a host
that has direnv:

```nix
{
  imports = [ iets.nixosModules.default ];
  programs.direnv.enable = true;
}
```

`iets.direnv.enable` follows `programs.direnv.enable`. When it is on, the
module swaps nix-direnv's `use flake` for this repository's own `direnvrc`,
so your `.envrc` keeps saying `use flake`. It takes a local flake reference
with an optional `#attr`, and the flake needs a `flake.lock`. The module never
turns on direnv itself. Set `iets.direnv.enable = false` to keep nix-direnv's
`use flake`.

## Other shells

Without a fragment, `print-dev-env` takes `devShells.<system>.default` and a
few older names. With `#x`, as in `iets print-dev-env .#ci`, it looks for `x`
under `devShells`, `packages` and `legacyPackages`
([Commands](../reference/commands.md#print-dev-env) has the order).

## Timings

`--timings` prints the cache result and the total time on stderr, and leaves
stdout for the bash:

```sh
iets print-dev-env --timings . > /dev/null
```

## JSON environment

To see what the shell would set without loading it:

```sh
iets print-dev-env --json .
```

The output is what `nix print-dev-env --json` prints.

## Limits

iets can't make a shell whose builder isn't `bash`, a content-addressed
shell, or `--gc-root` together with `ssh-ng://`. It doesn't implement
`--redirect`, `--profile`, `--command` or `--phase` of `nix develop`. The
details are in [Commands](../reference/commands.md#print-dev-env).
