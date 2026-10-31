# How a flake reference becomes a tree

You type `.#hello`, and somewhere between that text and the evaluation there
have to be bytes to read. This page follows that path: why the root flake is
read from your working tree, how each locked input becomes readable, and
where iets hands the job to a real `nix`.

The reference syntax is in [Commands](../reference/commands.md). The corners
the parser leaves out are in
[Known divergences from Nix 2.34](../reference/divergences.md).

## Projects

A native command evaluates purely, flake or not
([Settings](../reference/settings.md)). Pure evaluation reads a file only
under a store path the evaluation allowed, so an entry file that isn't a
flake would have nothing to read, not even its own text. iets gives such a
run a **project**. It finds the project by walking up from the entry file,
and a pure evaluation can read anything below it.

The markers rank like this:

| marker | weight | why |
|:---|:---|:---|
| a working tree | the nearest one wins over everything below it | the repository is the unit the code lives in, so a subdirectory marker must not stop a sibling directory from being read |
| `flake.nix` | the nearest one, when there is no working tree | it claims a boundary: Nix makes that directory a tree. An unpacked tarball or a store copy of a checkout lands here |
| `iets.toml` | the outermost one, and only when neither of the preceding rows answers | it claims nothing. It sets lint levels for what is below it, a project may hold several, and a deep one must not shrink the root |

With no marker at all, the entry file's own directory is the project, so a
one-off expression in a scratch directory still evaluates.

A working tree is found the same way as for the root flake of a flake
reference ("The root flake"), and git is the only SCM
that marks a root.

The project is an **allow list**. Nothing is hashed
and nothing is staged. A path keeps naming your checkout: `toString ./x` is
`/home/me/repo/x`, and so are the positions in an error. The file set isn't
filtered, so an untracked or ignored file reads like any other. These
consequences are deliberate
([Known divergences from Nix 2.34](../reference/divergences.md)):

- Under a flake, `toString ./x` is a store path, because the root tree is a
  virtual store copy. Under a project it is the checkout path.
- `"${./x}"` copies that one path, not a slice of a tree copy, so the store
  path it gives differs from the one the same tree gives as a flake.

Only a derivation attribute can make a `.drv` depend on where your
repository lives, so that is where the check sits. A string that holds the
text of a path outside the store reaches `derivationStrict` with a mark on
it, and `impure-path-in-drv` (outside a project) or `absolute-path-in-drv`
(inside one) reports the attribute that holds it. Both are errors by default
([Lints](../reference/lints.md)).

A flake reference gets no project. Its root tree is already a virtual copy,
filtered by the tracked file set, and a second root with no filter would let
the evaluation read an untracked file by its absolute path. Classic mode
get none either: each one answers like the real nix, `--pure-eval` included
([Classic mode](../reference/classic.md)).

## Accepted references

`iets eval --raw DIR#attr.path` prints what
`nix eval --raw --option eval-cache false DIR#attr.path` prints. The
reference goes through the parser `builtins.parseFlakeRef` uses, which
follows Nix's `parseFlakeRefWithFragment`: a flake id first, then a URL,
then a path.

iets takes only a local path: `.`, `./x`, `/abs`, `~/x`, or a `path:` URL.
Anything else is unsupported, and classic mode hands it to the real
`nix`:

- a registry reference (`nixpkgs#hello`, and `x#y` even where `./x` exists,
  because that is what Nix's parser does);
- a remote reference (`github:`, `gitlab:`, `git+https:`, `git+file:`, an
  `https://….tar.gz`);
- a `?query` parameter on a path (`?dir=`, `?rev=`, `?narHash=`).

**The lock file must be complete.** Every node reachable from the
root needs a `narHash`, or must be a relative `path:` input, and every input
the `flake.nix` declares needs a lock entry. Otherwise the run says
`flake.lock missing or incomplete, run nix flake lock`. A flake that declares
no inputs needs no lock file, as in Nix. iets has no registry,
no locking in memory and no `nix flake lock` of its own, so if you change an
input's URL in `flake.nix` and don't lock again, iets doesn't notice. The
lock wins.

## The root flake

Nix's `callFlake` is embedded verbatim and called with the lock file text
and `builtins.fetchTree`. The root's `sourceInfo` is built as Nix's fetcher
would build it for the reference the command line implies. A directory
inside the working tree of a source control system (SCM) is that working
tree, and anything else is a plain path. A Jujutsu workspace without `.git`
and a Mercurial working tree are plain paths, as they are for Nix.

An interface (`source::scm::Scm`) supplies what a working tree gives to the
evaluation: the tracked files with their contents and modes, the dirty flag,
the revision and its date, the count on request, and the observations the
tracing cache checks again. git is its only implementation.

For a git working tree, the tree is the set of index entries that still
exist in the working tree, with the working tree's contents and modes.
Untracked files, `.git` and submodules are excluded. `git` supplies the
tree before the evaluation starts: `ls-files --stage -v` and
`status --porcelain -uno` give the file set and the dirty flag, and
`rev-parse HEAD` and `log -1 --format=%ct` give the revision and its date.
A clean tree gets `rev`, `shortRev` and `revCount`; a dirty one gets
`dirtyRev` and `dirtyShortRev`.

An index entry that is not in the working tree makes the tree dirty. `git
status` doesn't report an entry that git no longer checks (a sparse checkout,
`update-index --skip-worktree`), so iets looks for it on disk, as Nix does.

