# Pure evaluation and local sources

In Nix, a pure evaluation may not read `/etc/hostname`. It may fetch
`file:///etc/hostname`, given a hash, and then read the result. iets closes
that door. This page says what the rule is, why Nix has no such rule, and
what the rule costs.

The rules of pure evaluation are in [Settings](../reference/settings.md).
The project and the root tree are in
[How a flake reference becomes a tree](flakes-and-sources.md). The entry in
[Known divergences](../reference/divergences.md) lists what differs from Nix.

## Two contracts under one name

Nix's `pure-eval` promises **reproducibility**: the result depends only on
the expression and its locked inputs. A fetch with a hash keeps that promise
whatever its source is, because the hash fixes the bytes. So Nix lets a
hashed fetch read anything, a local file included. Nix does not promise that
the evaluation stays inside the flake.

iets promises **confinement** as well: a pure evaluation of a clone reads
the clone and its locked inputs, and nothing else on the machine. That is
why a project's `iets.toml` may not set `pure`, and why a link in the root
tree cannot reach `.git/config`. A CI host that evaluates a change from a
stranger relies on it.

A hash does not give confinement. It says what the bytes are, not where the
evaluation may take them from.

## What a hash lets through

Nix 2.34 answers each of these in pure evaluation. `Z` is the zero hash.

| Expression | What the evaluation gets |
|:---|:---|
| `fetchurl { url = "file:///abs/f"; sha256 = Z; }` | `hash mismatch … got: sha256:…`, the hash of the file |
| the same with that hash, then `readFile` | the bytes of the file |
| `fetchTree { type = "path"; path = "/abs/dir"; narHash = Z; }` | `NAR hash mismatch`, with the hash of the whole tree |
| the same with that hash, then `readFile (tree + "/f")` | any file of the tree |
| `fetchTarball { url = "file:///abs/x.tar.gz"; sha256 = …; }`, and a `tarball` input | the same, for a file that is an archive |
| `fetchGit { url = "file:///abs/repo"; rev = …; }`, and a `git` input | any commit of any repository the user can read |

A hash is the only gate, and the error message gives the hash. Two
evaluations are enough: the first learns the hash, the second reads the
bytes. `builtins.tryEval` does not catch the mismatch, so one evaluation
cannot do both. The download error also tells a missing file from one that
exists.

The lock file is no gate either. `fetchTree` asks a `path` input for a
`narHash`, which the expression can supply itself, and the `flake.lock` of an
untrusted change has the same author as its `flake.nix`.

iets did the same until this rule, because each fetcher was ported from Nix
as Nix has it. It also had two holes of its own. A project is an allow list
checked on the text of a path, and the kernel followed a link below it:
`readFile ./link` read the target of a link that points out of the project,
with no hash at all. And `import` followed a link to a file outside the root
tree of a flake.

Nix knows that a local fetch is a read. Under `restrict-eval`,
`EvalState::checkURI` (eval.cc) sends an absolute path and a `file:` URL
through the same allow list as `readFile`, and `fetch` and `fetchTree` call
it (primops/fetchTree.cc). Nix asks the question only in the mode that
confines, and `pure-eval` is not that mode. iets has no `restrict-eval`, and
its pure evaluation claims the confinement, so the check is part of it.

## The rule

> In pure evaluation, a fetch from the local file system is a read. It may
> not leave the root, and it goes through the same accessor as `readFile`.

**The root** is the one a run already has. For a flake it is the tree of the
flake on the command line: the working tree of its source control system,
else the directory of its `flake.nix`. For any other entry it is the
project: the working tree, else the nearest `flake.nix` directory, else the
outermost `iets.toml` one, else the entry's own directory
([How a flake reference becomes a tree](flakes-and-sources.md)).

**The sources** the rule covers:

- a `file:` URL of `fetchurl`, of `fetchTarball` and of a `tarball` input;
- the `path` of a `path` input, in `fetchTree` and in `getFlake`;
- the repository of a local `git` or `hg` input, and of `fetchGit`.

**The accessor** is `store_hooks::local_source`. It does what a read does:

1. A path at or below the root flake's directory becomes the path under the
   root tree's store copy. So the tree's filter applies: a file the source
   control system does not track is `not tracked by Git`, as it is for
   `readFile`.
