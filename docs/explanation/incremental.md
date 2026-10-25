# Incremental evaluation (prototype)

This branch holds a prototype. It records what each job of an evaluation
depends on, at the grain of one binding, so that after a change only the jobs
whose dependencies changed are evaluated again. Nothing here is a supported
interface.

## The question

A merge to nixpkgs changes few derivations. Over 100 consecutive merges of
2026-10-24 (`9dbd131e6c65` to `4e63084a455b`), a merge changed the drvPath of
0.015 % of the 82,553 `x86_64-linux` jobs on average, and of 1.6 % over the
whole window. CI still evaluates every job for every merge.

The tracing cache ([How caching works](caching.md)) cannot help: it keeps one
read set for the whole command, and any change misses. A read set per job
with a file as its unit does not help much either. Most jobs read
`all-packages.nix`, `python-packages.nix`, `maintainer-list.nix` and the
`pkgs/by-name` listings, so a merge invalidated 14.6 % of the jobs on average,
and one merge in ten invalidated all of them.

## What the prototype records

`IETS_FINE_TRACE` turns the trace on, at one worker and without the IR cache.

| observation | made when | invalid when |
|---|---|---|
| a region: a file's root, or one binding of a set or `let` of 8 bindings or more | the evaluation enters it | its syntax changes; positions and comments do not count |
| a name looked up in such a set, or in a `readDir` listing | `get`, `?`, `with`, a formal argument | the name comes or goes in a set or listing the lookup went through |
| a whole read of such a set or listing | `attrNames`, `==`, iteration | any name of it comes or goes |
| a position that reached a value | `unsafeGetAttrPos`, `__curPos` | a byte before it changes |
| a file, a copied tree, a path's kind | as in the tracing cache | as in the tracing cache |

The compiler wraps the value of each binding of a large set in `Ir::Region`,
and the region's hash leaves out the names of the set. So a new package
changes neither the region around `all-packages.nix`'s set nor any other
binding of it. A local variable is hashed by the name of its binder, never by
its slot: slots follow symbol ids, and those follow the order of interning,
which differs between two runs.

`//`, `mapAttrs`, `removeAttrs` and the flat copy of a union keep the
provenance of their operands. A lookup of `fetchurl` in `pkgs` is therefore a
lookup in `all-packages.nix`'s set and in the `by-name` listings it was made
from, and the addition of a package does not change it.

Each force of a thunk is a frame. What the frame observes, and an edge to the
trace of each forced thunk it reads, become the thunk's trace, which the
thunk's state word holds once it is forced. The trace of a job is what the
trace of its thunk reaches. So one evaluation of many jobs that share thunks
still gives each job everything it depends on, and no more:

- 1,000 jobs evaluated together get the same regions, files, trees and
  paths as when each runs alone, and the check of their traces against the
  window gives the same answer for every job and change;
- a cell keeps the forced thunk instead of its value, and `mapAttrs` makes
  real thunks, so a read of a forced value always goes through its trace;
- a copy of a path to the store is cached, so a cached copy reads the trace of
  the first one.

## Checking a change

`iets fine-regions FILE=AS` prints the regions and the names of the large sets
of a file as if it were at `AS`. A change between two revisions is then a list
of changed regions, names that came or went per set and per directory, the
files that changed, and the first changed byte of each file.

`IETS_FINE_DAG=DIR` writes the traces of a run as one graph, with the trace
of each job of the result list. `iets fine-check CHANGES DAG...` marks the
changed observations and makes one pass over the graph, in the order the
traces were made, since a trace comes after the traces it reads.

## Results

All measurements are on bld1 (32 cores, 125 GB), at one worker per process,
without the IR cache.

### How many jobs a merge invalidates

On a random sample of 1,000 jobs, with the merges of the window:

| model | mean per merge | merges at 99 % or more |
|---|---|---|
| a file read invalidates | 14.65 % | 10 of 100 |
| this trace | 4.54 % | 3 of 100 |
| this trace, with three nixpkgs patterns removed (below) | 0.69 % | 0 of 100 |
| only the jobs whose drvPath changed | 0.015 % | 0 of 100 |

