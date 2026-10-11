# Why iets says `--builder`

You know Nix, so you type `iets build --store ssh-ng://host .#pkg`. iets
answers `unknown flag '--store'` and `did you mean '--builder'?`. The native
commands use their own names on purpose, and the classic names keep Nix's.
This page says why. The flags themselves are in
[Builders and the wire](../reference/builders.md).

## Three questions

A build answers three questions: where the work runs, which store registers
the paths, and where the bytes end up.

| question | `nix build` | `iets build` |
|---|---|---|
| where the work runs | the store `--store` names, which may pass it on to the machines in `builders` | `--builder`, else the `builder` setting |
| which store registers the paths | the store `--store` names; the derivations and sources go to `--eval-store` first, which is the same store unless you set it | the builder; the local daemon too with `-o` |
| where the bytes end up | always a store | the builder; the local store with `-o`; a plain directory with `--fetch-to` |

Nix answers the first two questions with one URL and has no flag for the
third. iets has one flag per question.

## Where the models disagree

`--builder` looks like `--store` with another name, and for
`--builder ssh-ng://host` without `-o` it behaves like
`nix build --store ssh-ng://host --eval-store auto`: the outputs stay on the
host. The two still differ in ways that would bite you if one were an alias
of the other.

- **Writes during evaluation.** With `--store ssh-ng://host` alone, Nix
  writes every derivation and source the evaluation adds into the remote
  store while it evaluates, because the eval store defaults to the store
  (`EvalCommand::getEvalStore` in `src/libcmd/command.cc`). With
  `--eval-store auto` they go to the local store, and `RemoteStore::buildPaths`
  copies the derivations over before it builds (`copyDrvsFromEvalStore` in
  `src/libstore/remote-store.cc`). iets evaluates in memory, writes nothing
  during evaluation, and sends only the closure the builder lacks when the
  build starts. That is neither of Nix's answers.
- **Out-links.** Nix makes `result` only when the store is a local file
  system store (`MixOutLinkBase::createOutLinksMaybe` in
  `src/libcmd/command.cc`). So `nix build --store ssh-ng://host -o result`
  builds and makes no link, with no message.
  `iets build --builder ssh-ng://host -o result` brings the closure into the
  local store and links it there, as Nix does with `--builders`.
- **Store kinds.** `--store` takes any store Nix knows: `local?root=DIR`, a
  plain path, `file://`, `s3://`, `dummy://`. `--builder` takes `daemon`,
  `unix://`, `ssh-ng://` and `grpc://`, the transports iets speaks. An alias
  would have to refuse most of what the name promises.
- **Several places.** The `builder` setting can list places by system and
  feature, which is what Nix's `builders` does. iets picks the place in the
  client, and Nix's build hook picks it in the daemon. So `--builder` is
  half `--store` and half `--builders`.

## What Nix can't say

`--fetch-to DIR` restores each output into `DIR/<hash>-<name>` as plain
files. It registers nothing, so you don't need to be a trusted user, and you
don't need a local store at all. Nix keeps an output in a store, always. The
nearest Nix gets is two commands per path after the build:

```sh
nix store dump-path --store ssh-ng://host /nix/store/…-pkg | nix-store --restore out/…-pkg
```

(`src/nix/dump-path.cc`, and `opRestore` in `src/nix/nix-store/nix-store.cc`).
`--store local?root=DIR` is not the same thing: it is a store with its own
database, and its path is `DIR/nix/store/…`.

With one flag per question, you can build on a farm, register in your local
store and extract into a third place in one run:

```sh
iets build --builder grpc://farm:50051 -o result --fetch-to out .#pkg
```

Calling `DIR` a store would be wrong, and a `--store` alias has no way to
say "extract here".

## The decision

- **The native commands keep `--builder` and `--fetch-to`.** `--store` and
  `--option store` there are usage errors that name `--builder`. An alias
  would promise Nix's meaning and give another one on each point in "Where
  the models disagree". An error costs you one retry. A near-synonym costs
  you a build on the wrong host or a `result` that is missing.
- **The classic names keep `--store`**, with Nix's meaning as far as iets
  can give it (`docs/reference/classic.md`). That's where iets promises to
  be a drop-in.
- **`--fetch-to` and `-o` combine.** One extracts the bytes, and the other
  registers the paths in the local store.

`docs/reference/divergences.md` has the short form for Nix users.
