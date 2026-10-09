# Caching, and why the input store is safe

iets keeps three things on disk. Two are caches — they hold the result of
work that can be done again. One is not a cache at all: it is where a
fetched flake input *lives*. All three follow the same discipline, and this
page explains what that discipline is and why the third one is safe to keep
in a user's home directory with no daemon and no signature.

| store | default root | entry | keyed on |
|---|---|---|---|
| compiled IR | `~/.cache/iets/ir` | one file | build id, path, system, store directory, `pure-eval`, lint levels, source |
| tracing runs | `~/.cache/iets/trace` | one file in a directory | the whole command; for the count of a revision, the SCM and the revision |
| fetched inputs | `~/.cache/iets/inputs` | one directory | the store path the lock implies |

Each cache has a flag that turns it off. `--no-tracing-cache` turns off
the tracing cache. `--no-ir-cache` turns off the IR cache. `--no-cache`
turns off both, and `IETS_CACHE=0` and the `cache` setting are the same
switch. No flag turns off the input store, because it is not a cache: a
run needs the input's bytes. `--cache-dir DIR` moves all four stores.

A fourth directory, `~/.cache/iets/roots`, is neither a cache nor a
store of content: it holds the GC roots iets owns. See the section on it
below.

The ideas come from Mic92's nix-tarmac: a writer lock a caller may give up
on, a touch interval so a hit writes nothing, a TTL sweep that runs at most
once a day, and poison-and-wipe instead of repair.

## One discipline, four rules

**Stage and rename.** An entry is one file. It is written under a temporary
name with everything the entry holds in it, and renamed into place. A
rename is atomic, so one rename publishes the whole entry: a reader sees
either the whole old entry or the whole new one, never a torn file and never
two generations mixed.

**Readers never lock.** The rename makes the lock unnecessary on the read
path, and the read path is the hot one. A
reader opens the entry once and reads every part it needs through that one
descriptor, so the bytes it goes on to use are the generation it validated,
whatever a sibling publishes meanwhile. The old file stays alive for as long
as that descriptor does, even after a publish or a sweep unlinked its name.

**A writer holds the store's `flock` for exactly one entry, and gives up
after 50 ms.** A skipped write is a miss on a later run. It is never a
stall and never an error. The lock exists so that two writers of one key
cannot cross inside one temporary file; it is not a critical section around
a run. The temporary name also carries the process id and a per-write
counter, so two threads writing one key in one process cannot cross either.
A killed writer leaves a temporary file that no reader looks at and that the
sweep removes after a day; the entry it was replacing stays whole and
usable.

Measured: four `iets eval` processes filling one cold store in parallel
wrote 97 IR entries and skipped none.

Two things the rename does not promise, and does not have to. Nothing is
fsynced, so a power loss can leave the name pointing at a file whose bytes
were never written; those bytes are then read as any other bad bytes are,
and the entry goes. And atomicity is per entry, not across entries: a run
that published two keys can be interrupted between them. No reader joins two
entries, so nothing depends on that.

**Poison and wipe; never repair.** An entry that does not decode is
deleted and the run continues as a miss. A store whose version file is of
another format, or whose garbage-collection stamp cannot be parsed, or
whose lock is not a file, is emptied and starts again.

**An entry carries a checksum over its own key.** Every entry of the IR
cache and of the tracing cache holds 16 bytes of blake3 over the key it
answers and the whole of its content, and a reader verifies it before it
decodes anything. A changed byte, a truncated entry and an entry copied to
another key are all a mismatch, and a mismatch is a miss that poisons the
entry. Without it a single changed byte gave a different answer with exit 0.

The check is also the only thing the two stores trust. A reader opens an
entry with `O_NOFOLLOW` and `O_NONBLOCK`, accepts a regular file alone, and
refuses a length above 512 MiB — a hundred times the largest entry nixpkgs
makes — before it allocates anything, so a name replaced by a symbolic link,
a FIFO or a sparse file of a terabyte is a miss and not a hang or an
out-of-memory abort.

**Nothing in an entry names a program.** An observation of a working tree
holds which query of which SCM was asked, and the validator builds the
command line from its own table, so an entry cannot start a program of its
own choosing. `nix fmt` is the one command that runs what an entry
remembered, and it runs it only when the entry names a file inside a store
path of the store directory.