Of the 0.69 %, 0.63 % comes from one merge, a refactor of
`build-rust-package` that changed no derivation (#578099). Without it the mean
is 0.06 %.

### What recording costs

All 80,618 jobs that evaluate at the base commit, in 8 processes of one
worker each, run at once; the mean of three rounds:

| run | wall | sum of the processes | largest process |
|---|---|---|---|
| the branch's base commit | 16.10 s | 115.4 s | 5.9 GB |
| this branch, trace off | 16.53 s (+3 %) | 118.4 s (+3 %) | 5.9 GB |
| trace on | 22.76 s (+41 %) | 163.4 s (+42 %) | 7.3 GB (+24 %) |
| trace on, graph written | 23.86 s (+48 %) | 168.6 s (+46 %) | 7.5 GB (+27 %) |

A process records 42 to 55 million forces in 170 to 213 million words of
traces and about 1.4 million distinct observations. Its graph is 0.7 to
0.9 GB on disk. The drvPaths of the two runs are the same, and they are those
of the full evaluation.

### What checking costs

`iets fine-check` of one merge against the 8 graphs, which load in parallel,
takes 1.35 s of wall time and 4.9 s of CPU, 4 % of the 115 s the processes of the evaluation take.
Nearly all of it is reading the graphs, which a process that kept them would
not pay.

For each of the 111 changes of the window (each merge, each 10 merges, all
100), across all 80,618 jobs, 3,865 drvPaths changed, and the check flagged
every one of them.

### Merge by merge

The replay takes the 100 merges in order. For each merge it checks the live
graphs against the merge's changes, moves the worktree to the new commit, and
evaluates again, with the trace on, only the invalid jobs and the new ones.
Their traces replace the old ones for the next merge. It then compares every
drvPath, kept or new, with the full evaluation of that commit.

| | |
|---|---|
| drvPaths compared | 8.06 million, every job at every commit |
| wrong drvPaths | 0 |
| jobs evaluated again | 365,378, 4.5 % |
| time for the 100 merges | 469.5 s, of which 187.8 s is the check |
| a full evaluation of each merge, for comparison | 1,610 s (16.1 s each) |
| time per merge, median | 2.48 s |
| time per merge, 90th percentile | 13.2 s |

Eleven merges evaluate 9,000 jobs or more: three evaluate every job (6, 52,
88), one 61 % (80), seven 11 % (each adds a `by-name` package). They take
220 s. The other 89 merges take 249 s together, 2.4 s at the median, and
evaluate 125 jobs each on average. Of those 249 s, 163 s is the check, which
reads every graph from disk again. A checker that kept the graphs in memory
would not pay that; it was not built.

## Why some merges still invalidate everything

Three places in nixpkgs read a whole large set for every job:

| place | what it reads | what triggers it |
|---|---|---|
| `maintainers/computed-team-list.nix:8` | `maintainersById` groups every maintainer by GitHub id | a maintainer is added or edited |
| `pkgs/top-level/stage.nix:158` | `conflictingAttrs = intersectAttrs res super` asserts that `all-packages.nix` and `by-name` share no name | a name of `all-packages.nix` comes or goes |
| `lib/customisation.nix:319` with `all-packages.nix:1516` | `texFunctions` passes `pkgs` itself as the arguments of `callPackage`, and `unpassedArgs` takes `attrNames` of them | a `by-name` package comes or goes (11 % of jobs) |

The third has a one-line fix in `lib`: `removeAttrs fargs (attrNames allArgs)`
is `filterAttrs (n: _: !(allArgs ? ${n})) fargs`. The second is a check whose
answer does not change; a checker that knew `intersectAttrs` could recompute
it from the listings. The first needs early cutoff.

A refactor of shared code (step 80, 61 % of the jobs) and a position that
moves inside a large file (`perl-packages.nix`, `vscode/extensions/default.nix`,
up to 2 %) also invalidate jobs whose drvPath does not change. Early cutoff,
below, keeps most of the third kind and of the moved positions.

## Early cutoff with named queries

A trace that changed does not always give a new value. A meta edit changes
the trace of a package's `outPath`, but not the string. Early cutoff
evaluates the changed value again and keeps its readers when the value is
the same.

That value needs a name that the next commit can evaluate. The prototype
names a value by its attribute path in one package set, the root, which
`builtins.fineRoot` marks. A batch is then `pkgs = import nixpkgs { ... }`,
with the configuration of `ci/eval/outpaths.nix`, and its jobs are
`pkgs.<path>.drvPath`. For the 80,618 jobs of the base commit, these drvPaths
are the drvPaths of `outpaths.nix`.

### What a query records

- Each cell of the root set is a query, named by its attribute. A set that a
  query forces to is tagged with the query's path. A cell of a tagged set
  becomes a query when a lookup or an iteration reaches it.
- A read of a query records the trace of its force and its path, in place of
  an edge.
- The first read of a path records a fingerprint of the value. A scalar or a
  list of scalars counts by value. A set counts by its names and by those
  cells that hold no thunk. A function has no fingerprint: a read of it is an
  edge, and a call of it depends on the traces of every set that holds it.
- `==` on a tagged set depends on how the set was built.

### Routes

Many paths can reach one thunk. In nixpkgs, `pkgsBuildBuild`,
`pkgsHostTarget` and the other `pkgs*` sets are `self // { ... }` copies, so
they share every cell of the root. Each thunk keeps a ring of the paths that
reached it, and a read records the first of them.

A reader can reach the thunk through any path of the ring, or through no
tagged set (`callPackage` reads `intersectAttrs` of a union). The check is
therefore:

| the force of the thunk | the sets above the paths of its ring | the read |
|---|---|---|
| clean | clean | clean |
| changed | clean | a suspect: its path is evaluated again |
| clean, or suspects | clean, or suspects | a suspect of those suspects |
| any | one changed | changed |

A ring grows during the run. A reader sees only the paths that existed when
its trace closed. Without this rule, a set would depend on paths made after
it, and the check would loop.

A set that a second force reaches (`foo = foo_2`) is not tagged again. Its
reader gets an edge to that force, and the queries that the force read carry
over.

### Checking and verifying

`iets fine-check --serve` keeps the graphs in memory. A check marks each job
as hard, as a suspect with the queries to verify, or as clean. The replay
then:

1. evaluates the hard jobs, the new jobs, the suspects whose queries no
   other suspect reads, and `builtins.fineQuery` of the other queries, in
   batches of 2,000;
2. keeps a suspect when every query it read keeps its fingerprint;
3. evaluates the other suspects.

A verified query has a new trace in its batch. The checker reads the newest
trace of each path, so it never checks a stale trace again. A graph that
owns no job stays loaded while it holds the newest trace of a query that
another graph read.

### What queries cost

The 80,618 jobs as `pkgs.<path>.drvPath`, in 8 processes of one worker each,
at once; the mean of three rounds (`overhead2.sh`):

| run | wall | sum of the processes | largest process |
|---|---|---|---|
| the branch's base commit | 14.62 s | 103.6 s | 5.3 GB |
| this branch, trace off | 15.52 s (+6 %) | 108.2 s (+4 %) | 5.3 GB |
| trace on | 21.47 s (+47 %) | 150.7 s (+45 %) | 6.6 GB |
| trace on, queries | 24.18 s (+65 %) | 172.0 s (+66 %) | 6.7 GB |
| trace on, queries, graph written | 25.13 s (+72 %) | 179.0 s (+73 %) | 7.0 GB |

The base commit varies by 3 % between runs (15.38 s in `compare.sh`), so
the cost of the trace when it is off is between 1 and 6 %.

A process has 470,000 to 530,000 paths. About 104,000 of them were read as
queries with a fingerprint, and 84,000 to 113,000 joined the ring of another
path. Paths are
made lazily: a first version that gave a path to every cell of every tagged
set made 16.9 million paths per process and took 118 s.

A read of a path that is no query depends on the sets above every path of
its ring. A ring has 31 paths on average, so the readers of one ring share
these edges as one trace. Without it, the edges were 25 % of the graph, and
the run with queries took 26.63 s and 7.9 GB. The check gives the same
result for the 111 change sets, and the replay below takes 394.0 s in place
of 399.2 s, with the same jobs.

### What it gives

The same 100 merges, with the same batches and the same trace, once with
every suspect evaluated and once with the cutoff (`replay2.sh`):

| | every suspect evaluated | cutoff |
|---|---|---|
| wrong drvPaths, of 8.06 million | 0 | 0 |
| jobs evaluated | 365,705, 4.5 % | 226,848, 2.8 % |
| queries verified | 0 | 57,675 |
| evaluation | 356.3 s | 244.9 s |
| check | 122.5 s | 154.3 s |
| time for the 100 merges | 478.8 s | 399.2 s |
| time per merge, median | 1.93 s | 2.22 s |
| time per merge, 90th percentile | 17.1 s | 5.7 s |

The cutoff pays where one change reaches many jobs through a value that
stays the same:

| merges | what changed | evaluated, without and with the cutoff |
|---|---|---|
| 9, 43, 50, 51, 83, 85, 97 | a `by-name` package came (`texFunctions`) | 9,002 to 9,652 jobs, 15.6 to 17.2 s; 27 to 180 jobs and 4 to 34 queries, 1.5 to 4.5 s |
| 12, 63 | an edit moved the positions in `perl-packages.nix` | about 1,550 jobs, 17 s; about 50 jobs and 50 queries, 4 s |
| 80 | a refactor of `build-rust-package` | 52,712 jobs, 27.0 s; 40,074 jobs and 4,524 queries, 25.6 s |
| 52, 88 | a maintainer came or went (`maintainersById`) | 80,625 jobs, 34 to 35 s; 43,637 to 55,441 jobs and 20,003 to 32,107 queries, 32 to 38 s |
| 6 | a name of `all-packages.nix` went (`conflictingAttrs`) | every job in both |

Step 80 keeps little because a function has no fingerprint. Steps 52 and 88
keep a third to a half of the jobs, but they verify nearly one query per kept
job, so they cost as much as evaluating everything. Step 6 changes the
construction of the root set, which every job reads.

The check costs more with the cutoff, 0.6 to 3.3 s per merge: a kept job
stays in its old graph, so more graphs stay loaded. A checker that merged
graphs would not pay that.

The batch shape alone changes little. Without the cutoff, the batches of
`pkgs` evaluate 365,705 jobs, and those of `outpaths.nix` 365,378.

### Compared with CppNix and stock iets

The same 80,618 jobs at the base commit, in 8 processes, at once; the mean
of three rounds (`compare.sh`). Both give the drvPaths of the full
evaluation. A merge is warm for the caches of stock iets only in part: its IR
cache keeps the files that did not change, and its tracing cache misses.

| run | wall | sum of the processes | largest process |
|---|---|---|---|
| CppNix 2.35.2, `nix-instantiate --eval --strict` | 29.93 s | 213.1 s | 5.0 GB |
| stock iets, no cache | 15.38 s | 108.2 s | 5.3 GB |
| stock iets, IR cache warm | 14.70 s | 105.0 s | 5.3 GB |
| stock iets, tracing cache hit (the same commit) | 0.04 s | 0.2 s | |
| this branch, the full traced run of a first commit | 25.13 s | 179.0 s | 7.0 GB |
| this branch, a merge (`replay2.sh`, the check included) | 2.22 s median, 5.73 s at the 90th percentile, 38.2 s at most | | |

CppNix has no evaluation cache for this kind of expression: a second run
takes as long as the first. The resident checker holds every live graph in
memory, which the table leaves out: 9.9 GB for the 8 graphs of the first
commit, and more while kept jobs hold older graphs.

## Limits

- The trace runs at one worker, without the IR cache.
- The job list of a commit comes from its full evaluation, not from an
  enumeration, and jobs that fail are left out.
- The thresholds (8 bindings, one region per binding) were not tuned.
- The trace of `with` lookups, `?` and formal arguments is a name per set; the
  trace of anything that reads a whole set, such as `attrNames` or `==`, is the
  whole set.
- A query is an attribute path in one package set. A value outside it, or a
  function, has no cutoff.
- A route that changed makes every read below it changed, even when the
  reader took another route: the ring has no record of the route a reader
  took.

## Use

```
IETS_FINE_TRACE=FILE iets eval --impure --workers 1 -E EXPR   # one trace for the run
IETS_FINE_TRACE= IETS_FINE_JOBS=DIR iets eval ...              # a trace per element of a list
IETS_FINE_TRACE= IETS_FINE_DAG=DIR iets eval ...               # the graph of the run
iets fine-regions FILE=AS...                                   # the regions and names of a file
iets fine-check CHANGES DAG...                                 # the jobs a change invalidates
iets fine-check --job N DAG                                    # the observations of job N
iets fine-check --serve                                        # a resident checker (load, drop, gc, check)
builtins.fineRoot SET                                          # the cells of SET are queries
builtins.fineQuery VALUE                                       # the fingerprint a query records
```

The scripts of the measurements are in `bench/incremental/`. They expect a
clone of nixpkgs at `~/incr/nixpkgs`, a worktree of it at `~/incr/base`, and
the full evaluation of each commit in `~/incr/drvs/`.
