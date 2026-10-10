# iets - at least it's *something*

> DISCLAIMER: this project is LLM-ware. This README is written by my little
> hands. The rest of the project is LLM output with *some* level of
> supervision. Mostly Opus 5.5 and Fable 5.1 at the time of writing.

Maintainer: [zimbatm](https://github.com/zimbatm), under the Numtide org.

iets is a re-implementation of the Nix client (in Rust). With inputs based on
more than a decade of using the language and seeing how the community and
customers use it.

While iets remains [mostly, but not fully](docs/reference/divergences.md)
compatible with Nix, we are also exploring new ideas. And find interfaces that
allow reducing the amount of tools needed to operate with Nix day-to-day.

```sh
iets build -A hello ./nixpkgs                  # nix-build
iets build .#checks                            # nix-fast-build
iets eval-jobs --flake .#hydraJobs             # nix-eval-jobs
iets search -I nixpkgs=./nixpkgs ripgrep       # nix search
iets diff -A hello -A figlet ./nixpkgs         # nix-diff; --closure is nvd
iets prefetch --unpack https://ftp.gnu.org/gnu/hello/hello-2.12.1.tar.gz # nurl, nix-prefetch
iets print-dev-env --gc-root .direnv/env .     # nix-direnv's `use flake`
```

## Features

Some of the ideas that went into iets:

* **Fast eval, and it tells you what it is doing.** Sometimes Nix hangs. Many
  beginners hit Ctrl-C thinking Nix is stuck. It was slow evaluating, with no
  feedback, and slowly. iets makes eval 1.7-1.9x faster than Nix 2.34.8 on
  NixOS configs on one core, and 3.1-3.5x on four (see
  [Benchmarks](#benchmarks)), and says what it is up to while it works.

* **Hash bumping that bumps itself.** Bumping FODs (fixed-output derivations)
  means putting a bad hash, running a build, and then replacing the errored
  hash with the right one. iets detects the error, fixes the hash in your
  source, and builds again. See
  [docs/how-to/fix-hashes.md](docs/how-to/fix-hashes.md).

* **Purity without flakes.** Nix eval should be pure by default, to make the
  language follow it's own principles of purity, regardless of flakes.
  Flakes are supported too. iets adds introspection tools to detect when
  impurities leak into evals or builds. See
  [docs/reference/lints.md](docs/reference/lints.md).

* **Eval caching everywhere.** Caching uses file tracing, so it works on all
  evals. And is more precise than upstream's flake eval cache. Running `nix
  fmt` through iets is feels instant.
  See [docs/explanation/caching.md](docs/explanation/caching.md).

* **No local store needed.** A single binary for your CI environment, that
  pushes the builds to a Nix daemon or a remote builder over `ssh-ng://`
  (eg: https://nixbuild.net) or
  [`grpc://`](https://github.com/Mic92/nix-grpc-store). See
  [docs/how-to/build-remotely.md](docs/how-to/build-remotely.md).

* **A drop-in.** Call it as `nix`, `nix-build`, `nix-shell` or
  `nix-instantiate` and it behaves like that command. Whatever it doesn't
  implement goes to the real `nix`. See
  [docs/reference/classic.md](docs/reference/classic.md).

* **Made for scripts and agents.** Every command speaks JSON, has stable exit
  codes, and errors come with a code, a position and often the fix. See
  [docs/how-to/script-iets.md](docs/how-to/script-iets.md).

* **The evaluator knows where code came from.** It tracks which source a piece
  of Nix comes from, so language features can be enabled or disabled per
  "package", and the language can evolve without it being all-or-nothing.

* **Errors that point at your source** (experimental). We also track the
  source to drv correspondence, so when a build fails we can tell you where
  the error came from in your source instead of the `/nix/store`. Sometimes.
  This one still needs more work. See
  [docs/explanation/provenance.md](docs/explanation/provenance.md).

## Benchmarks

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="bench/results/eval-dark.svg">
  <img alt="Eval wall time against Nix 2.34.8 on nixos-desktop and nixos-minimal, warm and cold page cache. iets is 1.7x to 1.9x faster on 1 worker and 3.1x to 3.5x faster on 4 workers. fix 0.3.0 is 1.1x to 1.2x slower on 1 worker and 1.7x to 2.2x faster on 4 workers. Lix 2.95.2 is within 3% of Nix." src="bench/results/eval-light.svg">
</picture>

To regenerate, run `bench/run.nu nixos-desktop nixos-minimal` twice (once with
`COLD=1`), with `LIX` and `FIX` set to the binaries, then `bench/chart.nu
--pins bench/results/env.txt bench/results/*.json`.

## Scope

For now iets is 100% a client and there is no nix-daemon implementation. We are
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
* `fetchGit` copies a local working tree, or a local repo at a `rev` or `ref`,
  by itself. Remote git fetches and git flake inputs run the real `nix`.
* No import-from-derivation (IFD).
* `fetchurl` and `fetchTarball` need a `sha256`. No `fetchClosure` or `hg`
  fetching.
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

Linux only (x86_64 and aarch64). From the flake:

```sh
nix run github:numtide/iets -- version    # run it once
nix profile add github:numtide/iets       # keep it on your PATH
nix build github:numtide/iets#static      # one static binary
```

Or build it from source:

```sh
nix develop -c cargo build --release
./target/release/iets eval --strict -E '1 + 1'
./target/release/iets help
```

On NixOS, import `nixosModules.default` and set `iets.classic.enable = true`.
That puts the classic names (`nix-build`, `nix-shell`, ...) ahead of `nix` in
your path. If direnv is enabled, `use flake` goes through iets too.

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
same licence. `proto/nix_remote.proto` comes from Mic92/nix-grpc-store, under the MIT
licence.
