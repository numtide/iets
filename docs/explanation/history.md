# What was tried and removed

You have an idea for the evaluator, the build path or the settings, and
there's a fair chance we already tried it. This page keeps what we built,
what we measured and why it came out again, newest first, so nobody has to
try the same idea a third time. The code and the full tables are in git. The
numbers behind a *current* default live on that feature's page.

## No machine-wide settings file

*2026-09-27*

iets read `/etc/iets/iets.toml` (or `$IETS_CONF_DIR/iets.toml`) under the
user file, and the NixOS module had an `iets.settings` option to write it.
It copied the shape of `/etc/nix/nix.conf`. The file, the option and the
variable are gone.

iets is a client. In practice the file held one setting: `builder`, with a
`token-file` in one user's home directory. Every account on the host got
that builder, and root read that user's token. The setting belongs to the
user, so it lives in `~/.config/iets/iets.toml`. Settings come from the
user file, `$IETS_CONFIG`, `--iets-option` and the `IETS_*` variables.

iets ignores a leftover `/etc/iets/iets.toml`, without an error.

## No style lints

*2026-09-27*

The registry had 41 lints. 18 of them came from the parser and the
compiler and were about how code looks: `with`, `with-non-literal`,
`with-nested`, `shadow-builtin`, `shadow-binding`, `unused-binding`,
`unused-formal`, `rec-unused`, `legacy-let`, `deprecated-builtin`,
`overrides`, `unsafe-builtin`, `current-system`, `if-bool-literal`,
`eq-bool`, `manual-inherit`, `empty-let` and `ellipsis-without-bind`. They
went, along with the families `scope` and `style`.

The reason is scope: iets reports what only an evaluator can
know, and has no opinion about how code looks. See
[What iets lints](lint-scope.md). Style is the project's call, and nixpkgs
has its own style for each kind of file. statix and deadnix read the syntax, so copying
their rules into iets told you nothing new.

The removal took 523 lines out of `src/`. The compiler doesn't record a
frame's bindings and uses. On nixos-server with one worker, a run that
compiles every file went from 12.201 G to 12.183 G instructions. A run with
a warm compiled-file cache did not change (9.393 G).

In place of the lints, iets gives you the data. `iets parse --json` prints
the syntax tree with each node's byte span, and a finding is a record with
a span and an edit. A rule is a program you write.

## Late writes to settings

*2026-09-27*

`parallel::guard` was a check in debug builds only. It took a fingerprint
of the fiber template's settings, and after the run it compared that with
the main fiber's settings. A setting written after the clone made a test
fail.

It caught a late write only when a test happened to do one, and it did
nothing in a release build. The bugs that led to it (`--dump-drvs`, the IR
cache, the search path) had one cause: the evaluator had public setting
fields, and `api::configure` wrote them after construction.

`eval::EvalConfig` replaced the guard, `api::configure` and
`Run::configured`. The settings go to a builder, and the evaluator's
setting fields are private, so a late write does not compile. See
[How iets evaluates](evaluation.md).

## No `fast-build`

*2026-09-26*

`iets fast-build` and its classic mode command `nix-fast-build` are gone. They
were 920 lines of another tool's command line: twelve flags it existed to
refuse, its own meaning for `-f`, its own `--option` handling and its own
reject type, beside 510 lines of `iets build`. With them went the result
records behind it (`build/results.rs`: the JSON and JUnit result files,
`--stream-json-lines`, the Actions step summary), the stream that fed it
(`jobs/stream.rs`) and `--retries` in the dispatcher.

What it could do that `build` could not is `build`'s: a set of
derivations is built whole, every root to its own verdict, and `--json`
prints the outcomes. That took about 90 lines. Skipping what is built
needed none: the store already does it.

Running evaluation and builds at the same time did not come along. It
needed a second pipeline: a dispatch thread, a batcher and an incremental
uploader. And it was not worth it. On this repository's 18 checks, the
whole tree evaluates in 0.8 s with the parallel walk and 1.1 s with
`nix-build`'s sequential one, while the builds take minutes. The tracing
cache, which the streaming walk could not use, makes the second run 0.03 s.