2. Every link on the way is resolved on the text of the path, one component
   after the other, as Nix's `SourceAccessor::resolveSymlinks` does. The
   kernel is not asked to follow one.
3. The allow list judges the path the links end at. A path outside the root
   and outside every store path the evaluation holds is
   `access to absolute path '…' is forbidden in pure evaluation mode`.
4. The bytes come from what the path resolved to. `http::get` does not open
   a `file:` URL for a pure evaluation.

A repository is the one source no copy can stand for, because `git` opens it
in place. It must be a directory on disk that the root holds: under a flake
the flake's own repository or a directory it tracks, else a directory of the
project.

The rule is about what an expression may ask for. `iets prefetch URL` reads
the URL its user typed, and `nix flake prefetch-inputs` takes `--impure` for
a lock whose inputs lie elsewhere on the machine.

The check comes before `/nix/store` and the input store are asked. So what
is allowed does not change with what a cache holds, and a hash the
evaluation knows does not buy the bytes of a file it may not read.

A plain read in a project is judged the same way. `readFile`, `readDir`,
`hashFile`, `builtins.path`, `import` and the copy of `"${./x}"` resolve the
links below the project on the text of the path, or let the kernel prove
that the path stays inside it (`store_hooks::admits`), and a link that leaves
the project is refused. `"${./link}"` still copies the link itself, as Nix
does.

The mismatch messages keep the hash. With the source confined, the message
names the hash of a file the evaluation may read anyway, and `fix-hashes`
and the `set sha256 to …` hint need it.

## What it costs

**Derivations.** Nothing. The rule turns some evaluations that succeed into
errors. An evaluation that still succeeds gives the same `.drv` paths.

**nixpkgs.** Nothing. At the pinned revision (02c087f3), `builtins.fetchurl`
is called at 8 places in 6 files. Six have an `https` URL and two are update
scripts that pass a URL through. The `fetchTarball` calls name `https`
URLs. `fetchGit` is called with a URL out of the caller's own lock file
(`import-cargo-lock.nix`, `import-npm-lock`), and by `lib.fileset` on a
working tree with no `rev`, which a pure evaluation refuses already. Nothing
calls `fetchTree`. The 184 lines that hold `file:/` are service settings,
test scripts and documentation: none is the argument of a fetcher that runs
during evaluation.

**`<nix/fetchurl.nix>`.** Nothing. `src/corepkgs/fetchurl.nix` is a
derivation with the builder `builtin:fetchurl`. The daemon runs it in the
build sandbox, and the evaluator reads no file for it.

**Local inputs outside the root.** This is the cost. A flake that locks
`path:/home/me/lib`, `git+file:///home/me/lib` or
`file:///home/me/x.tar.gz` evaluates purely in Nix, and people do this to
work on two checkouts at once. iets refuses it, and `--impure` is the way to
say that the machine's files are fair game. A relative `path:` input is not
affected: it is read below its parent's tree. An input that is already in
`/nix/store` or in the input store is not exempt, because the check comes
first.

**A `path` input inside a working tree.** It is read through the root
tree's filter, so an untracked file is not part of it, and its `narHash` is
that of the tracked files. Nix copies the directory whole.

## What was not done

**Refusing the `file:` scheme.** That is a rule about URLs, and the hole is
not about schemes: a `path` input and a local repository have none to
refuse. It would also refuse a `file:` URL below the project, which a
`readFile` may read.

**Hiding the hash in the mismatch message.** A caller who can guess the
content confirms it by success or failure, the existence of a file still
shows, and a hash known by other means reads the file as before.

**Leaving it to the sandbox.** The sandbox must stay whatever iets does: a
hashed fetch sends its request before it checks the hash, so a pure
evaluation with a network can send out what it read as the path of a URL.
The sandbox is the second boundary, not the only one.

## Limits

- The walk and the read are two steps. A process that swaps a directory for
  a link between them can still lead the read elsewhere. The author of the
  flake cannot do that from inside an evaluation.
- The root is found from the directory the command names, and the walk does
  not resolve the links above it. A root that is itself reached through a
  link is the place the user named.
- A classic name follows the rule too. A boundary that held for
  `iets eval-jobs` and not for `nix-eval-jobs` would protect nobody who had
  not read this page.