**Nothing in an entry is in clear that does not have to be.** An observation
of an environment variable holds a hash keyed with the cache's own secret,
because knowing whether the value changed is the whole job. Directories of a
cache are 0700 and its files 0600 whatever the umask, and a root of an older
iets is tightened on the next run. What is still in clear is what an entry
is for: the paths a run read, the bytes it printed, and the content hash of
every file it read.

That holds for *any* bytes, not only for bytes iets wrote: a cache
directory is writable by whoever runs iets, so a truncated, corrupt or
crafted entry must be a miss and never a crash. The IR decoder therefore
checks every stored constant against the immediate encodings the writer can
produce — a word is never a raw bit pattern the evaluator would follow as a
pointer — checks every index against what it has already decoded (a local
slot against its frame, a `with` depth against a `with` scope and the depth
before it, a symbol against the entry's name table, a position against the
file's length), checks every invariant the evaluator reads without one of its
own (a `with` depth list rises, an `inherit (e)` binding has a `from` list,
an allow table is in the order the writer wrote it), and bounds both its
nesting and the items its counted runs may ask for by the entry's own
length.

**iets removes only what iets put there.** A wipe is scoped to the names
that store's own layout makes — the version file, the lock, the stamp, the
temporary files, the entries and, for the IR cache, its shard directories.
Anything else in the directory survives, and the directory itself is never
removed. The three stores are subdirectories (`trace/`, `ir/`, `inputs/`)
of the cache root, so `--cache-dir DIR` and `IETS_CACHE_DIR` never make
`DIR` itself a removal target. A store subdirectory that exists, holds
something, and carries no name from that layout is not iets's: iets
refuses it with an error naming it and runs without that cache, rather than
writing into it or removing anything from it.

The sweep removes entries not read for 30 days. It runs on a write path, at
most once a day per store, and never on the read path. The stamp is claimed
under the writer lock and the walk then runs unlocked, so a sweep of a large
store never holds a sibling's write back. It only ever removes a name that
looks like one of that store's keys, so the version file, the lock, the
stamp, and any nested store under a shared cache directory all survive.

A sweep of a tracing store of 10,000 entries, warm page cache, 16-core
EPYC: 0.03 s when nothing is past the TTL, 0.13 s for a dry run that also
sizes every entry, 0.57 s when all 10,000 go. The once-a-day sweep of a
real store is the first row.

A warm hit writes nothing at all. The tracing cache rewrites its read set
only when validation refreshed a stat field, and otherwise touches the
entry's mtime only when that mtime is over a day old. The IR cache takes
the mtime from the same `fstat` that sizes its read, and touches through
that open descriptor. So a hot cache on a busy machine does no disk writes.

## The IR cache: the same file, compiled once

Parsing and compiling the 3,500 files of the nixos-desktop workload was
about 28 % of a one-worker run. The IR cache writes each imported file's
compiled IR after its first compile and loads it on later runs. All the
files load in 229 ms against 696 ms to compile;
`hackage-packages.nix` alone loads in 71-77 ms against 190-218 ms to
compile.

Three things make the IR process-specific, and the loader undoes each one.
Symbol ids are re-interned. Positions are stored relative to the file and
rebased. Constant words are stored by content for a literal and by name for
a global or a `builtins` member. Binding frames are sorted by symbol id,
which differs between processes, so the loader re-sorts them and rewrites
every local index through a permutation stack.

Only `import` uses it. `scopedImport`, `-E` and a file under a `~/` path
compile from source. Files under 1 KiB are not cached: below that the load
costs what the compile costs.

The key holds the build id, so a new binary never reads an old entry.

It also holds everything the compiler folds out of the evaluator's
configuration, because a folded constant is stored by content and not by
name. That is the system (`builtins.currentSystem`), the store directory
(`builtins.storeDir`), the compile-time lint levels whose warnings the entry
carries, and `pure-eval`, which removes the impure names altogether: without
the flag in the key an impure run's entry answered a pure one and
`builtins.currentSystem or "fallback"` gave the system. Every other constant
is stored by name and looked up in the live tables on load, so the search
path follows the run, and a file with a `~/` path literal is never cached.

It is also the only cache `eval-jobs` uses, which makes its effect easy to
see. On `nixos/release-small.nix` with four workers, 16-core EPYC:

| tool | run | wall | max RSS |
|---|---|---|---|
| `iets eval-jobs` | cold | 0.29 s | 85 MiB |
| `iets eval-jobs` | warm | 0.26 s | 85 MiB |
| `nix-eval-jobs` | cold | 0.68 s | 113 MiB |
| `nix-eval-jobs` | warm | 0.66 s | 113 MiB |

nix-eval-jobs keeps no on-disk evaluation cache for a file argument, so its
two rows differ only in page cache.

What a second nixpkgs costs on disk, measured by evaluating the
nixos-desktop workload at the pinned nixpkgs and then at a nixpkgs two
releases older: the second added one tracing entry
(0.9 MiB) and 56 IR entries (0.5 MiB). Ten of the 66 IR entries carried
over — 15 % — and the tracing entry shares nothing at all, because its key
holds the nixpkgs path.

## The tracing cache: keyed on what a run read

This is the one that is not in Nix. `iets eval` and `iets build` remember
what a run printed, together with every observation of the outside world
the run made. A later run with the same command validates those
observations. If nothing changed, it replays the result and skips
evaluation entirely. `print-dev-env` and `search` do the same, and so do the
subcommands iets serves under the classic `nix` name — `nix eval`,
`nix build`, `nix derivation show`, `nix develop`, `nix fmt` — each with a
key of its own (`docs/reference/classic.md` says what each entry holds).

Every primop that looks at the world records one observation.

| observation | recorded by | validated by |
|---|---|---|
| a file's size, mtime, ctime, device, inode and sha256 | `import`, `readFile`, `hashFile`, every `.drv` read | `lstat`; on a stat mismatch the file is rehashed and accepted if the hash matches |
| a directory listing's hash | `readDir` | re-list and rehash |
| a path's kind | `pathExists`, `readFileType`, the `default.nix` probe, every search-path candidate, every marker the SCM detection probed above a flake directory | re-stat |
| a copied tree's NAR hash and stat hash | `./path` coerced to a string, `builtins.path`, `filterSource`, a flake's working tree, a `path:` input | recompute the stat hash with `lstat` only; on a mismatch re-dump the NAR with the recorded filter and compare. A `path:` input also records its newest mtime, which the evaluation sees as `lastModified`, and the same bytes with another date are a change |
| a resolved symlink | `storePath`, `builtins.path` and `filterSource` when canonicalising changed the path | canonicalise again |
| an environment variable | `getEnv`; `HOME` when a `~/` literal is compiled | read it again |
| an environment variable, as a keyed hash | `getEnv`; `HOME` when a `~/` literal is compiled | read it again and hash it again |
| which query of which SCM was asked in which directory, and its exit status and output | the queries of the SCM that read a flake working tree; for git, `ls-files`, `status` and `rev-parse HEAD` | ask the same query again, rehash |

What follows from an observation is no observation. The date and the
count of a revision follow from the revision: a commit is named by the
hash of its content, its date and its parents, so they cannot change while
the name stays. `rev-parse HEAD` is the observation, and a validation
starts no `git log` and no `git rev-list`. That took 1 s from each hit on
a nixpkgs checkout. A tree has a count only when it is clean and has its
full history, and those two facts stay observations: `git status` gives
the dirty flag, and the marker `.git/shallow` is a path observation.

Detection is an observation too, and not only the tree it found. A flake
directory with no working tree above it is a `path:` input *because* every
ancestor lacked a marker, so each probe is recorded. A `git init` in a
parent directory then misses, as it must: the flake becomes a git tree
whose files are the tracked ones.

One limit: a replace ref (`git replace`) or a graft changes the history
that `git` shows, and the revision stays. iets does not observe them, so
a count that the store kept does not follow such a change.

The count is kept in this store by revision. The key holds the build id,
the name of the SCM and the revision, and no command. The entry holds the
count and an empty read set, because nothing can make it stale. It is an
entry like the others: `--cache-dir` moves it, the sweep removes it 30
days after the last read, and the flags that turn the tracing cache off
turn it off, so the count is then computed at each read. Nix keeps the
same two values by revision in its fetcher cache (git.cc:getRevCount,
getLastModified).

Measured on a clean nixpkgs checkout with 1,066,246 commits (release
build, a warm IR cache, `hyperfine`, 10 runs, 2026-09-28). "Before" is the
commit that made both values observations and counted before the
evaluation:

| run of `iets eval --raw .#ATTR` | before | after | `git` processes |
|---|---:|---:|---|
| a miss, the attribute does not read `revCount` | 2.23 s | 1.18 s | 5, then 4 |
| a hit | 1.09 s | 0.63 s | 5, then 3 |
| a miss that reads `revCount`, first run at the revision | 2.23 s | 2.23 s | 5 |
| a miss that reads `revCount`, the store has the count | 2.23 s | 1.20 s | 5, then 4 |

A hit still reads the state of each of the 54,000 files of the tree
(`lstat`) and starts `git ls-files`, `git status` and `git rev-parse`.

One tree is not walked. A `path:` input that names a store path, such as
`path:/nix/store/…-source`, is hashed like any other, and when the
fixed-output path of that hash is the directory itself, its name is its NAR
hash. It is then recorded as a path's kind: still a directory, still the
same bytes, by the argument that makes an input store entry one observation
(below). A nixpkgs pinned that way cost 0.51 s to validate, one `lstat` per
file, and costs two system calls now. A store path whose name is not the
hash of its content — a build output, say — keeps its tree observation.

The read set has a second reader. `iets cache paths ENTRY` prints the paths
in it that a watcher can use, NUL-separated, or `--json` with the kind of
each. The cache owns that answer, because it owns the read set; saying it in
a watcher's own words is the consumer's part, and `nix/direnvrc` turns these
into direnv `watch_file` calls (`docs/how-to/use-with-direnv.md`). A command
names the entry it read with `--entry-to FILE`, so no caller has to derive
the key a second time.

The selection is for a watcher that compares modification times. A `Stat`
observation is left out when its path exists: it recorded existence and kind,
and an mtime is not that — a git index refresh moves `.git` without changing
what was observed. A path observed `Missing` stays, because it has no mtime
to churn and its creation is the change it looks like.

A run that had an entry and lost it says why, in one line:

```
$ iets eval --strict ./flake-attrs.nix
re-evaluating: /home/u/repo/flake.nix changed
```

The line names the one observation that no longer holds and what about it
changed — a file's contents, a directory's entries, an environment variable,
a working tree. The file the command names is one of those files, so
editing it reads the same as editing anything it imports. Under `--log-format internal-json` it is a level-3
record with code `cache-invalidated` (docs/reference/errors.md). Validation
runs on four threads, so the read set's own order decides which observation
is named: the lowest-indexed failure wins, and one change therefore always
names the same thing. A run with no entry prints nothing: a first run
invalidated nothing. Under a classic name the line stays off stderr, because
Nix prints nothing there, and the record goes to `json-log-path` alone
(`docs/reference/divergences.md`).

The key is computed from the command *before* evaluation: the build id, the
subcommand, the expression, or the path of the file or the flake directory,
the `--arg` pairs, `-A`, `--strict`, `--json`, `--raw`, the lint levels, the
search path, `builtins.currentSystem`, and the working directory only when a
relative path literal can reach it. `--workers`, `--builder`, `--timings`
and `--dry-run` are not in the key: they do not change the answer.

The key names the file by path and never by content, and the same holds for
a flake's `flake.nix` and `flake.lock`. The run reads all of them like any
other file it reads, so the read set holds them, and one command therefore
keeps one entry however often the file is edited. Content in the key would
make an edit a *different question* instead of a stale answer: the run would
miss with no entry, say nothing about why, and leave the entry of every
earlier version of the file behind until the sweep. One command, one entry,
and an edit of any file the run read — the one named on the command line
included — is an invalidation that names it.

Some things make a run uncacheable rather than wrong: `builtins.currentTime`
and every network fetcher without a hash. `currentTime` is a lazy thunk over
an internal primop, so the mark happens only if the program forces it.

`builtins.exec` is the third, and the mark is on the whole run, not on the
file that called it. The tracing cache has no unit smaller than a run, and
a value built from what a program printed is that program's answer
everywhere it flows. Recording it as an observation would not help: the
only validator is to run the program again, which is the cost and the
effect a hit exists to skip, and an entry must never start a program of
its own choosing (above). So a run marks itself at the first call that
reaches a program, and a run that only mentions `exec` stays cacheable. The
setting is in the key, because `builtins ? exec` answers it. The IR cache
never sees the output: it is compiled as a `«string»`, as `--expr` is, and
the IR cache holds files.

An entry is one file, `entry`, in a directory named by the key. The file
starts with a version byte, the number of parts, the checksum over the key
and the rest of the file, and a table of the parts — where each one is and
how long it is — and which parts exist depends on the subcommand.
`readset` holds the observations. `result` holds the exact
stdout bytes for `eval`, `nix eval` and `nix derivation show`, the root
derivation paths and the wanted outputs for `build` and `nix build`, and a
store path or two for the commands that go on to use one (`nix develop`,
`nix fmt`). A `build` or `nix build` entry adds `drvs` (every derivation the
evaluation produced) and `sources` (where each source's bytes were), so a
hit can still upload the closure, and any entry whose run printed lint
warnings adds `warnings`, which a hit replays.

An `eval` entry omits `drvs` and `sources` deliberately: the subcommand is
in the key, so nothing would read them back, and writing the 16 MB `drvs` of
nixos-desktop cost 40 ms — 3 % — on every miss.

One file is what makes the entry atomic. A lookup opens it, reads the table,
and then reads only the parts it wants out of that open file: the read set
first, and the result, tables and warnings after validation. A write the
lock cut short, or a writer that was killed, leaves the previous entry
untouched, because nothing was renamed. Two writers of one key each publish
a whole entry, so the loser's bytes are replaced, never woven in. The one
thing the file costs: a hit that refreshed a stat field has to publish the
whole entry again, not just the read set. For an `eval` entry that is the
read set plus a few kilobytes. For a `build` entry of nixos-desktop it is
17 MB, on a path that runs only when a file's mtime moved and its content
did not.

The cost of recording is about 3 % at one worker on nixos-desktop, and
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

The classic `nix` subcommands have the same cache, and there the comparison
is with Nix's own evaluation cache, which is an sqlite file of attribute
values keyed on the locked flake's fingerprint. A `path:` flake with a
`github:` nixpkgs that is already in the store, a local daemon, every output
already valid, medians on a 16-core EPYC that was busy with other work:

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
tens of milliseconds of each other. The difference is in when each one has
a warm path at all: Nix's is off for a dirty git tree, for `-f` and for
`--expr`, which is the state a tree is in while somebody works on it. A git
tree also costs Nix a copy on every run, clean or dirty. In this repository
`nix develop -c true` is 0.04 s on an iets hit and 0.20 s in Nix either
way, `nix derivation show .#default` 0.03 s against 0.08 s, and
`nix eval --raw .#default.drvPath` 0.03 s against 1.1 s.

What a hit costs is the read set: 1,200 to 1,600 observations for the rows
above, validated in 9 to 18 ms. The one case that was slow is gone — a
`path:` input that is a store path used to be walked file by file (above).
`nix build` also waited up to 250 ms for its heartbeat thread to notice the
build was over, on a hit and on a miss alike.

Two properties are worth naming. Errors are never cached: an entry is
written only after the result was printed without error. `nix eval` prints
`«error: …»` in place of a value it could not force and still exits 0, as
Nix does, and that too is an error: the read that failed left no observation
of the absence, so a run that printed one writes nothing. And the read set
over-approximates rather than under-approximates — a cancelled helper fiber
still leaves its observations in it, which costs a rare extra miss and never
a wrong hit. A file that changes mid-run may be recorded twice with two
hashes, and validation then fails on one of them, which is a miss.

One consequence surprises people: `builtins.trace` and `builtins.warn` write
to stderr *during evaluation*, and a hit does not evaluate, so a hit prints
none of them. Lint warnings are different — the entry records them and a hit
replays them.

## The input store, and why FOD-only makes it safe

A locked flake input that is not in `/nix/store` has to become readable
somehow. iets downloads it into `~/.cache/iets/inputs`, and evaluation
then reads the input's bytes out of *that*, not out of a store. A build
uploads the input's NAR straight out of it as well. Nothing is registered
with any daemon, at any point.

That sounds like it should need trust. It does not, and the reason is the
one design decision behind the whole store: **an entry may only ever be
named by the hash its content must have, and that hash is known before the
fetch begins.**

A locked flake input is a fixed-output derivation in the only sense that
matters here. The lock file states the input's `narHash`. iets derives the
store path from that hash before it opens a socket. It streams the archive
into a staged directory, writes the NAR out of the index straight into a
hasher, and the staged directory becomes an entry **only if that hash is
the one the lock named**. A wrong tree is never named, never read and never
uploaded.

So the content verifies itself. Follow what that removes:

- **No signature is needed.** A signature answers "who says these bytes are
  what they claim to be?". A fixed-output hash answers it without anyone
  saying anything: recompute the hash and you know. That is why the
  put operation takes the hash the tree must have and has no other form.
- **No trusted builder is needed.** Nothing here is built. There is no
  input-addressed output, whose path is a *claim* about a build that
  happened somewhere, and which therefore needs someone trustworthy to have
  made it.
- **No daemon is needed.** A daemon exists to be the one process allowed to
  write a shared, trusted store. This store is neither shared with other
  users nor trusted in that sense: it is a directory in the user's own
  cache, and a caller does not have to trust it because it re-derives what
  the name must be from the lock.
- **Entries are safe to share between processes.** Two writers of one key
  can only ever write bytes the other would have written, because the name
  determines the content. That is why the concurrency rules above are
  enough and no protocol is needed on top of them.

What a reader *does* check on open is small: the index's own checksum and
its version byte, and, for each pack, the checksum of the pack's own index
and that every object it names lies inside it. Anything else wipes the
entry or the pack, which costs one fetch. It never re-hashes the tree,
because the name already is the hash: an object whose *bytes* were changed
in place, under an index that still fits, is therefore read as it stands.
Rehashing it on every open would read every input of every evaluation,
which is what the index exists to avoid.

An entry is one file, `inputs/<store path base name>`: the NAR hash and size
the fetch verified, and an index of every member's name, mode, symlink
target, size and the id of its bytes, the blake3 of them. The bytes
themselves are objects in `inputs/packs/`, LZ4-compressed when that shrinks
them, and an object two inputs share is stored once. The design is
nix-tarmac's: a store of tarball contents keyed by their hash, so a second
revision of nixpkgs costs only the files that changed — 4 to 5 MiB, index
included, for revisions 200 commits apart, where a spool cost 247 MiB each.
The first revision costs 96 MiB. Where tarmac indexes
its one pack in LMDB, here every pack is written once under a temporary name,
indexes itself in a footer, and is renamed into place, which is the rule the
rest of this page follows. No tree is ever unpacked: a full nixpkgs revision
costs 19 MB of index over the evaluator's own 75 MB, where unpacking a tree
cost 307 MB — 13.5 s and 382 MB peak before, 7.7 s and 94 MB after.

The entries name the objects; the sweep removes entries. After a sweep, a
collection under the store's lock marks every object some entry names,
removes each pack that holds none of them, and rewrites a pack whose dead
bytes outweigh its live ones into a new pack. A process that has a pack open
keeps reading it after the unlink. A fetch that relied on a pack the
collection removed sees it under the same lock before it publishes, and
writes those objects itself, from the download it still holds.

One place in the evaluator maps a store path to its bytes, and it serves
`import`, `readFile`, `readDir`, `pathExists`, `readFileType`, `hashFile`,
`builtins.path`, `filterSource` and a path interpolation alike, from a real
file system, from an input-store entry's objects, or from memory. `/nix/store` is checked first and wins when it has
the tree; an input that appears there later, because a build registered it,
changes nothing, since the two hold the same bytes by construction.

This also gives the read set a cheap shape. An input-store entry is one
observation, not one per file, because the entry's name *is* the NAR hash
of its tree: a name that is still there still holds the bytes the run read.
Validating a whole nixpkgs is therefore one stat call and a version-byte
comparison, against the 3,513 file observations the same tree would cost if
its files were checked one by one. A validated observation also marks the
entry as used, at most once a day as every other touch is, because a hit
reads nothing out of the entry and the sweep would otherwise take an input
that is in use.

There is one place a process waits for a sibling. Before it downloads,
a process takes a lock on that tree's name. A second process that needs the
same tree prints one line, blocks, and then reads the entry the first one
stored. If the first fetch failed, the second downloads it itself. The lock
is per tree, so different inputs still download in parallel.

## The roots store: what iets keeps alive

`~/.cache/iets/roots` is where iets keeps the GC roots it owns. Every
other root mechanism takes a path from the caller — `print-dev-env
--gc-root PATH`, `eval-jobs --gc-roots-dir DIR`, `plumbing build --gc-root
DIR`, `nix-instantiate --add-root PATH`, `nix-build -o` — and this is the
one iets decides for itself. Its users:

| Who | What the root holds | Key | Switch |
|:---|:---|:---|:---|
| `nix fmt` | the formatter's output | flake directory, `formatter.<system>` | none |
| `nix develop` | the `-env` output, which references the whole shell | flake directory, attribute path | `shell-gc-root` |
| `nix-shell` | every input the entry realised, one link each | working directory, the files and attributes asked for, the path's place in the sorted list | `shell-gc-root` |
| `iets build` | every output it printed | working directory, installable, attribute, output | `build-gc-root` |
| `nix-build --no-out-link`, `nix build --no-link` | every output that got no symlink | working directory, the files, attributes or installables asked for, the name the symlink would have had | `build-gc-root` |

An entry is a symlink into `/nix/store`, named by a 64-hex key of what the
root is for, so two projects keep two roots and one project replaces its own
in place. A key never holds a `.drv` path or an output path: an edit would
then leave the previous root behind for the TTL to sweep, where a key made
of what the user asked for is replaced in place. The daemon's
`gcroots/auto/<hash>` entry points at our link, not at the store path, so
removing our link is the whole release: the daemon's entry goes stale and is
ignored.

One root per thing. Where a caller named a path, that path is the root and
the roots store stays out of it: `print-dev-env --gc-root PATH` keeps its
own link (and owns the failure, where a root iets chose is best effort),
`nix-build -o result` keeps `result`, and `iets build --fetch-to DIR`
already has the bytes outside the store.

A shell with no single store path to hold — `nix-shell` realises the
derivation's inputs and not the derivation — takes one link per path. It is
what nix-direnv has to spell out with `nix-instantiate --add-root` and
`nix-store --add-root` for the same reason. The links are numbered by the
sorted path list, so re-entering the same project replaces them in place; a
set that shrank leaves links behind, and the TTL takes them.

`shell-gc-root` and `build-gc-root` (`docs/reference/settings.md`) both
default on and are separate switches, because the lifetimes differ. A shell
root pins a large closure for as long as somebody keeps entering that
project. A build root pins an output the caller just asked for and is about
to use, and on the classic names it is a deliberate divergence from Nix
(`docs/reference/divergences.md`), which is its own reason to turn off.

Three of the four rules above do not apply. A root holds nothing that can be
recomputed, so there is no format version, nothing to stage and rename, and
nothing to poison and wipe: a symlink is either right or absent. Its only
writer is `replaceSymlink`, which is already atomic. The TTL sweep is the
rule that does apply, and it fits better here than anywhere else — "keep
alive what somebody used recently" is exactly what a root should mean — so
every run touches its link (`utimensat` on the link, not on the store path
it names, whose mtime is the store's epoch) and `iets cache gc` walks this
store with the other three. A swept root frees nothing by itself; it lets
the next `nix collect-garbage` decide.

The store is not iets's to claim by itself. A sweep removes only names of
its own shape, so anything else in the directory survives, and a root iets
cannot register — no local daemon, a store that refuses — is a warning, not
a failure: the command it belongs to still answers.

### Why `nix fmt` wanted one

`nix fmt` has two things it must not redo. The first is the evaluation of
`formatter.<system>`. The second is the build of what that names — and a
formatter nothing points at is garbage, so the next `nix collect-garbage`
takes it and the run after that builds it again.

Mic92's flake-fmt exists because stock `nix fmt` does both every time. It
answers the first with an `LD_PRELOAD` shim that records the mtime of every
file the evaluation opened, and the second with an out-link under
`~/.cache/flake-fmt` that doubles as a GC root.

iets already answers the first, natively and better. `nix fmt` goes through
the tracing cache, keyed on the flake and the system, so a second run with
nothing changed revalidates its read set and evaluates nothing. The read set
holds content hashes, not mtimes, and that is exactly the difference a
formatter creates: a formatter rewrites files, and a file rewritten to the
same bytes is still a hit. An mtime says it changed. So the steady state of
`nix fmt` — run it, it reformats nothing, run it again — is a hit, and only
a run that really changed a file the evaluation read costs an evaluation.
The one exception is a `path:` flake, a directory outside any git
repository: its `lastModified` is the newest mtime in the tree, the
evaluation can see it, and so a rewrite to the same bytes is a new date and
an evaluation.

The second idea is the roots store above.

## What this does not cover

The input store holds trees whose content address is a flake `narHash`. It
does not hold a substitutable-but-absent `/nix/store` path, and it does not
need to: `builtins.storePath` hands such a path to the daemon, which
substitutes it before any read, and import-from-derivation is unsupported,
so the evaluator never needs bytes the store does not have yet.

`git` and `hg` inputs are the other gap. They need a repository, which iets
cannot clone, so they still go through a real `nix` that writes
`/nix/store`.

The NAR hash of a source tree has no store of its own. It does not need one:
the tracing cache's tree observation already holds it.
