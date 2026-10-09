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
  Nix's `ssh-ng` store does, and reads `NIX_SSHOPTS`.
- `grpc://` is its own protocol; it has its own section below.

Any other store URL is the usage error `unsupported builder 'URL':
expected daemon, unix:///path, ssh-ng://[user@]host or grpc://host:port`,
and under a classic name it falls back to the real nix.

## What the client ships

- **Every derivation the evaluation made**, as the exact ATerm it hashed.
  An input derivation the evaluation did not make came from a
  `/nix/store/*.drv` reached through string context or `import`; the
  client reads that file, and `--timings` counts it as `read from
  /nix/store`.
- **Every source the evaluation added**, dumped again from where the
  evaluation read it: a local tree with the entries a `filterSource`
  filter accepted, a fetched input, or a `builtins.toFile` text held in
  memory. After hashing, the client checks that the NAR still gives the
  planned store path, and fails if the tree changed since evaluation.
- **Sources the evaluation did not add** (under a derivation read from the
  store, or a path pinned with `builtins.storePath`) go to one
  `nix path-info --json` call. A path the local store has as a recursive
  sha256 source or a text file is read from `/nix/store`. Anything else,
  an input-addressed build output or a path that is not valid locally, is
  left to the daemon: Nix treats such an `inputSrcs` entry the same way,
  valid or substitutable on the builder.

The client walks the closure once and reads nothing during the walk. It
asks the daemon which paths it has, then locates and hashes only the
sources the daemon lacks. Each path is sent after every path it
references: sources first, a `toFile` text after the sources it names,
each derivation after its inputs.

`iets plumbing plan` prints the result of that walk
(`docs/reference/plumbing.md`).

### The legacy `DRV` form

```
iets build [--source STOREPATH=LOCAL]... DRV
```

`DRV` is a `.drv` store path, or a copy of one named
`<hash>-<name>.drv`. The root comes from that file and its closure from
`/nix/store`; `--source STOREPATH=LOCAL` names the original local tree of
an input source. The rest is the same as above.

## The two transports compared

`iets build`, classic `nix-build` and `nix build` build the same way over
every transport. `nix-instantiate`, `nix-shell` and `print-dev-env`
upload the same way. `iets eval-jobs` uploads in batches while the walk
runs.

| | worker protocol (`daemon`, `unix://`, `ssh-ng://`) | `grpc://` |
|---|---|---|
| a ready set | one `BuildPathsWithResults` | a `Want` on the `Schedule` stream and one `BuildDerivation` per derivation of the set's closure, `max-builds` in flight |
| build parallelism | the daemon's, `-j` through `SetOptions` | the client's, in flight, placed by the scheduler |
| log lines | attributed by the daemon's activity ids | by the call, which knows its derivation |
| output paths | `QueryDerivationOutputMap` | statically, from the derivations |
| out-links | the local daemon, the closure copied back first | the local daemon substitutes from its caches; the rest by `FetchNars` |

Stdout of the classic names is byte-identical to the real nix
(`scripts/classic-e2e.nu`); all progress goes to stderr.

## The worker protocol

iets uses no Nix library and no local store. The protocol is written from
the C++ source of Nix 2.34.8.

### Handshake

As Nix's `initConnection`: `WORKER_MAGIC_1`, our version 1.38, the
daemon's version, a feature-set exchange at >= 1.38 (we offer none), the
obsolete CPU-affinity and reserve-space words, then the daemon's
`nix_version` and its trust flag, then one `STDERR_LAST`. **Daemons below
protocol 1.35 are refused**, and no features are negotiated.

### `SetOptions`

The fields Nix's `RemoteStore::setOptions` sends, with these values:

| Option | Value |
|:---|:---|
| keep-failed | 0 |
| keep-going | 1 with `--keep-going` or more than one root, else 0 |
| try-fallback | 0 |
| verbosity | `lvlInfo` |
| max-jobs | the CPU count (`max-jobs = auto`; 0 would forbid local builds) |
| max-silent-time | 0 |
| build verbosity | `lvlVomit`, so builder output arrives as `resBuildLogLine` results attributed to their build activity, not as plain log lines |
| cores | 0 |
| substitutes | on |
| the override map | empty |

Under a classic name these come from nix.conf instead: `keep-failed`,
`keep-going`, `fallback`, `max-jobs`, `max-silent-time`, `cores` and
`substitute`. The override map then holds every other store setting the
user set outside the system nix.conf (`docs/reference/settings.md`).

### One op per phase

1. **`QueryValidPaths`** on every planned path (sources, text paths,
   derivations) in one call, substitute flag off. Then, off the wire,
   the client locates and hashes the missing sources: eval-time origins
   and `--source` overrides first, one `nix path-info --json` batch for
   the rest. The flake fetcher uses the same op with the substitute flag
   on, for a locked input missing from the eval store
   (`docs/reference/flakes.md`).
