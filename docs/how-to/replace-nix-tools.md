# Replace your Nix tools with iets

You probably have a handful of tools next to Nix: one to search, one to diff
derivations, one to prefetch hashes, one to evaluate jobsets. iets does all
of that in one binary. Here is what to type instead of each.

| you used | use instead |
|---|---|
| `nix search` | `iets search` |
| `nix-diff` | `iets diff` |
| `nvd` | `iets diff --closure` |
| `nurl`, `nix-prefetch`, `nix-prefetch-url` | `iets prefetch` |
| `nix-fast-build` | `iets build .#checks` |
| `nix-eval-jobs` | `iets eval-jobs` |
| nix-direnv's `use flake` | `iets print-dev-env` |

Every flag is in [Commands](../reference/commands.md), and the deliberate
differences are in
[Known divergences from Nix 2.34](../reference/divergences.md). To keep your
habits, iets also answers to `nix`, `nix-build`, `nix-instantiate`,
`nix-shell` and `nix-eval-jobs`, and runs the real `nix` for the rest
([Classic mode](../reference/classic.md)).

The examples use `$nixpkgs` from "Get nixpkgs" in
[Get started with iets](../tutorial/getting-started.md). iets evaluates
purely by default, so it has no `<nixpkgs>` search path and no
`builtins.currentSystem`. An expression that reads either needs
`--impure`.

## Replace `nix search`

```sh
iets search -I nixpkgs=$nixpkgs '^ripgrep$'
```

```
* ripgrep (15.2.0)
  Utility that combines the usability of The Silver Searcher with the raw speed of grep
```

The output is Nix 2.34.8's, byte for byte, escape sequences included, so
the colors are written even when stdout is not a terminal. Use `--json` when
a program is reading.

The first search over a package set has to evaluate it. iets caches the index
of packages, so every later search over the same set is a hit:

```sh
iets search --timings -I nixpkgs=$nixpkgs fd find
```

```
cache: hit, 56568 entries validated, 0 rehashed, load 0.017s, validate 0.038s
```

On the pinned nixpkgs, the first search took 6.5 s when last measured, and a
hit takes under 0.1 s. Real `nix search` took 21 s on the same installable.

With no installable, `iets search` searches `<nixpkgs>` from `-I` or
`NIX_PATH`. To search something else, name it:

```sh
iets search ./pkgs.nix '^'
iets search path:/some/flake ripgrep
iets search --impure --expr 'import <nixpkgs> {}' git gui
```

`iets search` has no flake registry, so `nix search nixpkgs ^` under the
classic mode command goes to the real `nix`.

## Replace `nix-diff`

The examples from here on use this `demo.nix`, two derivations that differ in
one word and one variable:

```nix
let
  mk = word: greeting: derivation {
    name = "demo";
    system = "x86_64-linux";
    builder = "/bin/sh";
    args = [ "-c" "echo ${word} > $out" ];
    GREETING = greeting;
  };
in
{
  a = mk "one" "hello";
  b = mk "two" "goodbye";
}
```

```sh
iets diff -A a -A b demo.nix
```

```
- /nix/store/623kpc3y80b1cmfpp41ndkx8y2ila1az-demo.drv
+ /nix/store/sx7zfirysrh620gfmv0ycy5p3jwfzpzz-demo.drv
  args:
    @@ line 2
    - "echo one > $out"
    + "echo two > $out"
  env GREETING:
    - hello
    + goodbye
```

`-` is the first side and `+` the second. `-A X -A Y FILE` compares two
attributes of one file. You can also give two installables (a `.drv` path, a
flake reference or a Nix file), each of which must evaluate to exactly one
derivation:

```sh
iets diff /nix/store/…-a.drv /nix/store/…-b.drv
```

Exit codes follow `diff(1)`: 0 when they are equal, 1 when they differ.
Nothing is built and nothing is written to the store. iets follows input
derivations two levels down. `--depth N` goes deeper and `--all` goes to the
bottom.

`--json` prints one object with every difference as an entry of `changes`:

```sh
iets diff --json -A a -A b demo.nix
```

```json
{"a":"/nix/store/623k…-demo.drv","b":"/nix/store/sx7z…-demo.drv",
 "changes":[{"a":"\"-c\"\n\"echo one > $out\"\n","b":"\"-c\"\n\"echo two > $out\"\n","key":"","what":"args"},
            {"a":"hello","b":"goodbye","key":"GREETING","what":"env"}],
 "equal":false}
```

## Replace `nvd`

`nvd` tells you which packages and versions changed. `--closure` does the
same for the output closures:

```sh
iets diff --closure -A a -A b demo.nix
```

```
the two closures hold the same packages
```

When they differ, each line is one package:

```
removed libfoo 1.2
added   libbar 0.9
changed hello 2.12 -> 2.12.1
```

