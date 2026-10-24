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
| a full evaluation of each merge, for comparison | 1,760 s |
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

Early cutoff is the rest. A refactor of shared code (step 80, 61 % of the jobs)
and a position that moves inside a large file (`perl-packages.nix`,
`vscode/extensions/default.nix`, up to 2 %) invalidate jobs whose drvPath does
not change. Only evaluating the changed part again and comparing what it gave
would keep them.

## Limits

- The trace runs at one worker, without the IR cache.
- The job list of a commit comes from its full evaluation, not from an
  enumeration, and jobs that fail are left out.
- The thresholds (8 bindings, one region per binding) were not tuned.
- The trace of `with` lookups, `?` and formal arguments is a name per set; the
  trace of anything that reads a whole set, such as `attrNames` or `==`, is the
  whole set.

## Use

```
IETS_FINE_TRACE=FILE iets eval --impure --workers 1 -E EXPR   # one trace for the run
IETS_FINE_TRACE= IETS_FINE_JOBS=DIR iets eval ...              # a trace per element of a list
IETS_FINE_TRACE= IETS_FINE_DAG=DIR iets eval ...               # the graph of the run
iets fine-regions FILE=AS...                                   # the regions and names of a file
iets fine-check CHANGES DAG...                                 # the jobs a change invalidates
iets fine-check --job N DAG                                    # the observations of job N
```

The scripts of the measurements are in `bench/incremental/`. They expect a
clone of nixpkgs at `~/incr/nixpkgs`, a worktree of it at `~/incr/base`, and
the full evaluation of each commit in `~/incr/drvs/`.