2. **`BuildPathsWithResults`** on the missing sources the client cannot
   produce (plain store paths, which the daemon turns into a substitution
   goal), before any upload. The daemon rejects a path whose reference is
   not valid, so it must hold these before the derivations that name
   them. Skipped when there are none. A failed substitution ends the run
   with the daemon's message. `nix build` does the same for an
   `inputSrcs` entry it does not own.
3. **`AddMultipleToStore`** of the missing ones in one framed stream, in
   plan order. Each entry is a `ValidPathInfo` in the 1.16 layout the
   daemon fixes for this op, followed by the NAR: no deriver, NAR hash and
   size from our own dump, registration time 0, `ultimate = false`, no
   signatures, and the content address the daemon recomputes and checks:

   | Path | Content address |
   |:---|:---|
   | a source tree | `fixed:r:sha256:<nar hash>` |
   | a `builtins.toFile` path | `text:sha256:<flat hash>` plus references |
   | a `.drv` | `text:sha256:<sha256(aterm)>`, references = inputSrcs + inputDrvs |

   A derivation is a text path, as Nix registers it. A content-addressed
   path is accepted from an untrusted client because it needs no
   signature.
4. **`BuildPathsWithResults`** in normal mode on `<root drv>!<out1,out2>`
   (`!*` without `--out`), the legacy `DerivedPath` string that protocol
   >= 1.30 reads, one entry per root in the same op. A failed status is
   that root's verdict. `--no-build` and `--dry-run` stop before this
   step; the substitution in step 2 still runs, or the derivations could
   not register.
5. **`QueryDerivationOutputMap`** per root that built: its output paths.
6. **`NarFromPath`** per wanted output with `--fetch-to DIR`, restored
   into `DIR`.

Why one op per ready set: the protocol is serial per connection, and every
`BuildPathsWithResults` starts a new daemon worker. A per-derivation op
idles the daemon between derivations.

### Events

| Wire | Event |
|:---|:---|
| `STDERR_NEXT` | an unattributed `Log` |
| `STDERR_START_ACTIVITY` of type `actBuild` (105) | `Started`, and the activity id maps to its derivation |
| `STDERR_RESULT` of type `resBuildLogLine` (101) or `resPostBuildLogLine` (107) | a `Log` attributed through that map |
| `STDERR_ERROR` | decoded as Nix's `readError`: message plus traces |

Under `internal-json` or with a `json-log-path` every activity message is
also forwarded to the log (`docs/reference/errors.md`).

### `--timings`

One line per phase, on stderr:

```
plan: 0 sources, 0 drvs, 0.009s
connect: protocol 1.38, trusted, 0.001s
query: 9 paths, 8 missing, 0.002s
resolve: 7 sources, 6 not from this eval, read from /nix/store, 0.003s
substitute: 5 paths left to the daemon, 0.004s
add: 4 paths, 3 bytes, 0.005s
build: 0.006s
outputs: 0.007s
fetch: 2 outputs, 0.008s
worker: 1 ops, 2 round trips, 3 bytes sent
```

`resolve` is the locate and hash of the missing sources. `substitute`
appears only when paths were left to the daemon. Round trips are the ops
plus the three handshake exchanges.

## `grpc://`

