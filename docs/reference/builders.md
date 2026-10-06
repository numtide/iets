# Builders and the wire

Where a build goes, what the client sends it, and the two protocols it
speaks: the Nix worker protocol and `grpc://`. The NAR format both use is
at the end.

`iets build` evaluates in memory, computes the closure of the root
derivations, and ships what the store lacks. The client owns no local
store: nothing on either path reads `/nix/store` unless an input
derivation came from there. Why this shape instead of `nix-build`'s:
`docs/how-to/build-remotely.md`.

## The builder URL

```
--builder daemon
--builder unix:///path
--builder ssh-ng://[user@]host[?remote-program=CMD]
--builder grpc://host:port[?k=v&…]
```

Without the flag the URL is the `builder` setting in the user `iets.toml`, and
nix.conf's `store` when that is unset (`docs/reference/settings.md`). A
classic `--store URL` or `--option store URL` wins over both.

- `daemon`, `auto` and an empty value are
  `/nix/var/nix/daemon-socket/socket`.
- `ssh-ng://` runs `ssh host nix-daemon --stdio` and uses its stdio, as
  `ssh-store.cc` does, and reads `NIX_SSHOPTS`.
- `grpc://` is its own protocol; it has its own section below.

Any other store URL is the usage error `unsupported builder 'URL':
expected daemon, unix:///path, ssh-ng://[user@]host or grpc://host:port`,
and under a classic name it falls back to the real nix.

## What the client ships

- Every derivation the evaluation created is in `Evaluator::drv.drvs`
  (`DrvInfo { outputs, references, aterm }`); `derivationStrict` keeps
  the ATerm it hashed, so the plan never re-serializes a derivation. An
  input derivation that is not in the table came from a pre-existing
  `/nix/store/*.drv` reached through string context or `import`; the plan
  reads that file, and `--timings` reports the count as `read from
  /nix/store`.
- Every source the evaluation added has an origin in `Evaluator::known`
  (`store::source::SourceOrigin`, all of them through `known.origins()`):
  a `SrcPath` plus the NAR hash and size. `./path` and `builtins.path`
  record the original local tree plus the set of entries a `filterSource`
  filter accepted, so the client re-dumps the same NAR; a fetched input
  records its input-store entry; `builtins.toFile` records the contents
  in memory and the references. After hashing, the client checks that the
  NAR still yields the planned store path and fails if the tree changed
  since evaluation.
- Sources with no recorded origin — under a derivation read from the
  store, or a path pinned with `builtins.storePath` — go to a batch
  locator: one `nix path-info --json` call. A path the local store has as
  a recursive sha256 source or a text file is read from `/nix/store`.
  Anything else, an input-addressed build output or a path that is not
  valid locally, is left to the daemon: Nix treats such an `inputSrcs`
  entry the same way, valid or substitutable on the builder.

`plan.rs`: `plan(roots, Inputs { drvs, sources })` walks the derivation
closure once, iteratively, with a visited set (O(n)), emits derivations in
post-order and the source paths in registration order, and keeps the
eval-time origin of each source. It reads and hashes nothing: the daemon
is asked first, and `Plan::resolve(missing, locate)` then locates and
hashes only the sources the daemon lacks, on a thread pool. A `toFile`
text is listed after the sources it references, so the daemon sees every
reference before the path that needs it.

`iets plumbing plan` prints the result of that walk
(`docs/reference/plumbing.md`).

### The legacy `DRV` form

```
iets build [--source STOREPATH=LOCAL]... DRV
```

`DRV` is a `.drv` store path, or a copy of one named
`<hash>-<name>.drv`. The root comes from that file and its closure from
`/nix/store`; `--source STOREPATH=LOCAL` names the original local tree of
an input source. The planner is the same one, with empty evaluation
tables.

## The dispatcher

Every command that realises a derivation goes through
`crate::build::Dispatcher`, over every transport: `iets build`, classic
`nix-build` and `nix build`. `nix-instantiate`,
`nix-shell` and `print-dev-env` use the same two halves without the
renderer.

Two ways of feeding it:

- `build::upload::PlanUpload` ships a `remote::plan::Plan` computed once,
  for the commands that know every root before they build anything.
