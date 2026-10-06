# Flakes, and how a reference becomes a tree

A flake reference on a command line is text. Evaluation needs bytes. This
page explains the path between the two, why the root flake is read from
your working tree and not from the store, and where iets still has to hand
the job to a real `nix`.

Scope and the exact reference syntax are in
[reference/commands.md](../reference/commands.md); the corners the parser
leaves out are in
[reference/divergences.md](../reference/divergences.md).

## A project with no flake

A native command evaluates purely, whether or not a flake is involved
(`reference/settings.md`). Pure evaluation reads a file only under a store
path the evaluation allowed, so an entry file that is not a flake would have
nothing to read, its own text included. iets therefore gives such a run a
**project**: the nearest directory at or above the entry file that holds a
`flake.nix`, the root of a working tree or an `iets.toml`, and the entry
file's own directory when no ancestor holds one (`src/project.rs`). A pure
evaluation reads below it.

A working tree is the one `source::scm` knows, the same detection the root
flake of a flake reference goes through ("The root flake is your working
tree"), so git is the one SCM that marks a root today and iets gains the
next one in one place.

The project is an **allow list, not a copy of a tree**. Nothing is hashed,
nothing is staged, and a path keeps naming the checkout: `toString ./x` is
`/home/me/repo/x`, and so are the positions in an error. The file set is
not filtered either, so an untracked or ignored file reads like any other —
the thing a flake refuses, and the reason a scratch file in a repository is
not a special case here.

Two consequences, both deliberate, both in
[reference/divergences.md](../reference/divergences.md):

- Under a flake, `toString ./x` is a store path, because the root tree is a
  virtual store copy. Under a project it is the checkout path.
- `"${./x}"` copies that one path, not a slice of a tree copy, so the store
  path it gives differs from the one the same tree gives as a flake.

What the checkout location can really break is a derivation, and that is
where the check sits: a string that carries the text of a non-store path
reaches `derivationStrict` marked, and `impure-path-in-drv` (outside a
project) or `absolute-path-in-drv` (inside one) reports the attribute that
holds it. Both are errors by default (`reference/lints.md`). A read cannot
make a `.drv` depend on where the repository lives; only an attribute can.

A flake reference gets no project. Its root tree is already a virtual copy
whose filter is the tracked file set, and a second, unfiltered root would
let the evaluation read an untracked file by its absolute path. The classic
names get none either: each one answers like the real nix, `--pure-eval`
included (`reference/classic.md`).

## What is in scope, and why

`iets eval --raw DIR#attr.path` prints what
`nix eval --raw --option eval-cache false DIR#attr.path` prints. The
reference goes through the same parser `builtins.parseFlakeRef` uses, which
follows Nix's `parseFlakeRefWithFragment`: a flake id first, then a URL,
then a path.

Only a local path is taken: `.`, `./x`, `/abs`, `~/x`, or a `path:` URL.
Everything else is reported as unsupported, which is what makes the classic
names fall back cleanly to the real `nix` on it:

- a registry reference (`nixpkgs#hello`, and `x#y` even where `./x` exists,
  because that is what Nix's parser does);
- a remote reference (`github:`, `gitlab:`, `git+https:`, `git+file:`, an
  `https://….tar.gz`);
- a `?query` parameter on a path (`?dir=`, `?rev=`, `?narHash=`).

**The lock file must exist and be complete.** Every node reachable from the
root needs a `narHash`, or must be a relative `path:` input, and every
input the `flake.nix` declares needs a lock entry. Otherwise the run says
`flake.lock missing or incomplete, run nix flake lock`. There is no
registry, no in-memory locking and no `nix flake lock` of its own: iets
evaluates what a lock file already decided. As a consequence, a
`flake.nix` input whose URL changed without re-locking is not detected. The
lock wins.

## The root flake is your working tree

Nix's `callFlake` is embedded verbatim and called with the lock file text
and `builtins.fetchTree`. The interesting part is what it is called *on*.

The root's `sourceInfo` is built exactly as Nix's fetcher would build it
for the reference the command line implies. A directory inside the working
tree of a source control system (SCM) is that working tree; anything else
is a plain path.

One interface, `source::scm::Scm`, supplies what a working tree gives to
the evaluation:

- the root of the working tree;
- the tracked files, with the contents and the modes of the working tree;
- the dirty flag;
- the revision and its date, and if the revision has a count;
- the count of the revisions, on request;
- the observations that the tracing cache checks again;
- the words for messages: the name of the SCM and the command that tracks
  a file.

iets has one list of the SCMs that it knows, and detection reads only
that list. git is the one entry. A Jujutsu workspace without `.git` and a
Mercurial working tree are thus plain paths, as they are for Nix.

For a git working tree, the tree is the set of index entries that still
exist in the working tree, with the working tree's contents and modes.
Untracked files, `.git` and submodules are excluded. Four `git` commands
supply it before the evaluation starts: `ls-files --stage -v` and
`status --porcelain -uno` give the file set and the dirty flag, and
`rev-parse HEAD` and `log -1 --format=%ct` give the revision and its date.
A clean tree gets `rev`, `shortRev` and `revCount`; a dirty one gets
`dirtyRev` and `dirtyShortRev`.

An index entry that is not in the working tree makes the tree dirty, and
`git status` does not report every one of them: it never reports an entry
that git checks no more, which is what a sparse checkout and
`update-index --skip-worktree` make. `-v` tags such an entry, and iets
looks for it on disk. Nix reads the same rule out of libgit2's status.

`revCount` is a lazy value. The attribute is in the set when Nix gives
one, so `self ? revCount` and `builtins.attrNames self` are those of Nix.
The fifth command, `rev-list --count`, runs only when the evaluation reads
the value: it takes 1 s on a nixpkgs checkout, and almost no flake reads
it. iets then keeps the count in the tracing store, with the name of the
SCM and the revision as the key, and the next run at that revision starts
no count (`docs/explanation/caching.md`). Nix keeps the count by revision
too (git.cc:getRevCount).

**Deprecated:** `revCount`. The count is expensive, a shallow clone has
none, and each commit changes it. iets plans to remove the attribute. A
read gives the warning `rev-count` (`docs/reference/lints.md`). Use
`lastModifiedDate` and `shortRev` for a version string.

The store path is then computed from the NAR hash of that tree — and
**nothing is written or fetched**. The copy is virtual. The store path is
registered with a note saying "the bytes of this path are in that local
directory, filtered by that file set", and every read under it goes to your
working tree: `import`, `readFile`, `readDir`, `pathExists`, `hashFile`, a
path interpolation, `builtins.path`. The store is never consulted for the
root tree.

Two things follow from that, and both are the point:

- **Your edits are what is evaluated.** No copy step stands between saving
  a file and evaluating it.
- **Untracked files behave as git says.** Reading one fails with Nix's own
  "Path '...' in the repository ... is not tracked by Git" error,
  `pathExists` says `false`, and `readDir` does not list it. The tracked
  file set is the filter, so iets and Nix disagree about nothing here.

The flake's own `flake.nix` is imported under its store path, so positions,
`./x` path literals and `readDir` see exactly what Nix sees.

### A symlink resolves in the store copy

A virtual copy costs one rule. In a real store copy, a relative symlink
resolves inside the copy and an absolute one points out of it. On your disk
the same link resolves in the working tree, where a relative target can
leave the tree and the filter hides nothing from the kernel. A read that
let the kernel resolve the link would therefore see files the store copy
does not hold: a file the SCM does not track, `.git/config`, or any file
the absolute target names.

So a path under a store path this evaluation made is resolved on the text
of the path, component by component, exactly as Nix's
`SourceAccessor::resolveSymlinks` resolves it. Each target is read through
the tree's own filter, and the result is judged by that filter and by pure
evaluation's allow list:

- a link to a file the filter left out is "not tracked by Git", as it is
  for Nix;
- a link out of the copy becomes another absolute path, which a pure
  evaluation refuses with `access to absolute path '...' is forbidden`;
- a tracked link to a tracked directory is followed, and `readFile`,
  `readDir` and `pathExists` see the directory.

`readFile`, `readDir`, `hashFile` and `builtins.path` resolve every
component, `pathExists` every component but the last, and `readFileType`
none, which is what each primop asks `realisePath` for. A path under no
such store path is left to the kernel, which resolves it the same way, so
the walk costs nothing there.

A flake directory that is itself a symlink to a working tree is that
working tree: `git.cc` opens the repository, which resolves the path. An
explicit `path:` reference keeps the link and copies the link object, as
Nix does.

## Getting an input readable

An input is locked, so its store path is known before anything happens. It
still has to become readable. Four steps run in order, and the first that
works wins.

**0. The input store.** If iets has fetched this tree before, it is
already in `~/.cache/iets/inputs`, keyed by the store path the lock
implies. Two `open` calls, no network, no daemon, no store. Why this is safe
without a signature is in [caching.md](caching.md).

**1. Substitution, as one daemon operation.** `QueryValidPaths` with the
substitute flag makes the daemon run `substitutePaths` and answer with the
paths it now has. Nothing is evaluated and no derivation is built. Nix does
the same thing before any fetcher runs, for every input a lock file holds:
it marks such an input `__final` and substitutes its store path first.

An expression cannot reach that path in Nix, because `builtins.fetchTree`
rejects a `__final` input and the internal primop that would do it is absent
from `builtins`. So iets substitutes the tree itself. A local copy beats a
download, which is why this comes before the fetcher. When no daemon
answers, the step is skipped silently.

This step is also what makes iets agree with `nix eval` on a locked `git`
input that a substituter holds but the fetcher cannot reach. Nix's git
fetcher raises `Cannot find Git revision '<rev>' in ref '<ref>'` whenever
the fetch of the rev fails and a stale local ref swallows the failure; the
flake path never gets there, because the tree came from the substituter. The
operation costs 0.002 s, against 0.13 s for the warm `nix build --no-link`
it replaced (0.49 s cold), plus 0.05 s once for the connection.

The connection goes to the *eval* store — `nix.conf`'s `store`, never the
`builder` setting. A remote builder has no say in what `import` reads.

**2. The archive types, fetched by iets.** `github`, `gitlab`, `sourcehut`
and `tarball` are one download and one index, so iets does them itself.
The URLs and headers are the ones Nix builds, the access token comes from
`nix.conf`'s `access-tokens` matched the way Nix matches it, and the four
compressions libarchive would autodetect — gzip, xz, zstd, bzip2 — are
undone.

The download streams. No tree is ever unpacked: the decompressed tar goes
into a spool inside a staged directory, a second pass records where every
member's bytes are, and the NAR is written straight out of that index by
reading each file back at its offset. Peak memory is therefore the queue
plus the index — a few hundred bytes per member and nothing per byte. A full
nixpkgs revision costs 19 MB of index over the evaluator's own 75 MB, where
unpacking a tree cost 307 MB: 13.5 s and 382 MB peak before, 7.7 s and
94 MB after. Once the NAR hash matches, every file goes into the input
store's packs as an object named by the blake3 of its bytes, unless a pack
already holds it, and the spool is deleted.

The unpacking rules are not the tar's rules; they are the rules of the git
tree behind a locked input, because that tree is what the lock's `narHash`
hashes. Directories survive, empty ones included. A regular file keeps one
bit of its mode. A hard link resolves to a second entry with the target's
blob. A member listed twice keeps its last copy. One leading directory is
stripped, but only when it is the only entry at the top. Anything else — a
device node, a `..` in a name, a member under a symlink — is an error.

**3. `git` and `hg`, the last shell-out.** These need a repository, which
iets cannot clone. It runs
`nix eval --raw --impure --expr '(builtins.fetchTree <locked attrs>).outPath'`
and then checks that the path `nix` reports is the one the lock implies, so
a difference in the file set is an error rather than a silent divergence.
When no `nix` other than iets itself is on `PATH`, the error says so and
names the command to run first.

`file` inputs are not in this set, for a specific reason: a `file` input's
store path is the *flat* hash of the bytes under the input's name, which
the lock's `narHash` does not give. iets cannot say where the file belongs
before it has it, so the type stays unsupported.

## What a remote root flake would need

The command line parses `github:NixOS/nixpkgs/<rev>#hello` and refuses it.
The download itself exists. Four parts do not:

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

Items 1 to 3 would make a *locked* `github:`, `gitlab:` or tarball root
work. Item 4 is what most command lines need.

## What this costs

A flake whose only input is `github:NixOS/nixpkgs/e554fab`, five runs with
a warm page cache, timing the evaluation alone:

| `#attr` | from the input store | from `/nix/store` |
|---|---|---|
| `nixpkgs.lib.version` | 0.271 s | 0.238 s |
| `(import nixpkgs {}).hello.drvPath` | 0.573 s | 0.619 s |

Reading a member is not the cost: it is one `pread`, plus an LZ4
decompression when the object was stored compressed, against an `open`, an
`fstat` and a growing buffer for a file under `/nix/store`. The table was
measured when an entry was a spool; with the packs, the heavier evaluation
from three nixpkgs revisions in one store takes the time it took from the
spool, 0.12–0.22 s either way with `--no-cache`. What the input store
charges is opening the entry — 3.2 MB of index for a full nixpkgs, read and
decoded in 24–40 ms, where the spool's 1.7 MB index took 12–32 ms on the
same machine.

Against real Nix, on a NixOS flake with 31 locked inputs and a devShell:

| run | wall |
|---|---|
| `nix eval --raw --option eval-cache false` | 4.28 s |
| `iets eval --raw --no-tracing-cache` | 0.49 s |
| `iets eval --raw`, cache miss | 0.56 s |
| `iets eval --raw`, cache hit | 0.02 s |

The same flake's whole NixOS system configuration: `nix` 7.21 s, iets
1.27 s uncached.

## What the cache records

Everything above is recorded in the tracing cache. The SCM records its own
observations: the `git` queries that read the working tree (`ls-files`,
`status`, `rev-parse HEAD`) become command observations. The date and the
count of the revision are no observations. They follow from the revision,
and a commit cannot change while its name stays: the name is the hash of
its date and of its parents. Thus a validation starts three `git`
processes, not five. The marker `.git/shallow` is an observation, because
it decides if the tree has a count. The marker each ancestor was asked for
during detection is an observation too, so a working tree that appears
above a `path:` flake misses. The working
tree's file set becomes a filtered tree observation with no seen-set,
because the SCM names the file set and an unlisted entry cannot change the
result. Every file read under the root is a file
observation on its *local* path. A remote input that `/nix/store` holds is
one stat; one the input store serves is one observation for the whole entry.

So a commit, an edit of a tracked file, a `git add`, a garbage-collected
input or a swept input-store entry all miss. An untracked file does not.