[nix-grpc-store](https://github.com/Mic92/nix-grpc-store) is a gRPC
service in front of a Nix store. iets speaks it natively, so
`--builder grpc://…` and `--store grpc://…` need no nix, no plugin and no
local store.

### URL parameters

`grpc://host:port?k=v&…`, with nix-grpc-store's own parameter names and
defaults:

| parameter | default | what it does |
|---|---|---|
| `system` | the `system` setting | `x-nix-system` on every call: which worker cluster the balancer routes to |
| `token-file` | none | file with a Keycloak/OIDC bearer token, re-read for every call. Needs TLS |
| `client-cert`, `client-key` | none | PEM client identity for mTLS. Both or neither |
| `ca-cert` | none | PEM CA trusted in addition to the usual roots (`NIX_SSL_CERT_FILE` in reference/settings.md) |
| `insecure` | `false` | plaintext instead of TLS. Local testing only |
| `connect-timeout` | 30 | seconds the first call waits for the service |
| `reschedule-retries` | 8 | how often a build a worker bounced goes back to the scheduler |
| `restart-grace` | 120 | seconds to wait for a scheduler that went away, or for a worker that can take an `Unplaceable` derivation |
| `max-builds` | 64 | concurrent `BuildDerivation` calls; nix.conf `max-jobs` does not apply |
| `nar-connections` | 4 | accepted for URL compatibility; this client fetches over its one channel |

### Auth and headers

| Header | Value |
|:---|:---|
| `authorization` | `Bearer <token>` from `token-file`, read fresh for each call, so a rotating projected volume or a CI refresher works |
| `x-nix-system` | on every call, so an upload and the build that follows land on the same cluster. A `BuildDerivation` overrides it with the derivation's own `platform` |
| `x-nix-worker` | on a `BuildDerivation`: the address the scheduler assigned, which the balancer routes to |
| `x-nix-features` | on a `BuildDerivation`: the derivation's `requiredSystemFeatures`, comma separated |

### Flow

`StoreInfo` once, for the trust flag. Then:

1. `QueryValidPaths` with every planned path's base name, in one call,
   then the client locates and hashes the missing sources, as on the
   worker protocol.
2. The sources the client cannot produce stay in the store's hands: a
   worker substitutes an input when the build asks for it, so iets only
   checks that some substituter has them, which is what
   `QueryValidPaths` with `substitute` answers.
3. `AddMultipleToStore`: one client-streamed zstd stream whose
   decompressed bytes are exactly the worker-protocol
   `AddMultipleToStore` framing the `unix://` path writes: count first,
   then each `ValidPathInfo` at 1.16 followed by its NAR. The flags travel
   alone in the first message; the data follows in 256 KiB chunks, and the
   stream is ended, not flushed, once at the end.
4. Per derivation, at most `max-builds` in flight, a derivation only once
   every input derivation of it is done: a `Want` on the batch's one
   `Scheduler.Schedule` stream, with the output keys, the inputs, the
   features and the platform. The scheduler answers `Cached` (the verdict
   is `AlreadyValid`), `Assigned` with a worker address and an
   `assign_id`, or `Unplaceable`, which is advisory: the `Want` stays
   queued and fails after `restart-grace`. On `Assigned`,
   `BuildDerivation` to that worker carries the `BasicDerivation` wire
   form at `protocol = 1 << 8 | 37`, `build_mode = 0` and the
   `assign_id`. `log_line` and `phase` become `Log` events;
   `BuildDerivationDone.result` is a `BuildResult` at 1.37 and becomes the
   derivation's verdict, a `Failed` event on failure. Derivations whose
   outputs `QueryValidPaths` already reports valid are skipped, so they
   cost no `Want`.
5. `--fetch-to DIR`: the farm never copies an output into a local store,
   so `FetchNars` streams the NARs back: one zstd stream across all
   paths, flushed per frame, each frame tagged with its `path_index` and
   the last one of a path marked `eof`. Each NAR is spooled beside its
   destination and restored there.

The worker-protocol tunnel (`Connect`) is never opened. A farm endpoint
refuses it with `UNIMPLEMENTED`, which is why `nix` needs `--eval-store
auto` there; iets always evaluates in its own process, so it needs
nothing.

### What a `write` token can and cannot do

`write`, the role an OIDC token usually gets, may request builds and
import **signed** paths. nix-grpc-store forces `CheckSigs` on for that
role no matter how trusted the proxy's uid is. Everything iets uploads is
content-addressed, so the daemon recomputes the address and accepts it
without a signature. Sending an *unsigned, input-addressed* path (an
output built locally) needs the `trusted` role, which is what a CI host's
client certificate gets. iets never needs it: it uploads only sources and
derivations.

Outputs land in the farm's binary cache, not in the local store.
`--fetch-to` restores them into a directory. An out-link first asks the
local daemon to substitute the outputs (`QueryValidPaths` with
`substitute`), which fetches them from the farm's cache when `nix.conf`
lists it. A worker's own path infos carry only the worker's key, not the
cache key. What the local daemon cannot substitute is read back with
`QueryPathInfos`, fetched with `FetchNars`, and registered through one
worker-protocol `AddMultipleToStore` with each path's signatures, which
the daemon checks as `nix copy --from` does. The local `nix.conf` must
therefore list and trust the cache (`substituters`,
`trusted-public-keys`), or the user must be trusted.

### Retries

`UNAVAILABLE` is what gRPC folds every connect failure into.

| What | Policy |
|:---|:---|
| a unary call | 500 ms doubling to 4 s, until `connect-timeout` has passed |
| the `Schedule` stream | reconnects when the scheduler says `Restarting`, or goes away with `UNAVAILABLE`, `UNKNOWN` or `INTERNAL`, for up to `restart-grace`, and sends every unanswered `Want` again |
| a bounced `BuildDerivation` (`ABORTED`, `FAILED_PRECONDITION`, `UNAVAILABLE`, `UNKNOWN`, `INTERNAL`) | back to the scheduler, up to `reschedule-retries` times. Unlike nix-grpc-store's own client, each ask after the first waits: 1 s doubling to 30 s |
| `NOT_FOUND` | the worker has not got the `.drv` closure: iets uploads it (`QueryValidPaths` then `AddMultipleToStore`, with the same routing headers) and asks once more |
| an `Assigned` naming an address that bounced twice in a row | refused without a call: the derivation goes back to the scheduler once the address's cooldown has passed, bounded by `restart-grace`, not by `reschedule-retries` |

A worker that crashed stays placeable until the scheduler notices (42 s
seen on farm.numtide.com, 2026-09-22), and every bounce off it is
instant. So all builds of one run share what they learn about an address:
two bounces in a row close it for the same 1 s to 30 s ladder, and a
build that returns a result on it, failed or not, opens it again. The
error names the last address:
`'<drv>': waited 120s, and every worker the scheduler assigned turns
builds away (last 'worker:50051')`.

## Measured: the transports

### The worker protocol (`scripts/daemon-e2e.nu`, release build)

"Local daemon" is this machine's `nix-daemon` on its socket; the hello
closure is GC-rooted there, so every path is valid and a cold run is
impossible. "Scratch daemon" is `nix-daemon --store local?root=DIR` on its
own socket: an empty chroot store, so the cold numbers are real uploads
and a build is not attempted (`--no-build`). `ssh-ng://` is the same code
over a child's stdio; the script tests it with an `ssh` shim that runs
`nix-daemon --stdio` locally, not against a real host.

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
0.87 s "build" is the sandbox build itself. The 413 paths the local daemon
lacked for nixos-desktop are nixpkgs subtrees that no local evaluation had
copied yet; the run registered them, as `nix-instantiate` would have.

The upload is daemon-bound: about 0.5 ms per path for the sequential
`addToStore`. No client-side change to the upload changes that
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

`nix-grpc-daemon` in plaintext standalone mode (it schedules onto itself)
in front of a scratch chroot store on this machine, so these are the
transport's own figures, not a farm's.

| case | connect | QueryValidPaths | AddMultipleToStore | build | RPCs |
|---|---|---|---|---|---|
| custom drv + 1 source tree, `--fetch-to` | 0.009 s | 2 paths, 2 missing, 0.020 s | 2 paths, 721 B, 0.009 s | 0.63 s | 6 |
| hello cold, `--no-build` | 0.000 s | 776 paths, 776 missing, 0.006 s | 776 paths, 490 KB, 0.26 s | skipped | 3 |
| hello warm, `--no-build` | 0.000 s | 776 paths, 0 missing, 0.006 s | skipped | skipped | 2 |

The cold hello upload is 490 KB on the wire against 2.66 MB over the
worker protocol: the same bytes, zstd level 1 in one stream across all 776
paths.

The script also checks:

- classic `nix build --store grpc://…?insecure=1 --eval-store auto` prints
  the same stdout as the real nix with its nix-grpc-store plugin;
- an out-link whose output no local substituter has is fetched back with
  `FetchNars` and handed to the local daemon;
- `iets build --builder grpc://… --json` gives the same outcome per
  attribute as the worker protocol on the same store;
- a multi-root `iets build` over `grpc://` names every attribute on
  stderr.

Against `farm.numtide.com` with a Keycloak `write` token (2026-09-20): a
`runCommand` with 572 cached inputs went `Want`, `Assigned`,
`BuildDerivation` on a farm worker in 2.6 s, 5 RPCs; a classic `nix-build`
out-link then came from cache.numtide.com through the local daemon.

## NAR

The NAR iets writes is byte-identical to `nix-store --dump`, not merely
hash-identical. `tests/nar_parity.rs` checks it on a tree of unicode and
awkward names, empty files, empty directories, symlinks (dangling, and
pointing out of the tree), executables, contents on and off the 8-byte
boundary, deep nesting and a large file:

- the whole NAR matches `nix-store --dump` byte for byte;
- every subtree on its own hashes like `nix-store --dump | sha256sum`;
- a dump restored by iets dumps back to the same bytes;
- the reader agrees with the writer on the size, and rejects a truncated
  archive, a misspelled keyword and non-zero padding;
- the reader refuses a name longer than `NAME_MAX`.

Each test skips itself when `nix` is absent; `nix develop` has it.

On an AMD EPYC Genoa with a warm page cache, the pinned nixpkgs tree (40k
entries, 212 MB of NAR) and one 500 MB file (`bench/nar.nu --nix`):

| case | iets | Nix |
|---|---|---|
| NAR hash of the tree | 665-672 ms | `nix hash path` 1665 ms |
| dump of the tree, no hash | 545-567 ms | `nix-store --dump \| sha256sum` 2034 ms |
| NAR hash of a 500 MB file | 353-359 ms | `nix hash path` 386 ms |
| restore of the tree from a pipe | 1744-1804 ms | `nix-store --restore` 2019 ms |

The tracing cache keeps the NAR hash of each copied tree and dumps the
tree again only when its metadata changed (`docs/explanation/caching.md`).