- `build::upload::StreamUpload` is incremental, for the instantiation of
  the `eval-jobs` walk. Per batch it walks only the part of the closure it
  has not registered yet, then one `QueryValidPaths` and one
  `AddMultipleToStore`. It duplicates `remote::plan::plan`'s walk on
  purpose: `plan` cannot prune at a derivation already known valid, and
  re-planning a whole closure per batch is quadratic on a large `checks`
  set.

The dispatcher owns the batching, the provenance and the event stream; a `build::backend::Backend` owns the wire. Two
implementations:

| | worker protocol (`daemon`, `unix://`, `ssh-ng://`) | `grpc://` |
|---|---|---|
| a ready set | one `BuildPathsWithResults` | a `Want` on the `Schedule` stream and one `BuildDerivation` per derivation of the set's closure, `max-builds` in flight |
| build parallelism | the daemon's, `-j` through `SetOptions` | the client's, in flight, placed by the scheduler |
| log lines | attributed by the daemon's activity ids | by the call, which knows its derivation |
| output paths | `QueryDerivationOutputMap` | statically, from the derivations |
| out-links | the local daemon, the closure copied back first | the local daemon substitutes from its caches; the rest by `FetchNars` |

The `grpc://` fan-out is `build::fanout`, a dependency-ordered runner with
a cap on jobs in flight, unit-tested without a farm.

Stdout of the classic names is byte-identical to the real nix
(`scripts/classic-e2e.nu`); all progress goes to stderr, drawn by
`build::render`.

## The worker protocol

Code: `src/remote/worker/` (`wire.rs` framing, the `FramedSink` and
`write_object`, one `AddMultipleToStore` entry; `mod.rs` transport,
handshake, the `STDERR_*` loop and the ops; `src/remote/event.rs` what the
daemon reports). No Nix library and no store; the protocol is written from
the C++ source of Nix 2.34.8.

### Handshake

As `initConnection`: `WORKER_MAGIC_1`, our version 1.38, the daemon's
version, a feature-set exchange at >= 1.38 (we offer none), the obsolete
CPU-affinity and reserve-space words, then the daemon's `nix_version` and
its trust flag, then one `STDERR_LAST`. **Daemons below protocol 1.35 are
refused**, and no features are negotiated.

### `SetOptions`

What `RemoteStore::setOptions` sends, with these values:

| Option | Value |
|:---|:---|
| keep-failed | 0 |
| keep-going | 0 |
| try-fallback | 0 |
| verbosity | `lvlInfo` |
| max-jobs | the CPU count (`max-jobs = auto`; 0 would forbid local builds) |
| max-silent-time | 0 |
| build verbosity | `lvlVomit`, so builder output arrives as `resBuildLogLine` results attributed to their build activity, not as plain log lines |
| cores | 0 |
| substitutes | on |
| the override map | empty |

Under a classic name `worker::Options::try_from(&config)` carries
`max-jobs`, `cores`, `keep-going`, `keep-failed` and `substitute` from
nix.conf instead (`docs/reference/settings.md`).

### One op per phase

1. **`QueryValidPaths`** on every planned path (sources, text paths,
   derivations) in one call, substitute flag off. Then, off the wire,
   `Plan::resolve` on the missing sources: eval-time origins and
   `--source` overrides first, one `nix path-info --json` batch for the
   rest. (`Connection::substitute_paths` is the same op with the flag on,
   which asks the daemon to substitute before it answers. No build phase
   uses it; the flake fetcher does, on the eval store, for a locked input
   that is missing from it — `docs/reference/flakes.md`.)
2. **`BuildPathsWithResults`** on the missing sources the client cannot
   produce (plain store paths, the `DerivedPath::Opaque` case that the
   daemon turns into a substitution goal), before any upload:
   `registerValidPaths` (local-store.cc) rejects a path whose reference is
   not valid, so the daemon must hold these before the derivations that
   name them. Skipped when there are none. A failed substitution ends the
   run with the daemon's message. This is what `nix build` does through
   `copyPaths` plus `buildPaths` for an `inputSrcs` entry it does not own.
