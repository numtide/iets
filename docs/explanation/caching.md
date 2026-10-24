# How caching works

iets writes three things under `~/.cache/iets`. The compiled IR and the
tracing runs are caches: they hold the result of work iets can do again.
Fetched inputs aren't a cache. They're where a fetched flake input *lives*.
All three work the same way, and the input store is safe in your home
directory with no daemon and no signature.

| store | default root | entry | keyed on |
|---|---|---|---|
| compiled IR | `~/.cache/iets/ir` | one file | build id, path, system, store directory, `pure-eval`, lint levels, source |
| tracing runs | `~/.cache/iets/trace` | one file in a directory | the whole command; for the count of a revision, the SCM and the revision |
| fetched inputs | `~/.cache/iets/inputs` | one directory | the store path the lock implies |

`--no-tracing-cache` and `--no-ir-cache` turn off one cache each, and
`--no-cache`, `IETS_CACHE=0` and the `cache` setting turn off both. Nothing
turns off the input store, because a run needs the input's bytes.
`--cache-dir DIR` moves the three stores and the roots directory.

`~/.cache/iets/roots` holds the GC roots iets owns
([The roots store](#the-roots-store-gc-roots-iets-owns)). The design
comes from Mic92's nix-tarmac.

## Store safety

**Stage and rename.** An entry is one file. iets writes it in full under a
temporary name, then renames it into place. A rename is atomic, so one
rename publishes the whole entry. A reader sees either the whole old entry
or the whole new one, never a torn file and never two generations mixed.

**Readers never lock.** A reader opens the entry once and reads every part
it needs through that one descriptor, so the bytes it uses are the
generation it validated, whatever a sibling publishes in the meantime. The
old file stays alive for as long as the descriptor does.

**A writer holds the store's `flock` for exactly one entry, and gives up
after 50 ms.** A skipped write is a miss on a later run, never a
stall and never an error. The lock only stops two writers of one key from
crossing inside one temporary file. A killed writer leaves a temporary file
that no reader looks at and that the sweep removes after a day; the entry it
was replacing stays whole.

Nothing is fsynced, so a power loss can leave the name pointing at a file
whose bytes were never written. Those bytes are read like any other bad
bytes, and the entry goes. Atomicity is per entry, and no reader joins two
entries.

**Poison and wipe; never repair.** An entry that does not decode is
deleted and the run continues as a miss. Only bytes a reader read, or a
name that is not a regular file, are poison. A regular file that does not
open is a miss and stays: Windows refuses an open while a sibling renames a
new entry over the name. The wipe of a tracing entry takes the writer lock
and removes the entry only if it is still the generation the reader
rejected, so a good entry that a sibling published meanwhile stays. A store whose version file is of
another format, or whose garbage-collection stamp cannot be parsed, or
whose lock is not a file, is emptied and starts again.

**An entry carries a checksum over its own key.** Every entry of the IR
cache and of the tracing cache holds 16 bytes of blake3 over the key it
answers and the whole of its content. A reader checks it before it decodes
anything. A changed byte, a truncated entry and an entry copied to another
key are all a mismatch, and a mismatch is a miss that poisons the entry.

A reader opens an entry with `O_NOFOLLOW` and `O_NONBLOCK`, accepts only a
regular file, and refuses a length over 512 MiB before it allocates
anything. A name replaced by a symbolic link, a FIFO or a huge sparse file is a miss.

**Nothing in an entry names a program.** An observation of a working tree
holds which query of which SCM was asked, and the validator builds the
command line from its own table, so an entry cannot start a program of its
own choosing. `nix fmt` is the one command that runs what an entry
remembered, and it runs it only when the entry names a file inside a store
path of the store directory.

**Nothing in an entry is plain text that doesn't have to be.** An environment
variable is stored as a hash keyed with the cache's own secret. Directories
are 0700 and files 0600 whatever the umask.

All of this holds for any bytes, not only bytes iets wrote: a crafted entry must be a miss. The IR decoder checks every constant, index
and invariant the evaluator would otherwise trust.

**iets removes only what iets put there.** A wipe touches only the names
that store's own layout makes: the version file, the lock, the stamp, the
temporary files, the entries and, for the IR cache, its shard directories.
Anything else in the directory survives, and the directory itself is never
removed. The three stores are subdirectories (`trace/`, `ir/`, `inputs/`)
of the cache root, so `--cache-dir DIR` never makes `DIR` itself a removal
target. A store subdirectory that exists, holds something, and has no name
from that layout is not iets's: iets names it in an error and runs without
that cache.

The sweep removes entries not read for 30 days. It runs on a write path, at
most once a day per store, and never on the read path. The walk runs
unlocked, so a sweep of a large store never holds a sibling's write back.

A warm hit writes almost nothing. The tracing cache rewrites its read set
only when validation refreshed a stat field, and touches the entry's mtime
only when it is over a day old. So a hot cache on a busy machine does no
disk writes.

## The IR cache: compile each file once

Parsing and compiling the 3,500 files of the nixos-server workload was
about 28 % of a one-worker run. The IR cache writes each imported file's
compiled IR after its first compile and loads it on later runs. All the
files load in 229 ms against 696 ms to compile, and `hackage-packages.nix`
alone loads in 71-77 ms against 190-218 ms.

Only `import` uses it. `scopedImport`, `-E` and a file with a `~/` path
literal compile from source, and files under 1 KiB aren't cached, because
the load costs what the compile costs.

The key holds the build id, so a new binary never reads an old entry. It
also holds everything the compiler folds out of the evaluator's
configuration, because a folded constant is stored by content: the system,
the store directory, the compile-time lint levels, and `pure-eval`. Without
`pure-eval` in the key, an impure run's entry would answer a pure run, and
`builtins.currentSystem or "fallback"` would give the system. Every other
constant is stored by name and looked up in the live tables on load, so the
search path follows the run.

`eval-jobs` uses no other cache, so its effect is easy to see. On
`nixos/release-small.nix` with four workers on a 16-core EPYC:

| tool | run | wall | max RSS |
|---|---|---|---|
| `iets eval-jobs` | cold | 0.29 s | 85 MiB |
| `iets eval-jobs` | warm | 0.26 s | 85 MiB |
| `nix-eval-jobs` | cold | 0.68 s | 113 MiB |
| `nix-eval-jobs` | warm | 0.66 s | 113 MiB |

nix-eval-jobs keeps no on-disk evaluation cache for a file argument, so its
two rows differ only in page cache.

A second nixpkgs costs little on disk. Evaluating the nixos-server
workload at a nixpkgs two releases older than the pinned one added one
tracing entry (0.9 MiB) and 56 IR entries (0.5 MiB).

## The tracing cache: skip evaluation when nothing changed

`iets eval` and `iets build` remember what a run printed, together with
every observation of the outside world the run made. A later run with the
same command validates those observations. If nothing changed, it replays
the result and skips evaluation entirely. `print-dev-env`, `search` and the
`nix` subcommands and `nix-build` iets serves in classic mode do the
same, each with a key of its own ([Classic mode](../reference/classic.md)
says what each entry holds).

Every primop that looks at the world records one observation.

| observation | recorded by | validated by |
|---|---|---|
| a file's size, mtime, ctime, device, inode and sha256 | `import`, `readFile`, `hashFile`, every `.drv` read | `lstat`; on a stat mismatch the file is rehashed and accepted if the hash matches |
| a directory listing's hash | `readDir` | re-list and rehash |
| a path's kind | `pathExists`, `readFileType`, the `default.nix` probe, every search-path candidate, every marker the SCM detection probed above a flake directory, a fetched store path that the store has as valid | re-stat |
| a copied tree's NAR hash and stat hash | `./path` coerced to a string, `builtins.path`, `filterSource`, a flake's working tree, a `path:` input | recompute the stat hash with `lstat` only; on a mismatch re-dump the NAR with the recorded filter and compare. A `path:` input also records its newest mtime, which the evaluation sees as `lastModified`, and the same bytes with another date are a change |
| a resolved symlink | `storePath`, `builtins.path` and `filterSource` when canonicalizing changed the path | canonicalize again |
| an environment variable, as a keyed hash | `getEnv`; `HOME` when a `~/` literal is compiled | read it again and hash it again |
| which query of which SCM was asked in which directory, and its exit status and output | the queries of the SCM that read a flake working tree or a repository; for git, `ls-files`, `status`, `rev-parse HEAD`, `show-ref --head` for a `ref`, and `config core.attributesFile` for `export-ignore` | ask the same query again, rehash |

What follows from an observation is not itself an observation. The date
and the count of a revision follow from the revision, because a commit is
named by the hash of its content, its date and its parents.
`rev-parse HEAD` is the observation, so a validation starts no `git log`
and no `git rev-list`, which would cost 1 s on each hit on a nixpkgs
checkout. A tree has a count only when it is clean and has its full history,
and those two facts stay observations: `git status` gives the dirty flag,
and the marker `.git/shallow` is a path observation.

Detection is an observation too. A flake directory with no working tree in
a parent directory is a `path:` input because no ancestor had a marker, so
iets records each probe. After `git init` in a parent directory the next run
misses, as it must.

The count is kept in this store by revision. The key holds the build id,
the name of the SCM and the revision, and no command. The entry holds the
count and an empty read set, because nothing can make it stale. Nix keeps the
same two values by revision in its fetcher cache. The flags that turn the
tracing cache off turn this off too, so the count is then computed at each
read.

On a clean nixpkgs checkout with 1,066,246 commits, storing the count
halves a miss that doesn't read `revCount` (2.23 s to 1.18 s) and a hit
(1.09 s to 0.63 s). A hit reads the state of each of the 54,000 files of
the tree (`lstat`) and starts `git ls-files`, `git status` and
`git rev-parse`.

One tree is not walked. A `path:` input that names a store path, such as
`path:/nix/store/…-source`, whose name is the NAR hash of the directory
itself, records only the path's kind: still a directory, so still the same
bytes. Validating a nixpkgs pinned that way costs two system calls, where one
`lstat` per file would cost 0.51 s. A store path whose name is not the hash
of its content, such as a build output, keeps its tree observation.

A fetch records the kind of its store path only after it checked that the
path is valid. A hit only stats the path again and asks no daemon: a valid
path does not change, and the garbage collector deletes what it
invalidates.

The read set has a second reader. `iets cache paths ENTRY` prints the paths
in it that a file watcher can use, and `nix/direnvrc` turns them into direnv
`watch_file` calls ([Use iets with direnv](../how-to/use-with-direnv.md)).
`print-dev-env --entry-to FILE` writes the path of the entry it used, so no
caller has to derive the key a second time.

A run that had an entry and lost it says why, in one line:

```
$ iets eval --strict ./flake-attrs.nix
re-evaluating: /home/u/repo/flake.nix changed
```

The line names the one observation that is out of date: a file's contents, a
directory's entries, an environment variable, a working tree. Editing the
file you named on the command line looks the same as editing anything it
imports. Under `--log-format internal-json` it is a level-3 record with code
`cache-invalidated`
([Errors, exit codes and log records](../reference/errors.md)). Validation
runs on four threads, and the lowest-indexed failure wins, so one change
always names the same thing. A run with no entry prints nothing. In
classic mode the line stays off stderr and the record goes to
`json-log-path` alone
([Known divergences from Nix 2.34](../reference/divergences.md)).

The key is computed from the command *before* evaluation: the build id, the
subcommand, the expression, or the path of the file or the flake directory,
the `--arg` pairs, `-A`, `--strict`, `--json`, `--raw`, the lint levels, the
search path, `builtins.currentSystem`, and the working directory only when a
relative path literal can reach it. `--workers`, `--builder`, `--timings`
and `--dry-run` are not in the key: they do not change the answer.

The key names the file by path, never by content, and the same goes for a
flake's `flake.nix` and `flake.lock`. They are in the read set like any
other file, so one command keeps one entry however often you edit the file,
and an edit is an invalidation that names the file. With content in the key,
an edit would be a different question: the run would miss with no entry, say
nothing about why, and leave an entry for every earlier version of the file
until the sweep.

Some things make a run uncacheable rather than wrong: `builtins.currentTime`
(only if the program forces it) and every network fetcher without a hash.

An entry is one file in a directory named by the key: a header with a
checksum, then parts. `readset` holds the observations. `result` holds what a
hit replays: the stdout bytes for `eval`, the root derivation paths for
`build`, a store path or two for `nix develop` and `nix fmt`. A `build` entry
adds `drvs` and `sources`, so a hit can still upload the closure, and
`warnings` holds the lint warnings a hit replays. An `eval` entry leaves out
`drvs` and `sources`, because nothing would read them back and writing the
16 MB `drvs` of nixos-server cost 40 ms on every miss.

One file makes the entry atomic. A killed writer leaves the previous entry
untouched, and two writers of one key each publish a whole entry. The cost
is that a hit that refreshed a stat field publishes the whole entry again,
17 MB for a `build` of nixos-server.

The cost of recording is about 3 % at one worker on nixos-server, and
inside the noise at four, because the hashing happens on the fiber that read
the file and so spreads over the workers. What it buys:

| run | wall (median) | user | sys |
|---|---|---|---|
| `--no-tracing-cache` | 1.92 s | 1.43 s | 0.47 s |
| miss (recording and writing) | 1.98 s | 1.47 s | 0.48 s |
| hit | 0.04 s | 0.02 s | 0.06 s |

The read set for that run holds 5,340 observations: 3,513 files (37.7 MB
hashed), 781 directory listings, 643 copied trees, 399 stats and four
environment variables. The read-set file is 1.1 MB. A hit is 4-5 ms to read
and decode it, 23-40 ms to validate it on four threads, and the rest is
process start.

The classic `nix` subcommands use the same cache, where it competes with
Nix's own evaluation cache (an sqlite file keyed on the locked flake's
fingerprint). The setup was a `path:` flake with a `github:` nixpkgs in the
store, a local daemon and every output valid. Medians on a busy 16-core
EPYC:

| command | iets, evaluating | iets, hit | Nix 2.34.8, warm |
|---|---|---|---|
| `nix eval .#hello` | 0.16 s | 0.013 s | 0.44 s |
| `nix build --no-link .#hello` | 0.17 s | 0.039 s | 0.048 s |
| `nix derivation show -r .#hello` | 0.17 s | 0.019 s | 0.20 s |
| `nix develop -c true` | 0.17 s | 0.027 s | 0.11 s |
| `nix eval --raw .#sys.drvPath`, a NixOS system | 2.0 s | 0.028 s | 5.1 s |
| `nix derivation show .#sys` | 2.1 s | 0.036 s | 0.075 s |

Nix never answers `nix eval` from its cache, so its two `nix eval` rows are
full evaluations. Where it does answer, the two warm paths are within a few
tens of milliseconds of each other. The real difference is when each has a
warm path at all: Nix's is off for a dirty git tree, for `-f` and for
`--expr`, and a dirty tree is what you have while you work. A git tree also
costs Nix a copy on every run. In this repository `nix develop -c true` is
0.04 s on an iets hit and 0.20 s in Nix either way, and
`nix eval --raw .#default.drvPath` 0.03 s against 1.1 s.

A hit costs its read set: 1,200 to 1,600 observations for these rows,
validated in 9 to 18 ms.

Errors are never cached. An entry is written only after the result was
printed without error, and `nix eval`'s `«error: …»` in place of a value it
could not force counts as an error.

The read set errs on the side of too much. A cancelled helper fiber still
leaves its observations in it, which costs a rare extra miss and never a
wrong hit.

`builtins.trace` and `builtins.warn` write to stderr *during evaluation*,
and a hit does not evaluate, so a hit prints none of them. Lint warnings
are different: the entry records them, and a hit replays them.

## The input store: why it needs no trust

A locked flake input that is not in `/nix/store` is downloaded into
`~/.cache/iets/inputs`. Evaluation reads the input's bytes from there, and a
build uploads the input's NAR straight out of it. Nothing is registered with
any daemon.

This needs no trust because of one decision: **an entry may only ever be
named by the hash its content must have, and that hash is known before the
fetch begins.** The lock file states the input's `narHash`, and iets derives
the store path from it before it opens a socket. It streams the archive into
a staged directory and hashes the NAR, and the staged directory becomes an
entry **only if that hash is the one the lock named**. A wrong tree is never
named, never read and never uploaded.

The content verifies itself, which removes a lot:

- **No signature is needed.** A fixed-output hash answers "who says these
  bytes are what they claim to be?" without anyone saying anything:
  recompute the hash and you know. The put operation takes the hash the tree
  must have and has no other form.
- **No trusted builder is needed.** Nothing here is built, so there is no
  input-addressed output, whose path is a claim about a build that happened
  somewhere.
- **No daemon is needed.** This store is a directory in the user's own
  cache, shared with no one, and a caller doesn't have to trust it, because
  it re-derives what the name must be from the lock.
- **Entries are safe to share between processes.** Two writers of one key
  can only write bytes the other would have written, because the name
  determines the content.

A reader checks little on open: the index's own checksum and its version
byte, and, for each pack, the checksum of the pack's index and that every
object it names lies inside it. A failed check wipes the entry or the pack,
which costs one fetch. It never hashes the tree again, because the name is
the hash, so an object whose bytes were changed in place under an index that
still fits is read as it stands. Hashing on every open would read every
input of every evaluation, and the index exists to avoid that.

An entry is one file, `inputs/<store path base name>`: the NAR hash and
size the fetch verified, and an index of every member's name, mode, size and
the blake3 of its bytes. The bytes are objects in `inputs/packs/`, stored
once however many inputs share them, so a second revision of nixpkgs costs
only the files that changed: 4 to 5 MiB for revisions 200 commits apart,
where a spool cost 247 MiB each. No tree is ever unpacked: with the index a
full nixpkgs revision takes 7.7 s and 94 MB peak, unpacked 13.5 s and
382 MB.

The sweep removes entries, and a collection under the store's lock then
removes the packs no entry names and rewrites a pack that is mostly dead. A
fetch that relied on a removed pack notices under the same lock and writes
those objects again from the download it still holds.

One place in the evaluator maps a store path to its bytes, for `import`,
`readFile`, `readDir`, `pathExists`, `readFileType`, `hashFile`,
`builtins.path`, `filterSource` and path interpolation alike: from a real
file system, from an input-store entry's objects, or from memory.
`/nix/store` is checked first and wins when it has the tree, which changes
nothing, since the two hold the same bytes by construction.

This also gives the read set a cheap shape. An input-store entry is one observation for the whole tree, because the entry's name is the NAR hash of
its tree. Validating a whole nixpkgs is one stat call and a version-byte
comparison, against the 3,513 file observations the same tree would cost
otherwise. A validated observation also marks the entry as used, at most
once a day, so the sweep doesn't take an input that is in use.

Before a process downloads, it takes a lock on that tree's name. A second
process that needs the same tree prints one line, blocks, and then reads the
entry the first one stored. If the first fetch failed, the second downloads
it itself. Different inputs still download in parallel.

## The roots store: GC roots iets owns

`~/.cache/iets/roots` is where iets keeps the GC roots it owns. Every
other root mechanism takes a path from you (`print-dev-env --gc-root PATH`,
`nix-build -o`); this is the one place where iets decides for itself. Its
users:

| Who | What the root holds | Key | Switch |
|:---|:---|:---|:---|
| `nix fmt` | the formatter's output | flake directory, `formatter.<system>` | none |
| `nix develop` | the `-env` output, which references the whole shell | flake directory, attribute path | `shell-gc-root` |
| `nix-shell` | every input the entry realised, one link each | working directory, the files and attributes asked for, the path's place in the sorted list | `shell-gc-root` |
| `iets build` | every output it printed | working directory, installable, attribute, output | `build-gc-root` |
| `nix-build --no-out-link`, `nix build --no-link` | every output that got no symlink | working directory, the files, attributes or installables asked for, the name the symlink would have had | `build-gc-root` |

An entry is a symlink into `/nix/store`, named by a 64-hex key of what the
root is for, so two projects keep two roots and one project replaces its own
in place. The key never holds a `.drv` path or an output path, because an
edit would then leave the previous root behind for the TTL to sweep. The
daemon's `gcroots/auto/<hash>` entry points at iets's link, so removing that link is the whole release.

Where a caller named a path (`print-dev-env --gc-root PATH`,
`nix-build -o result`), that path is the root and the roots store stays out
of it. `nix-shell` realises the derivation's inputs, so it takes one link per
input, numbered by the sorted path list. A set that shrank leaves links
behind, and the TTL takes them.

`shell-gc-root` and `build-gc-root` ([Settings](../reference/settings.md))
both default on and are separate switches, because the lifetimes differ. A
shell root pins a large closure for as long as somebody keeps entering that
project. A build root pins an output the caller is about to use. On the
classic mode it diverges from Nix on purpose
([Known divergences from Nix 2.34](../reference/divergences.md)), which is
reason enough to turn it off there.

Most of "Store safety" doesn't apply to roots. A
root holds nothing that can be recomputed, so there is no format version and
nothing to stage, rename, poison or wipe: a symlink is either right or
absent. The TTL sweep does apply: every run touches its link (`utimensat` on the link itself), and `iets cache gc` walks this
store with the other three. A swept root frees nothing by itself; it lets
the next `nix collect-garbage` decide. A root iets cannot register (no local
daemon, or a store that refuses) is a warning.

### Roots for `nix fmt`

`nix fmt` must not redo the evaluation of `formatter.<system>` or the build
of what that names. A formatter that nothing points at is garbage, so the
next `nix collect-garbage` takes it and the run after that builds it again.
Stock `nix fmt` does both every time, which is why flake-fmt exists.

iets skips the evaluation through the tracing cache. `nix fmt` is keyed on
the flake and the system, so a second run with nothing changed revalidates
its read set and evaluates nothing. The read set holds content hashes, not
mtimes, so a file the formatter rewrote to the same bytes is still a hit.
Only a run that changed a file the evaluation read costs an evaluation. The
exception is a `path:` flake, a directory outside any git repository: its
`lastModified` is the newest mtime in the tree, so a rewrite to the same
bytes is a new date.

## Limits

The input store holds trees whose content address is a flake `narHash`. It
doesn't hold a substitutable-but-absent `/nix/store` path: `builtins.storePath`
hands such a path to the daemon, which substitutes it before any read, and
import-from-derivation is unsupported. `git` and `hg` inputs need a
repository, which iets cannot clone, so they go through a real `nix` that
writes `/nix/store`. The NAR hash of a source tree has no store of its own,
because the tracing cache's tree observation already holds it.