## TOML settings

*2026-09-26*

iets's own settings lived in `iets.conf`, in nix.conf's line syntax, while
the lints lived in a TOML `iets.toml`: two formats for one program. The
settings are TOML, in an `iets.toml`. See [Settings](../reference/settings.md).
iets does not read `iets.conf`, and nothing checks for a leftover one: the
NixOS module wrote the new file and removed the old one in the same switch.
`include` and `extra-<name>` went with the format.

## The ATerm buffer

*2026-09-25*

A profile put 9.3 % of a warm run in glibc `malloc` and `free`, even
though the evaluator allocates from an arena. 46 % of that came through
`RawVecInner::finish_grow`. The culprit was the derivation pipeline. Every
derivation is unparsed three times, each into a fresh 4 KiB `String` that
then grows. Inside each unparse, every store path, hash and method name was
built as its own `String` first.

The ATerm goes into one buffer per thread that keeps its capacity. The
buffer is taken out of its slot while in use, because
`hash_derivation_modulo` recurses. `print_store_path`, `print_method_algo`,
`base16_encode_into` and `Hash::write_string_with` append to it directly.
`StorePath::new`,
`make_store_path`, `make_type` and the fixed-output payload size their
buffer once instead of growing it, and `make_type` stopped printing each
reference into a string of its own.

| warm, instructions | before | after | |
|---|---:|---:|---:|
| nixos-server, 1 worker | 9.74 G | 9.39 G | −3.6 % |
| nixos-server, 4 workers | 10.54 G | 10.19 G | −3.4 % |
| nixos-minimal, 1 worker | 7.51 G | 7.22 G | −3.9 % |
| nixos-minimal, 4 workers | 8.12 G | 7.84 G | −3.5 % |

The drvPaths were identical on both workloads and equal to nix 2.34.8. The
drv fixtures round-trip byte for byte. The allocator
bucket went from 780 M instructions to 516 M, and `realloc` fell out of the
profile, so the work went away instead of moving. What is left is
`BTreeSet<StorePath>` clones and the `String` per store path that the
reference sets are made of.

## The loser tree

*2026-09-25*

The first profile of a warm-cache run put `AttrIter::next` at 7.1 %, second
only to `eval` itself and 2.3× the cost of a lookup. The multi-leaf merge
scanned every leaf head twice per attribute: once to pick the minimum, and
once to advance it. So eight leaves cost 16 head checks. A census of one
nixos-server run found where that landed: 8.4 million merge steps, of which
5.1 million came from iterations over six, seven or eight leaves, a few
hundred iterations of 5,000 to 12,000 attributes each.

A loser tree over the heads replaced the scan: one root path per attribute,
three comparisons at eight leaves. Equal syms are ordered by leaf, so the
winner surfaces first and the duplicates behind it are dropped as they
come. `Arena::flat` also merged twice, once through `len()` to size the copy
and once to fill it, which was 1.6 million of the 8.4 million steps. It
sizes the copy at the sum of the leaf lengths, fills it in one pass, and
gives the tail back to the bump pointer.

| warm, instructions | before | after | |
|---|---:|---:|---:|
| nixos-server, 1 worker | 10.03 G | 9.74 G | −2.9 % |
| nixos-server, 4 workers | 10.86 G | 10.54 G | −3.0 % |
| nixos-minimal, 1 worker | 7.75 G | 7.51 G | −3.0 % |
| nixos-minimal, 4 workers | 8.40 G | 8.12 G | −3.2 % |

The drvPaths were identical on both workloads and equal to nix 2.34.8. A fresh profile confirmed the bucket shrank instead of moving.
`AttrIter::next` went from 7.05 % to 4.49 % of a total that is itself 2.9 %
smaller, 707 M instructions to 437 M. `Arena::flat` fell out of the 0.4 %
list.