3. **`AddMultipleToStore`** of the missing ones in one framed stream, in
   plan order: sources and text paths first, then each derivation after
   its inputs. Each entry is a `ValidPathInfo` in the 1.16 layout the
   daemon fixes for this op, followed by the NAR: no deriver, NAR hash and
   size from our own dump, registration time 0, `ultimate = false`, no
   signatures, and the content address the daemon recomputes and checks:

   | Path | Content address |
   |:---|:---|
   | a source tree | `fixed:r:sha256:<nar hash>` |
   | a `builtins.toFile` path | `text:sha256:<flat hash>` plus references |
   | a `.drv` | `text:sha256:<sha256(aterm)>`, references = inputSrcs + inputDrvs |

   A derivation is a text path; `writeDerivation` registers it the same
   way. A content-addressed path is accepted from an untrusted client
   because it needs no signature.
4. **`BuildPathsWithResults`** in normal mode on `<root drv>!<out1,out2>`
   (`!*` without `--out`), the legacy `DerivedPath` string that protocol
   >= 1.30 reads, one entry per root in the same op. A failed status is
   that root's verdict. `--no-build` and `--dry-run` stop before this
   step; the substitution in step 2 still runs, or the derivations could
   not register.
5. **`QueryDerivationOutputMap`** per root that built: its output paths.
6. **`NarFromPath`** per wanted output with `--fetch-to DIR`, restored
   with `store::nar::restore_path`.

Why one op per ready set: the protocol is serial per connection, and every
`BuildPathsWithResults` constructs a daemon `Worker`
(build/entry-points.cc). A per-derivation op idles the daemon between
derivations.

### Events

| Wire | Event |
|:---|:---|
| `STDERR_NEXT` | an unattributed `Log` |
| `STDERR_START_ACTIVITY` of type `actBuild` (105) | `Started`, and the activity id maps to its derivation |
| `STDERR_RESULT` of type `resBuildLogLine` (101) or `resPostBuildLogLine` (107) | a `Log` attributed through that map |
| `STDERR_ERROR` | decoded as `readError`: message plus traces |

Under `internal-json` or with a `json-log-path` every activity message is
also forwarded to the log (`docs/reference/errors.md`).

### `--timings`

One line per phase: `plan` (the closure walk), `connect`, `query`,
`resolve` (locate plus hash of the missing sources, with the count read
from `/nix/store` when there are any), `substitute` (only when paths were
left to the daemon), `add`, `build`, `outputs`, `fetch`, `worker`. It also
counts worker ops, round trips (ops plus the three handshake exchanges)
and bytes written to the socket.

## `grpc://`

