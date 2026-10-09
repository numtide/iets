# Hashes in a lock file

A fetcher call needs a hash. Today you write it inside the expression, and
to learn it you either run `iets prefetch` or put in a fake hash and copy
the right one out of the error. `iets.lock` moves the hashes out of your
expressions into one file at the top of the repository:

```nix
let lock = import ./nix/lock.nix ./iets.lock; in
lock.fetchTarball "https://github.com/numtide/treefmt/archive/v2.0.0.tar.gz"
```

```json
{
  "unpack": {
    "https://github.com/numtide/treefmt/archive/v2.0.0.tar.gz": "sha256-Ckvpb2at7lg7AB0XNtIxeo8lWXX+04MVHHKoUs876dg="
  },
  "version": 1
}
```

The steps are in [how-to/lock-hashes.md](../how-to/lock-hashes.md). This
page says why it works the way it does.

## A Nix file, not a builtin

There were two ways to build this.

1. **The fetchers read `iets.lock` themselves.** You would write
   `builtins.fetchTarball url` and nothing else. But that expression means
   something different on Nix: there, it is an unhashed fetch, or an
   error in pure mode. A repository that relied on it could only be
   evaluated by iets.
2. **A small Nix file reads `iets.lock`** and passes the hash to the
   normal fetcher. It uses only `readFile`, `fromJSON` and the fetchers
   you already have.

iets uses the second. The question that decides it is: who can evaluate
your repository? With the Nix file, the answer is iets and Nix, with the
same store paths. With a builtin, only iets. That is a product decision
for every user of the repository, and it buys one `import` line.

A builtin would also reach fetcher calls in code you do not own. It would
change what a third-party expression means without its authors knowing.

Only filling the file needs iets: `iets lock` writes it. Someone on Nix
can still read the file, and can add an entry by hand.

## What you trust

The first fetch of a URL is not checked against anything. `iets lock`
takes what the URL serves, hashes it, and records the hash. From then on,
every fetch must match. This is trust on first use.

Compare it with what you have today:

- **It is weaker than a hash you checked yourself.** If someone wrote a
  hash after they compared the content with a release signature, that
  hash says more than a lock entry.
- **It is stronger than an unhashed `fetchTarball`.** That is trust on
  first use too, but nothing records what you got. After `tarball-ttl`
  (one hour by default), Nix fetches the URL again if it changed, and
  takes whatever it gets.

So the lock is a real improvement over unhashed fetches, and a real step
down from hashes that a person checked. Before you commit a new entry,
check that you trust the URL. The entry goes into code review like any
other line.

## Evaluation reads, `iets lock` writes

An evaluation never fetches something it cannot check, and never changes
a file in your repository. So:

- **A missing entry is an error.** The error names the command that adds
  it:

  ```
  error: iets.lock has no unpack hash for https://example.org/src.tar.gz.
  Check that you trust the URL, then run this in the directory of iets.lock:
    iets lock --unpack https://example.org/src.tar.gz
  ```

- **Only `iets lock` writes the file.** You run it on purpose, the way you
  run `nix flake update` or `npins add`.

If evaluation filled the file, the same commit could evaluate to
different results on two machines. And a file under version control would
change behind your back.

## The key is a URL and a mode

Each entry is keyed by the URL and by how the content becomes a store
object:

| Mode | The hash of | Fetchers |
|:---|:---|:---|
| `file` | the bytes the URL serves | `fetchurl` |
| `unpack` | the NAR of the unpacked archive | `fetchTarball`, `fetchzip`, `fetchFromGitHub` |

The mode is all the key needs. A fetcher name would add nothing: two
fetchers with the same mode give the same hash, and share the entry.
`builtins.fetchTarball` and `fetchzip` on the same URL give the same store
path. The `name` argument changes the store path but not the hash, so it
is not part of the key either.

Some arguments change the output in ways a URL and a mode cannot express:
`postFetch`, `stripRoot = false`, git submodules, `executable`. The lock
does not cover them. It holds only hashes that `iets lock` can compute
from the URL alone. For those calls, write the hash in the expression, as
before. A git checkout is pinned by its `rev` already.

Do not lock a tarball that holds a single file with `unpack`. Nix hashes
that file, and `iets lock` hashes the directory that holds it
([reference/commands.md](../reference/commands.md), `prefetch`). The
entry would not match on Nix.

## When the remote changes

Some URLs change what they serve: a moving tag, `master.tar.gz`, a
redirect. With a lock, that becomes a hash mismatch. That is what you
want, but the message must say what happened. The remote changed. Your
hash was right when you recorded it.

`iets lock` says so when you run it again:

```
error: the remote changed: iets.lock has sha256-t1Kr… for https://…, and it serves sha256-oyBk… now.
Check what changed, then run:
  iets lock --update https://…
```

During an evaluation, the fetcher reports the mismatch. `lock.fetchTarball`
adds a line to that error that says the remote changed, and names the same
`--update` command. iets and Nix both print it.

A `lock.hash` that you pass to a nixpkgs fetcher fails in the build, as a
fixed-output hash mismatch. The helper cannot add to that message. The
`fix-hashes` setting of `iets build` does not edit `iets.lock`, because it
never edits a hash that came from a data file. Run `iets lock --update`.

`--update` is a separate flag on purpose. Taking new content for a URL
you already trust is a new trust decision, so you have to ask for it.

## A lock is not a cache

A record of what a run fetched, used to start the same fetches early next
time, looks like the same table. It is not built yet. If it is built, it
must not share this file.

| | the lock | a fetch record |
|:---|:---|:---|
| a wrong entry costs | correctness | bandwidth |
| entries expire | never | yes |
| reviewed | yes, in git | no, not in git |

Such a record may read the lock for hints about what to fetch. The lock
must never read the record. An entry nobody reviewed must not become an
entry you trust.

## Prior art

- `flake.lock` and `nix flake update`: the lock for flake inputs, written
  only on request
  ([manual](https://nix.dev/manual/nix/2.34/command-ref/new-cli/nix3-flake-update)).
- npins ([github.com/andir/npins](https://github.com/andir/npins)) and niv
  ([github.com/nmattia/niv](https://github.com/nmattia/niv)): a JSON file
  of sources and a Nix file that reads it, which works on any Nix. The
  lock is the same shape, keyed by URL instead of by a name you choose.
- `tarball-ttl`, the refetch interval of an unhashed fetch
  ([manual](https://nix.dev/manual/nix/2.34/command-ref/conf-file#conf-tarball-ttl)).