A randomized test over every leaf count found what the key encoding must
keep. A real head must never tie with the sentinel that fills the tree
while it is built, so keys start at 1, not 0. The tie-break must prefer the
*last* leaf, or `//` returns the wrong side.

## The IR cache in helper fibers

*2026-09-25*

`parallel::try_run` clones the evaluator as the fiber template, and
`api::evaluate` installed the IR cache after that clone, inside the main
fiber. So every helper fiber ran without one. On nixos-server with a warm
cache, a run loaded 2804 entries at one worker and 60 at four. Each helper
parsed and compiled its own imports again.

| nixos-server, warm, instructions | before | after |
|---|---:|---:|
| 1 worker | 10.03 G | 10.03 G |
| 4 workers | 13.05 G | 10.88 G |

That is −16.6 % at four workers, and wall time went from 0.59 s to 0.47 s,
with identical drvPaths. The parallel overhead we had measured as +30 % was
+8 %, and the rest was this bug. A cold four-worker run also wrote the
cache it was meant to fill: 62 entries before, 2807 after. The search path
had the same flaw, so `<nixpkgs>` on a helper fiber resolved against an
empty `__nixPath`. The fix installs both before `try_run`, and
`src/api/run.rs` holds the regression test: the fiber template carries the
configuration, and a warm run reads one entry per file whatever the worker
count.

## `BuildPaths` in nix-grpc-store

*2026-09-20*

nix-grpc-store dropped `BuildPaths` and the `farm` flag in its store info
reply. Every node builds through its scheduler, so a standalone daemon
schedules onto itself. iets followed: one schedule stream per batch, a want
per derivation, and one build call to the assigned worker. The client-side
salting that worked around an unavailable worker went too, and a bounced
build goes back to the scheduler. Until iets followed, every build against
the farm failed with an empty `UNIMPLEMENTED`.

## Build flows

*2026-09-19*

The last build flow outside the dispatcher went. `nix-instantiate`,
`nix-shell` and `print-dev-env` had kept a client flow of their own, 392
lines with its own timings, because folding them into the dispatcher risked
classic stdout. They drive the worker backend through the shared upload,
and the classic byte comparison stayed at 59 of 59.

Cruft that went with it: a compile cache committed under `bench/` (5,287
files, 88 MB, regenerated by a script), one machine's benchmark results, a
workload generator and two workloads nothing ran, a demo script, and the
test fixtures of the removed object store. Kept on purpose: the parse cases
of the vendored Nix language suite, which no script runs, because
`iets parse` dumps its own AST format and not Nix's.

## The `pub(crate)` sweep

*2026-09-19*

We made every `pub` a `pub(crate)`, then restored the real external surface
(the binary, the examples, the tests) one compiler round at a time. After
that, rustc's dead-code lint named 48 unreachable items.

We cut:

- those items: a second string-context parser, ten `nix.conf` getters
  nothing read, provenance and parallel helpers
- creation-time speculation
- three wrappers that had one implementation each

We kept on purpose the test oracle for the spool reader, the `is_empty`
twins clippy requires, and one heap kind whose removal shifts every
discriminant.

We proposed and did *not* cut:

- One flag table for the six argument parsers. It would be about 120 lines
  of `match` against a 100-line dispatcher, and the hand-written usage
  strings would be at risk.
- One varint codec for the three on-disk formats, about 40 net lines.
- Two compression dependencies. Dropping them drops `.tar.bz2` and `.tar.xz`
  flake inputs, which is a behavior change and not a cleanup.

`src/` went from 50,315 to 49,397 lines.

## Creation-time speculation

*2026-09-19*

Helpers evaluated thunks at creation time, behind a flag, off by default.
Fifteen runs of nixos-server: 1.401 s off, 1.544 s at level 1, 1.405 s at
level 3; user time 2.58 s against 2.98 s.