Both outputs must be in the store, because iets asks the daemon about them.
If one is missing, iets stops and names the path, as it does with `demo.nix`
until you build both attributes. `--builder URL` picks the store to read.

## Replace `nurl`, `nix-prefetch` and `nix-prefetch-url`

`iets prefetch` gives you the hash of something you haven't fetched yet. It
prints one line on stdout and nothing else, so `$(iets prefetch …)` works in
a command substitution.

A URL, hashed flat, as `nix store prefetch-file` hashes it:

```sh
iets prefetch https://github.com/numtide/treefmt/archive/v2.0.0.tar.gz
```

```
sha256-/Sb+qZOLHhmr1D63ANxTxkhA4uTOpn0IcUO2UmQyRuo=
```

`--unpack` gives the NAR hash of the unpacked tarball, which is what
`fetchTarball`, `fetchzip` and `fetchFromGitHub` want:

```sh
iets prefetch --unpack https://github.com/numtide/treefmt/archive/v2.0.0.tar.gz
```

```
sha256-Ckvpb2at7lg7AB0XNtIxeo8lWXX+04MVHHKoUs876dg=
```

`--hash-type ALGO` picks `md5`, `sha1`, `sha256` (the default) or `sha512`.

For anything a `fetch*` function makes, give an expression, a file with
`-A`, or a flake fragment in place of a URL:

```sh
iets prefetch --impure -E '(import <nixpkgs> {}).hello.src'
iets prefetch --impure -E 'with import <nixpkgs> {}; fetchzip {
    url = "https://github.com/numtide/treefmt/archive/v2.0.0.tar.gz";
    hash = ""; }'
iets prefetch default.nix -A mypkg.cargoDeps
```

iets ignores the hash in the expression, so `hash = ""`, `lib.fakeHash` and a
stale hash all work, and it doesn't edit your files
([Commands](../reference/commands.md) says how). Unlike `nix store
prefetch-file`, `iets prefetch` adds nothing to any store.

## Replace `nix-fast-build`

`iets build` takes a whole set of derivations, so give it the checks of a
flake:

```sh
iets build .#checks
```

```
▶ building checks.x86_64-linux.one (/home/you/fb/flake.nix:4:7)
▶ building checks.x86_64-linux.two (/home/you/fb/flake.nix:10:7)
✔  checks.x86_64-linux.one (/home/you/fb/flake.nix:4:7) (0s)
✔  checks.x86_64-linux.two (/home/you/fb/flake.nix:10:7) (0s)
2 built, 0 failed
/nix/store/…-one
/nix/store/…-two
```

With no flags it picks the checks for the current system, reports each
check on its own, and skips what the builder already has. The exit code is
Nix's: 100 when a build failed.

```sh
iets build --builder ssh-ng://builder .#checks
iets build release.nix -A ci --argstr system x86_64-linux
iets build --json .#checks > outcomes.json
```

`--json` gives you the report: `{"outcomes":[{attr, drv, success, outputs,
reason, logTail}]}`. There's no JUnit file, so convert the JSON if you need
one. Outputs stay where they were built, and pushing them to a binary cache
is the builder's job ([Build on a remote builder](build-remotely.md)).

## Replace `nix-eval-jobs`

```sh
iets eval-jobs --no-instantiate demo.nix
```

```json
{"attr":"a","attrPath":["a"],"constituents":[],"drvPath":"/nix/store/623k…-demo.drv","globConstituents":false,"name":"demo","namedConstituents":[],"outputs":{"out":"/nix/store/9ymw…-demo"},"storeDir":"/nix/store","system":"x86_64-linux"}
{"attr":"b","attrPath":["b"],…}
```

Those are the JSON lines nix-eval-jobs 2.35 prints, and the flags you
already pass from Hydra or nixpkgs CI work as they are
(`tests/e2e_evaljobs.rs` compares both tools). Check these differences
before you swap it in:

- `--workers N` counts worker processes. The default is the CPU count,
  capped at 4. With `--max-memory-size 0` it counts fibers in one process.
- Line order is completion order, as it is for nix-eval-jobs. Sort by
  `attrPath` when you compare two runs.
- `--override-input`, `--override-flake` and `--repair` are errors. iets only
  evaluates a flake from a complete lock file. `--reference-lock-file`
  works: that file is the lock, and it must be complete too.

`--provenance` and `--chunks` exist only in iets. `--provenance` adds a `pos`
key to each job with the file, line and column where the derivation was
defined. It's off by default, so the output matches nix-eval-jobs.
`--chunks M --chunk N` splits a jobset, which is how nixpkgs CI shards its
evaluation.

## Replace nix-direnv

Setup is on its own page: [Use iets with direnv](use-with-direnv.md).
