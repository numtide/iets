# iets - at least it's *something*

> DISCLAIMER: this project is LLM-ware. This README is written by my little
> hands. The rest of the project is LLM output with *some* level of
> supervision. Mostly Opus 5.5 and Fable 5.1 at the time of writing.

iets is a re-implementation of the Nix client that focuses on providing a good
out of the box user experience. The input is based on more than a decade of
using the language and seeing how the community and customers use it. There is
a jump we can make forward.

```sh
iets build -A hello ./nixpkgs                  # nix-build
iets build .#checks                            # nix-fast-build
iets eval-jobs --flake .#hydraJobs             # nix-eval-jobs
iets search -I nixpkgs=./nixpkgs ripgrep       # nix search
iets diff -A hello -A figlet ./nixpkgs         # nix-diff; --closure is nvd
iets prefetch --unpack https://example.org/x.tar.gz # nurl, nix-prefetch
iets print-dev-env --gc-root .direnv/env .     # nix-direnv's `use flake`
```

## Features

Some topics I had in mind:

Sometimes Nix hangs. Many beginners hit Ctrl-C thinking Nix is stuck. It was
just evaluating, with no feedback, and slowly. iets fixes that by making eval
2-4.7x faster than Nix 2.34.8 on NixOS configs (see `bench/`). And by letting
you know what it's up to.

Bumping FODs (fixed-output derivations) means putting a bad hash, running a
build, and then replacing the errored hash with the right one. iets detects
the error, fixes the hash in your source, and builds again. See
[docs/how-to/fix-hashes.md](docs/how-to/fix-hashes.md).

Nix eval is impure, unless you use flakes. No hate against flakes, they are
supported out of the box, but eval puring shouldn't be coupled to them. iets
adds introspection tools to detect when impurities leak into evals or builds.
See [docs/reference/lints.md](docs/reference/lints.md).

Eval caching is done by tracing all the files being used for a particular
eval. That means eval caching is supported on all operations, not just flakes.
It is also more precise. So running `nix fmt` through iets is pretty instant.
See [docs/explanation/caching.md](docs/explanation/caching.md).

iets doesn't need a local `/nix/store` to work. It's a single binary you can
have in your CI environment, and that pushes all the builds to a Nix daemon or
a remote builder over `ssh-ng://` or `grpc://` (like https://nixbuild.net).
See [docs/how-to/build-remotely.md](docs/how-to/build-remotely.md).

It's a drop-in. Call it as `nix`, `nix-build`, `nix-shell` or
`nix-instantiate` and it behaves like that command. Whatever it doesn't
implement goes to the real `nix`. See
[docs/reference/classic.md](docs/reference/classic.md).

It's also made for scripts and agents. Every command speaks JSON, has stable
exit codes, and errors come with a code, a position and often the fix. See
[docs/how-to/script-iets.md](docs/how-to/script-iets.md).

iets's evaluator keeps track of which source some Nix code comes from. That
means we can enable/disable language features based on the "package". And
start to evolve the language without it being an all-or-nothing situation.

Speaking of which (experimental): we also keep track of the source to drv
correspondence. So when a build fails, we can tell you where the error came
from, in your source instead of the /nix/store. Sometimes. This one still
needs more work. See
[docs/explanation/provenance.md](docs/explanation/provenance.md).

## Scope

For now iets is 100% a client and there is no nix-daemon implementation. I'm
trying to strike a balance and make it featureful enough that it provides a
good out-of-the-box user experience and shortens the list of tools needed to
use Nix day-to-day, without becoming a kitchen sink.

## Not supported (yet)

Some things iets doesn't do. Under the classic names, most of these fall
back to the real `nix`, so you mostly won't notice.

* No nix-daemon, and no store of its own. Builds go to a daemon or a remote
  builder.
* Flakes have to be local and locked. No registry refs like `nixpkgs#hello`,
  no remote roots like `github:NixOS/nixpkgs#hello`, and no `nix flake
  lock`. Remote *inputs* in a `flake.lock` are fine.
* No `git` or `hg` fetching. That means `fetchGit` and git inputs too.
* No import-from-derivation (IFD).
* Unpinned `fetchurl`, `fetchTarball` without a `sha256`, and `fetchClosure`
  aren't there either.
* Only a handful of `nix` subcommands are native. The rest run the real
  `nix`.
* The library never frees memory, so don't embed it in a long-running
  program yet.
* Lots of other small details. iets is still very fresh.

The full list is in
[docs/reference/classic.md](docs/reference/classic.md#fallback-rules) and
[docs/reference/divergences.md](docs/reference/divergences.md).

## Project status / LLM-ware

As said in the disclaimer, this project is LLM-ware. Which is a very different
way to write software than the traditional way. Traditionally we would write
everything from the ground up. Designing the architecture. Picking the right
libraries. When done rigorously, we would get a sense of if a project is done
well or not.

With LLMs it's top-down.

I spent a lot of time verifying the outputs, which is pretty easy to do with
Nix. And poked at the architecture in different angles. But the insides of
the project are not 100% understood by me. And not necessarily the most
pleasing to read. At some levels, the software is of better quality than if I
had written it by hand. And then in others, there could be gaping holes the
LLM pretended was fine, because I didn't prompt it correctly.

How to get to a level of craft that makes me proud is still an open question
for me. And I think the best way to find out is to lean into it.

## Installation

Build it from source:

```sh
nix develop -c cargo build --release
./target/release/iets eval --strict -E '1 + 1'
./target/release/iets help
```

On NixOS, import `nixosModules.default` and set `iets.classic.enable = true`.
That puts the classic names (`nix-build`, `nix-shell`, ...) ahead of `nix` in
your path.

## Documentation

`./docs` is LLM-generated, but it should be useful as reference. Start with
[docs/tutorial/getting-started.md](docs/tutorial/getting-started.md). The
binary also carries the reference docs, so `iets help TOPIC` works from
anywhere. [ARCHITECTURE.md](ARCHITECTURE.md) is the map of the code.

## Credits

* All the computer scientists and OSS developers whose output was used to
  train the models.
* The agents also looked at code from [Nix](https://github.com/NixOS/nix),
  [fix](https://github.com/psyclyx/fix) and
  [Determinate Nix](https://github.com/DeterminateSystems/nix-src) when
  comparing for speed.

## Licence

LGPL-2.1-or-later, like Nix (see COPYING). Some files are copied from Nix
(corepkgs, `call-flake.nix`, `get-env.sh`, the `tests/lang` suite) under the
same licence. `proto/nix_remote.proto` comes from Mic92/nix-grpc-store, which
carries no licence yet.