[nix-grpc-store](https://github.com/Mic92/nix-grpc-store) is a gRPC
service in front of a Nix store. iets speaks it natively, so
`--builder grpc://…` and `--store grpc://…` need no nix, no plugin and no
local store. Code: `src/remote/grpc/` (`mod.rs` the URL, TLS, auth and the
retry policy; `drvwire.rs` the worker-protocol payloads the messages
carry; `build.rs` the calls; `sched.rs` the `Schedule` stream;
`backend.rs` the transport as one of the dispatcher's backends). The
service definition is vendored verbatim as `proto/nix_remote.proto` (rev
`4f6d239e`) and compiled by `build.rs`.

### URL parameters

`grpc://host:port?k=v&…`, with nix-grpc-store's own parameter names and
defaults (`src/client/store.hh`):

| parameter | default | what it does |
|---|---|---|
| `system` | the `system` setting | `x-nix-system` on every call: which worker cluster the balancer routes to |
| `token-file` | none | file with a Keycloak/OIDC bearer token, re-read for every call. Needs TLS |
| `client-cert`, `client-key` | none | PEM client identity for mTLS. Both or neither |
| `ca-cert` | platform roots | PEM CA that verifies the server |
| `insecure` | `false` | plaintext instead of TLS. Local testing only |
| `connect-timeout` | 30 | seconds the first call waits for the service |
| `reschedule-retries` | 8 | how often a build a worker bounced goes back to the scheduler |
| `restart-grace` | 120 | seconds to wait for a scheduler that went away, or for a worker that can take an `Unplaceable` derivation |
| `max-builds` | 64 | concurrent `BuildDerivation` calls; nix.conf `max-jobs` does not apply |
| `nar-connections` | 4 | accepted for URL compatibility; this client fetches over its one channel |

### Auth and headers

| Header | Value |
|:---|:---|
| `authorization` | `Bearer <token>` from `token-file`, read fresh for each call, so a rotating projected volume or a CI refresher works (`channel.hh:TokenFileCredentials`) |
| `x-nix-system` | on every call, so an upload and the build that follows land on the same cluster. A `BuildDerivation` overrides it with the derivation's own `platform` (`build.cc:routingFor`) |
| `x-nix-worker` | on a `BuildDerivation`: the address the scheduler assigned, which the balancer routes to |
| `x-nix-features` | on a `BuildDerivation`: the derivation's `requiredSystemFeatures`, comma separated |

### Flow

`StoreInfo` once, for the trust flag. Then:

1. `QueryValidPaths` with every planned path's base name, in one call,
   then `Plan::resolve` on the missing sources — the same split the worker
   path makes.
2. The sources the client cannot produce go to the store: a worker
   substitutes an input when the build asks for it
   (daemon/main.cc:`gatherInputs`), so iets only checks that some
   substituter has them, which is what `QueryValidPaths` with `substitute`
   answers.
3. `AddMultipleToStore`: one client-streamed zstd stream whose
   decompressed bytes are exactly the worker-protocol
   `AddMultipleToStore` framing the `unix://` path writes — the same
   `write_object`, count first, then each `ValidPathInfo` at 1.16 followed
   by its NAR. The flags travel alone in the first message; the data
   follows in 256 KiB chunks, as the C++ sink does, and the stream is
   ended, not flushed, once at the end.
4. Per derivation, at most `max-builds` in flight, a derivation only once
   every input derivation of it is done: a `Want` on the batch's one
   `Scheduler.Schedule` stream (`sched.rs`, build.cc:`GrpcStore::Run`),
   with the output keys, the inputs, the features and the platform. The
   scheduler answers `Cached` (the verdict is `AlreadyValid`), `Assigned`
   with a worker address and an `assign_id`, or `Unplaceable`, which is
   advisory: the `Want` stays queued and fails after `restart-grace`. On
   `Assigned`, `BuildDerivation` to that worker carries the
   `BasicDerivation` wire form (`derivation/aterm.cc:write`) at
   `protocol = 1 << 8 | 37`, `build_mode = 0` and the `assign_id`.
   `log_line` and `phase` become `Log` events; `BuildDerivationDone.result`
   is a `BuildResult` at 1.37 and becomes the derivation's verdict, a
   `Failed` event on failure. Derivations whose outputs `QueryValidPaths`
   already reports valid are skipped, so they cost no `Want`.
5. `--fetch-to DIR`: the farm never copies an output into a local store,
   so `FetchNars` streams the NARs back — one zstd stream across all
   paths, flushed per frame, each frame tagged with its `path_index` and
   the last one of a path marked `eof`. Each NAR is spooled beside its
   destination and restored with `store::nar::restore_path`.

The worker-protocol tunnel (`Connect`) is never opened. A farm endpoint
refuses it with `UNIMPLEMENTED`, which is why `nix` needs `--eval-store
auto` there; iets always evaluates in its own process, so it needs
nothing.

### What a `write` token can and cannot do

`write` — the role an OIDC token usually gets — may request builds and
import **signed** paths. The daemon forces `CheckSigs` on for that role no
matter how trusted the proxy's uid is (`src/daemon/main.cc`). Everything
iets uploads is content-addressed, so the daemon recomputes the address
and accepts it without a signature. Sending an *unsigned,
input-addressed* path — an output built locally — needs the `trusted`
role, which is what a CI host's client certificate gets. iets never needs
it: it uploads only sources and derivations.

Outputs land in the farm's binary cache, not in the local store.
`--fetch-to` restores them into a directory. An out-link
(`grpc::build::register`) first asks the local daemon to substitute the
outputs (`QueryValidPaths` with `substitute`), which fetches them from the
farm's cache when `nix.conf` lists it. A worker's own path infos carry
only the worker's key, not the cache key. What the local daemon cannot
substitute is read back with `QueryPathInfos`, fetched with `FetchNars`,
and registered through one worker-protocol `AddMultipleToStore` with each
path's signatures, which the daemon checks as `nix copy --from` does. The
local `nix.conf` must therefore list and trust the cache (`substituters`,
`trusted-public-keys`), or the user must be trusted.

### Retries

`UNAVAILABLE` is what gRPC folds every connect failure into.

| What | Policy |
|:---|:---|
| a unary call | `GrpcStore::retrying`: 500 ms doubling to 4 s, until `connect-timeout` has passed |
| the `Schedule` stream | reconnects when the scheduler says `Restarting`, or goes away with `UNAVAILABLE`, `UNKNOWN` or `INTERNAL`, for up to `restart-grace`, and sends every unanswered `Want` again |
| a bounced `BuildDerivation` (`ABORTED`, `FAILED_PRECONDITION`, `UNAVAILABLE`, `UNKNOWN`, `INTERNAL`) | back to the scheduler, up to `reschedule-retries` times (build.cc:`buildAssigned`). Unlike build.cc, each ask after the first waits: 1 s doubling to 30 s |
| `NOT_FOUND` | the worker has not got the `.drv` closure: iets uploads it (`QueryValidPaths` then `AddMultipleToStore`, with the same routing headers) and asks once more |
| an `Assigned` naming an address that bounced twice in a row | refused without a call: the derivation goes back to the scheduler once the address's cooldown has passed, bounded by `restart-grace`, not by `reschedule-retries` |

A worker that crashed stays placeable until the scheduler notices (42 s
seen on farm.numtide.com, 2026-09-22) and every bounce off it is instant.

The back-off above is per derivation, so `src/remote/grpc/health.rs` holds
what the threads of one batch share: two consecutive bounces off an
address close it for the same ladder, and a build that returns a result on
it, failed or not, clears the count. The refusals are bounded by
`restart-grace` because they cost no call, and the error names the last
address: `waited 120s, and every worker the scheduler assigned turns
builds away (last 'bld3:50051')`.

## Measured: the transports

### The worker protocol (`scripts/daemon-e2e.nu`, release build)

"Local daemon" is this machine's `nix-daemon` on its socket; the hello
closure is GC-rooted there, so every path is valid and a cold run is
impossible. "Scratch daemon" is `nix-daemon --store local?root=DIR` on its
own socket: an empty chroot store, so the cold numbers are real uploads
and a build is not attempted (`--no-build`). `unix://` and `daemon` are
the same code; `ssh-ng://` is the same code over a child's stdio, tested
with an `ssh` shim in `PATH` that runs `nix-daemon --stdio` locally (case
6), not against a real host: no host with a reachable `nix-daemon`
accepted the key.

| case | eval | plan | connect | QueryValidPaths | AddMultipleToStore | build (realise) | ops / round trips | bytes sent |
|---|---|---|---|---|---|---|---|---|
| custom drv + 1 source tree, deleted from the store, local daemon, `--fetch-to` | 0.005 s | 0.001 s | 0.077 s | 2 paths, 2 missing, 0.000 s | 2 paths, 2.4 KB, 0.001 s | 0.87 s | 6 / 9 | 2.9 KB |
| hello, local daemon (all valid) | 0.138 s | 0.006 s | 0.074 s | 776 paths, 0 missing, 0.008 s | skipped | 0.001 s | 4 / 7 | 62 KB |
| hello, scratch daemon, cold, `--no-build` | 0.159 s | 0.008 s | 0.020 s | 776 paths, 776 missing, 0.008 s | 776 paths, 2.6 MB, 0.247 s | skipped | 3 / 6 | 2.66 MB |
| hello, scratch daemon, warm, `--no-build` | 0.146 s | 0.007 s | 0.008 s | 776 paths, 0 missing, 0.011 s | skipped | skipped | 2 / 5 | 62 KB |
| nixos-desktop, scratch daemon, cold, `--no-build` | 1.48 s | 0.094 s | 0.015 s | 4,184 paths, all missing, 0.022 s | 4,184 paths, 40.4 MB, 1.89 s | skipped | 3 / 6 | 40.8 MB |
| nixos-desktop, scratch daemon, warm, `--no-build` | 1.65 s | 0.112 s | 0.055 s | 4,184 paths, 0 missing, 0.034 s | skipped | skipped | 2 / 5 | 333 KB |
| nixos-desktop, local daemon, `--no-build` | 1.21 s | 0.121 s | 0.081 s | 4,184 paths, 413 missing, 0.201 s | 413 paths, 23.2 MB, 1.67 s | skipped | 3 / 6 | 23.5 MB |

The 62 KB of a warm hello run is the `QueryValidPaths` request: 776 full
store paths as padded strings, and the 776 answers back. The custom drv's
0.87 s "build" is the sandbox build itself; the 0.87-1.1 s spread across
runs is the daemon, not the client. The 413 paths the local daemon lacked
for nixos-desktop are large nixpkgs subtrees that no local evaluation had
copied yet — the NixOS manual sources, python packages; the run registered
them, exactly as `nix-instantiate` would have.

The upload is daemon-bound: about 0.5 ms per path for the sequential
`addToStore` (restore, hash, check, lock, one SQLite transaction). No
client-side cut of the upload changes that
(`docs/explanation/history.md`).

### Byte identity

`scripts/daemon-e2e.nu` checks all of this after an upload:

- the `.drv` file in the store is `cmp`-identical to the one
  `nix-instantiate` wrote before it was deleted;
- `nix-store -q --hash`, `nix derivation show` and `nix path-info --json`
  (`ca`, `narHash`, `narSize`, `references`) match for the derivation and
  for the source;
- in the scratch store the hello derivation has the same hash as in the
  local store, and every input source has the same `ca`.

### `grpc://` (`scripts/grpc-e2e.nu`, release build)

`nix-grpc-daemon` in plaintext standalone mode — it schedules onto itself
— in front of a scratch chroot store on this machine (`--proxy-socket` a
`nix-daemon` on that store, `--proxy-store local?root=DIR`), so these are
the transport's own figures, not a farm's.

| case | connect | QueryValidPaths | AddMultipleToStore | build | RPCs |
|---|---|---|---|---|---|
| custom drv + 1 source tree, `--fetch-to` | 0.009 s | 2 paths, 2 missing, 0.020 s | 2 paths, 721 B, 0.009 s | 0.63 s | 6 |
| hello cold, `--no-build` | 0.000 s | 776 paths, 776 missing, 0.006 s | 776 paths, 490 KB, 0.26 s | skipped | 3 |
| hello warm, `--no-build` | 0.000 s | 776 paths, 0 missing, 0.006 s | skipped | skipped | 2 |

The cold hello upload is 490 KB on the wire against 2.66 MB over the
worker protocol: the same bytes, zstd level 1 in one stream across all 776
paths. `connect` is the `StoreInfo` round trip, which the daemon answers
from a cached backend connection.

What the other cases check:

- **Case 4** runs classic `nix build --store grpc://…?insecure=1
  --eval-store auto` and compares stdout with the real nix and its
  nix-grpc-store plugin, which matched here. The script prefers the `nix`
  on `PATH`, falls back to `/run/current-system/sw/bin/nix` — the dev
  shell's nix is often a different libnixstore than the installed plugin
  was built against — and, failing both, compares against the same store
  over the worker protocol instead and says so.
- **Case 5** checks the out-link path end to end: no local substituter has
  the output, so it is fetched back with `FetchNars` and handed to the
  local daemon, which refuses it for lacking a signature. That is correct,
  because a scratch daemon builds unsigned outputs; on a real farm the
  local daemon substitutes the output from the cache instead.
- **Case 6** runs `iets build --builder grpc://… --json` on the `checks`
  of `scripts/lib.nu`'s `checks-flake` and compares every outcome's
  attribute and success with the same flake built over the worker
  protocol on the same store.
- **Case 7** checks that a multi-root `iets build` over `grpc://` names
  every attribute on stderr.

Against `farm.numtide.com` with a Keycloak `write` token (2026-09-20): a
`runCommand` with 572 cached inputs went `Want`, `Assigned`,
`BuildDerivation` on bld1 in 2.6 s, 5 RPCs; a classic `nix-build` out-link
then came from cache.numtide.com through the local daemon. The fan-out's
own scheduling — the cap on builds in flight, the dependency order, a
failed derivation blocking its dependants, and the drv-tagged merge into
one event stream — is `build::fanout`, tested without a farm.

## NAR

`src/store/nar.rs` is the whole NAR implementation: the archive writer
(`dump_path`, `dump_string`), the hashes Nix addresses paths by
(`nar_hash`, `nar_size`, `flat_hash`), and the reader (`restore_path`,
`parse_length`).

It is on the hot path of every build. `src/remote/plan.rs` hashes every
source of a build in a thread pool, `src/remote/worker/wire.rs` dumps each
one again into the upload, `src/builtins/paths.rs` hashes a tree whenever
`"${./tree}"`, `builtins.path` or `filterSource` needs its store path, and
`src/remote/copy.rs` and `--fetch-to` restore NARs from a stream.

### Parity

The output is byte-identical to `nix-store --dump`, not merely
hash-identical. `tests/nar_parity.rs` proves it on a tree of unicode and
awkward names, empty files, empty directories, symlinks (dangling, and
pointing out of the tree), executables, contents on and off the 8-byte
boundary, deep nesting and a file larger than the writer's buffer:

- the whole NAR matches `nix-store --dump` byte for byte;
- every subtree on its own hashes like `nix-store --dump | sha256sum`;
- a dump restored by `restore_path` dumps back to the same bytes;
- `parse_length` agrees with `nar_size`, and rejects a truncated archive,
  a misspelled keyword and non-zero padding;
- the reader refuses a name past `NAME_MAX` rather than failing later at
  `File::create`.

Each test skips itself when `nix` is absent; `nix develop` has it.

### Shape

The reader follows nix-community/go-nix PR #123 (`pkg/narv2`, edef): a
synchronous state machine over one buffered window, with peek and discard,
token *groups* matched against pre-computed framed constants, and the node
shape chosen from a single byte. The writer emits the same groups.

- **Token groups, not tokens.** NAR framing is canonical — exact lengths,
  zero padding — so a run of keywords is one fixed byte string.
  `framed()` builds them at compile time (`TOK_NAR`, `TOK_REG`, `TOK_EXE`,
  `TOK_SYM`, `TOK_DIR`, `TOK_ENT`, `TOK_NOD`, `TOK_PAR`).
- **Byte dispatch.** The groups are cut so that byte 16 of a peeked node
  header names the node: `target` (6) after `symlink`, `contents` (8)
  after `regular`, `executable` (10) after an executable `regular`, and
  `y` — `directory`'s own last letter — for a directory. Inside a
  directory, byte 0 of a 16-byte peek is 1 for `)` and 5 for `entry`.
- **One window on each side.** `NarSink` stages framing and reads file
  contents into the same buffer; `Reader` is that window backwards. The
  sink starts at 128 KiB and grows to 1 MiB only when a file's contents
  would not fit.
- **No per-entry allocation.** The walk keeps one byte path buffer, one
  name arena and one entry index, all held to stack discipline. A node's
  type comes from the parent's `readdir`, not a second `lstat`.

### Measured

AMD EPYC Genoa, warm page cache, the pinned nixpkgs tree
(`/nix/store/ah3…-source`: 40k entries, 212 MB of NAR) and one 500 MB
file. `bench/nar.nu` records these; `bench/nar.nu --nix` adds the
reference rows.

| case | before | after | reference |
|---|---|---|---|
| NAR hash of the tree | 848 ms | 665-672 ms | `nix hash path` 1665 ms |
| dump of the tree, no hash | 705 ms | 545-567 ms | `nix-store --dump \| sha256sum` 2034 ms |
| NAR hash of a 500 MB file | 342 ms | 353-359 ms | `nix hash path` 386 ms |
| restore of the tree from a pipe | 1962-1988 ms | 1744-1804 ms | `nix-store --restore` 2019 ms |
| the parser alone | no such path | 30.5 ms (6.97 GB/s) | — |
| `"${./tree}"` store path | 1156 ms | 692-696 ms | — |

`parse_length` walks the framing of the whole 212 MB, 40k-entry archive in
30.5 ms, 1.7 % of a restore of the same archive; the rest is ext4 inode
creation and writeback. `perf` puts 99 % of the 500 MB hash in
`sha2::sha256::x86::digest_blocks`, the SHA-NI path `cpufeatures` selects
at run time, at the hardware's single-stream rate of 1.46 GB/s.

There is no separate NAR-hash cache: `Obs::Tree` of the tracing cache
records `nar_hash` beside a `stat_hash` over every entry's relative path,
type, size, mtime, inode and executable bit, and validation recomputes
only the `stat_hash`, re-dumping the NAR only when that differs
(`docs/explanation/caching.md`).