`revCount` is a lazy value. The attribute is in the set when Nix gives
one, so `self ? revCount` and `builtins.attrNames self` are those of Nix.
`rev-list --count` runs only when the evaluation reads the value, and iets
keeps the count by revision ([How caching works](caching.md)). A read gives
the `rev-count` warning ([Lints](../reference/lints.md)): use
`lastModifiedDate` and `shortRev` for a version string.

iets computes the store path from the NAR hash of that tree, and **nothing
is written or fetched**. The copy is virtual: the store path gets a note that
says "the bytes of this path are in that local directory, filtered by that
file set". Every read under it (`import`, `readFile`, `readDir`,
`pathExists`, `hashFile`, a path interpolation, `builtins.path`) goes to your
working tree, and the store is never consulted for the root tree.



In practice:


- **Your edits are what is evaluated.** No copy step stands between saving
  a file and evaluating it.
- **Untracked files behave as git says.** Reading one fails with Nix's own
  "Path '...' in the repository ... is not tracked by Git" error,
  `pathExists` says `false`, and `readDir` does not list it. The tracked
  file set is the filter, so iets and Nix disagree about nothing here.

The flake's own `flake.nix` is imported under its store path, so positions,
`./x` path literals and `readDir` see exactly what Nix sees.

### Symlinks

In a real store copy, a relative symlink resolves inside the copy, and an
absolute one points out of it. On your disk, the same link resolves in the
working tree, where a relative target can leave the tree and the filter
hides nothing from the kernel. A read could then see files the store copy
does not hold: a file the SCM does not track, `.git/config`, or any file the
absolute target names.

So a path under a store path this evaluation made is resolved on the text
of the path, component by component, as Nix's
`SourceAccessor::resolveSymlinks` does. Each target is read through the
tree's own filter, and the result is judged by that filter and by pure
evaluation's allow list:

- a link to a file the filter left out is "not tracked by Git", as it is
  for Nix;
- a link out of the copy becomes another absolute path, which a pure
  evaluation refuses with `access to absolute path '...' is forbidden`;
- a tracked link to a tracked directory is followed, and `readFile`,
  `readDir` and `pathExists` see the directory.

`readFile`, `readDir`, `hashFile` and `builtins.path` resolve every
component, `pathExists` every component but the last, and `readFileType`
none, which is what each primop asks `realisePath` for. In a pure
evaluation a path in the project is resolved the same way, and a link that
leaves the project is refused
([Pure evaluation and local sources](pure-eval-and-local-sources.md)). Any
other path is left to the kernel.

A flake directory that is itself a symlink to a working tree is that
working tree: `git.cc` opens the repository, which resolves the path. An
explicit `path:` reference keeps the link and copies the link object, as
Nix does.

## Locked inputs

An input is locked, so iets knows its store path before anything happens.
It tries these sources in order, and the first one that works wins. An
input on this machine, a `file:` URL, a `path` or a local `git` or `hg`
repository, first passes the allow list of pure evaluation, so that no
cache answers for a file the evaluation may not read
([Pure evaluation and local sources](pure-eval-and-local-sources.md)).

**`/nix/store`, if the store has the path as valid.** A file at the path is
not enough: an interrupted build or copy can leave one that the store never
registered. iets asks a local daemon, with one query for all the inputs of
the lock. With no local daemon, iets hashes the path and compares it with
the `narHash`. `builtins.fetchurl`, `builtins.fetchTarball` and
`builtins.path` with a `sha256` do the same check.