It burned CPU on work nobody asked for, and with no collector the results
stayed in memory. It also needed a third deque lane, a backlog cap and a
creation budget. Removed.

## Scheduling flags

*2026-09-18*

`iets build` got flags for a pool of N connections, upload batches of B
objects on M connections, per-derivation or K-at-a-time build operations,
submitting while the upload runs, critical-path order, and skipping
substitutable paths. They were measured on synthetic graphs of several widths
and depths, on hello and on a nixos-server upload, over a latency shim at 0
and 20 ms, over a unix socket, and against a real host 0.11 s away.

None paid. Per-derivation operations were 7.3× slower on one graph and
10× on another, because each one builds a daemon worker and the connection
is serial. Upload batches of 256 added 0.25 s and of 32 added 0.4 s. A
second uploader added 0.3 s and 9 % daemon CPU.

The reason is that the daemon registers paths one SQLite transaction at a
time, at 0.3–0.5 ms per path, and no client-side cut changes that. Extra
connections paid only as extra `max-jobs`, which the existing options
message already sets. The existing flow stayed; the flags, the shim and the
benchmark script went.

## Single-thread ideas

*2026-09-18*

To keep an idea, wall time had to be better by more than 2 % on one of the two
one-worker workloads, with no regression above 1 % elsewhere.

Kept: the compiled-IR cache.

Rejected:

- **Transient frames on a fiber-local stack.** 20 M of 59 M frames proved
  uncaptured, and it saved 320 MB of RSS on outpaths. But wall time was
  −1.7 % there and +2.5 % on server.
- **An inline cache on attribute selection.** 82 % hits on server, and yet
  +2.2 % to +5.8 % wall, because a miss stores into a shared IR node.
- **One escaped ATerm per derivation.** User time −1 to −3 %, wall −1.2 % on
  outpaths: under the bar, because the derivation path is only about 6 % of
  a run.

## Memory ideas

*2026-09-18*

None of these saved enough to pay for itself. The census came from the
outpaths workload after the four packing changes, with the arena at 7.1 GB:

- **Attribute positions in a side table.** A 16-byte attribute pads back to
  16 without a struct-of-arrays rewrite.
- **Compiling `hackage-packages.nix` lazily.** About 15 MB, but the AST
  stays alive.
- **Flattening `let` frames.** Under 56 MB.
- **A 24-byte string header.** 43 MB.
- **One shared closure for lambdas that capture nothing.** 16 MB.
- **One shared empty attribute set.** 22 MB.

Scoped release without a collector was ruled out too. Failed `tryEval`
scopes held 1.28 MB and not one was free of escapes. Per-unit scopes in
eval-jobs are 0.9 % of the arena, and the rest is the package set the next
unit reuses. Only a collector or a process restart bounds a long run.

## A builder daemon

*2026-09-18*

`iets-builderd`, with its own protocol, and a content-defined-chunking
object store were both removed. The `grpc://` transport is a *client* of
nix-grpc-store, added the same day: somebody else's daemon, with iets as
its client.

## An eval cache

*2026-09-17*

Proposed, measured, not built. The idea: publish "attribute A of source S
evaluates to P" next to the NARs, keyed by the source's NAR hash, the
system and the attribute path, signed with the cache key.

Measured for the 80,000 jobs of the outpaths workload: the index is 4.5 MB
with zstd, and 0.27 MB as a three-week delta. Against that, a lookup
competes with a 1.0 s local evaluation of a NixOS system and a 0.03 s
tracing-cache hit. The first milestone would have been a publisher in
`eval-jobs` and a consumer flag in `build`.

## Parallel tweaks

*2026-09-16*

Each was inside the noise or worse:

- An arena prefault thread (+20 % at sixteen workers).
- No stack trimming, plus prefaulting new stacks.
- A lock-free interner cache in front of the shards, which doubled the
  interner's own self time.
- AST buffer reuse.
- Coercion batches of eight.
- glibc malloc tunables.

Restricting the arena count was far worse.
