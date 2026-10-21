# Linux builds on a Mac

You have a Mac and a derivation whose system is `aarch64-linux` or
`x86_64-linux`. You want `iets build` to put the output in the Mac's own
`/nix/store`, with no remote builder, no account and no virtual machine
that you keep running. This page says how that works and why it is sound.
The measurements are from 2026-10-21 on mac02 (Apple M4, 32 GiB, macOS
26.7.1, Nix 2.35.3).

It works, and iets needs no change for it. The Mac's daemon builds the
derivation in a new Linux guest through its `external-builders` hook, and
iets is the daemon's client as for any build:

```sh
iets build --impure --builder daemon -I nixpkgs=$nixpkgs -E '(import <nixpkgs> { system = "aarch64-linux"; }).hello'
```

The tests ran iets against a second daemon on mac02's store, through its
socket with `--builder unix://…`, so the farm's daemon kept its settings.

The helper that runs the guest is
[guestbuild](https://github.com/numtide/guestbuild), a separate program,
because `ARCHITECTURE.md` lists local builds as a non-goal of iets. Its
`docs/design.md` has the full design and its `docs/testing.md` the tests.

## The design

The Mac's daemon owns the store from start to end. For a derivation of a
listed system, it writes a `build.json` with the builder, the environment,
the input closure and the output paths, and runs the helper as a build
user. The helper boots a guest with the store shared read-only, runs the
build in a sandbox like Nix's Linux sandbox, and gets each output back as a
Nix archive. It unpacks the archive at the output path, and the daemon
registers it as after a local build.

The daemon keeps everything a front end would otherwise rebuild:
scheduling, substitution, locks, temporary roots, structured attributes,
reference checks, `--rebuild` and registration.

## The shared writable store

Sharing `/nix/store` read-write into the guest looks like the way to skip
the unpack. It is unsound, and these measurements show it:

- **Case.** The `/nix` volume on mac02 is case-insensitive APFS
  (`diskutil info /nix`: "APFS", not "Case-sensitive APFS"), and Nix runs
  there with `use-case-hack = true`. Through a writable share,
  `echo lower > case-a; echo upper > case-A` in the guest left one file,
  holding `upper`. A Linux output with two names that differ only by case
  is corrupted with no error. The unpack applies Nix's case hack, so the
  output holds `A` and `a~nix~case~hack~1`, and the daemon reverses it when
  it hashes the path.
- **Locks.** While the Mac held `flock` on one file in the share and
  `fcntl` on another, the guest took `flock` on both. SQLite depends on
  these locks, so the guest can't share the daemon's database.
- **Registration.** Only the daemon may make a path valid, and it has no
  operation that registers files someone else put in the store.

So the answer is a copy that's merely fast.

## Store ownership

**Who owns the store during the build.** The Mac's daemon. The guest has
no daemon and no database, and the store share is read-only on both sides.

**What happens when the guest dies.** Nothing reaches the store. The
end-to-end test kills the framework's guest process during a build. The
daemon reports the build as failed, no output is valid, and no process is
left. The daemon holds the temporary roots for the build's inputs, so a
garbage collection during the build can't remove them.

## Cost of a guest

A guest per build is cheap enough that a warm guest buys nothing.

| measurement | value |
|---|---|
| kernel start to `/init` | 0.04 to 0.05 s |
| a trivial build, client to valid path | 0.6 to 0.9 s, with iets evaluating |
| the helper's resident memory | 9 MiB; the guest's memory is in the framework's process |
| a 3 GiB output | 12.7 to 22.3 s |
| GNU Hello from source, with tests, aarch64 | 34.8 to 36.6 s |
| the same, x86_64 under Rosetta | 195.5 to 197.6 s |

## x86_64-linux on Apple silicon

The same aarch64 guest runs x86_64 programs through Rosetta, registered
with `binfmt_misc` and the flags `FP`. `F` opens the interpreter at
registration, so the sandbox doesn't need to see it. `P` keeps `argv[0]`;
without it, Rosetta replaces `argv[0]` with the path, and two of GNU
Hello's tests fail.

GNU Hello for `x86_64-linux`, rebuilt in a guest with `--rebuild`, is byte
for byte the build in the public NixOS cache, which x86_64 hardware made.
One package doesn't prove translation correct. An output built under
Rosetta gets the store path of a native build, so a translation mistake
would look native. Rebuild a sample natively now and then. An Intel Mac
can't run an aarch64 guest, and it gets no Linux builds.

## What a user installs

guestbuild, and two lines of `nix.conf` that only root can write; its
nix-darwin module writes them. Then any client of the daemon builds Linux
derivations. Nothing else is needed:

- No developer account: an ad-hoc signature carries the virtualization
  entitlement.
- No `trusted-users` entry: the daemon registers the outputs of a build it
  ran itself.
- No remote builder, SSH key or running virtual machine.
- Rosetta, for `x86_64-linux` only, which is part of macOS.

That is more than "nothing": two lines of root-owned configuration. They
are what tells the daemon that it may build Linux derivations at all.