**The input store.** If iets has fetched this tree before, it's already in
`~/.cache/iets/inputs`, keyed by the store path the lock implies: no
network, no daemon, no store. [How caching works](caching.md) says why this
is safe without a signature.

**Substitution, as one daemon operation.** `QueryValidPaths` with the
substitute flag makes the daemon run `substitutePaths` and answer with the
paths it has. Nothing is evaluated and no derivation is built. Nix does
the same thing before any fetcher runs, for every input a lock file holds:
it marks such an input `__final` and substitutes its store path first.

An expression cannot reach that path in Nix, because `builtins.fetchTree`
rejects a `__final` input and the internal primop that would do it is absent
from `builtins`. So iets substitutes the tree itself, before the fetcher,
because a local copy beats a download. When no daemon answers, the step is
skipped silently. It also makes iets agree with `nix eval` on a locked `git`
input that a substituter holds but the fetcher cannot reach.

The connection goes to the eval store: `nix.conf`'s `store`, never the
`builder` setting.

**The archive types, fetched by iets.** `github`, `gitlab`, `sourcehut`
and `tarball` are one download and one index, so iets does them itself.
The URLs and headers are the ones Nix builds. The access token comes from
`nix.conf`'s `access-tokens`, matched the way Nix matches it. iets undoes
the four compressions libarchive would detect: gzip, xz, zstd and bzip2.

The download streams, and no tree is ever unpacked: iets indexes the tar in
a spool, then writes the NAR straight from that index. Once the NAR hash
matches, every file goes into the input store's packs and the spool is
deleted.

The unpacking rules are those of the git tree behind a locked input, because
the lock's `narHash` is the hash of that tree. A device, a socket and a fifo
are errors; a `..` in a name and a member below a symlink are not. The full
list is in
[Flakes](../reference/flakes.md#what-the-tree-keeps).

**`git` and `hg`, the last shell-out.** These need a repository, which
iets cannot clone. It runs
`nix eval --raw --impure --expr '(builtins.fetchTree <locked attrs>).outPath'`
and then checks that the path `nix` reports is the one the lock implies.
So a difference in the file set is an error.
When no `nix` other than iets itself is on `PATH`, the error says so and
names the command to run first.

`file` inputs are not supported. A `file` input's store path is the *flat*
hash of the bytes under the input's name, and the lock's `narHash` does not
give that, so iets cannot say where the file belongs before it has it.

## Remote root flakes

The command line parses `github:NixOS/nixpkgs/<rev>#hello` and refuses it.
The download works, but these are missing:

1. **A root has no lock entry**, so no `narHash` names and checks the tree
   before the download. The fetch would have to stage the archive, hash it,
   and only then name the entry by the hash it got. That inverts the
   property the input store is built on.
2. **`lastModified` of an archive comes from the archive**, and the spool
   index keeps no modification times.
3. **A flake reference is assumed to be a local directory** in three
   places: the lock file is read from disk, the root tree is copied from a
   local directory, and both the cache key and the `iets.toml` walk read
   the directory. A root in the input store needs a third kind of source.
4. **An unlocked reference must first resolve to a revision**, which needs
   the forge API per scheme, `tarball-ttl`, and the flake registries. A
   `git` reference needs a git fetcher, which iets has not got.

The first three would make a locked `github:`, `gitlab:` or tarball root
work. The last is what most command lines need, and it's the hard one.

## Input store cost

A flake whose only input is `github:NixOS/nixpkgs/e554fab`, five runs with
a warm page cache, timing the evaluation alone:

| `#attr` | from the input store | from `/nix/store` |
|---|---|---|
| `nixpkgs.lib.version` | 0.271 s | 0.238 s |
| `(import nixpkgs {}).hello.drvPath` | 0.573 s | 0.619 s |

Reading a member costs one `pread`, plus an LZ4 decompression when the
object was stored compressed. Opening the entry costs 3.2 MB of index for a
full nixpkgs, read and decoded in 24–40 ms.

Against real Nix, on a NixOS flake with 31 locked inputs and a devShell:

| run | wall |
|---|---|
| `nix eval --raw --option eval-cache false` | 4.28 s |
| `iets eval --raw --no-tracing-cache` | 0.49 s |
| `iets eval --raw`, cache miss | 0.56 s |
| `iets eval --raw`, cache hit | 0.02 s |

The same flake's whole NixOS system configuration: `nix` 7.21 s, iets
1.27 s uncached.
