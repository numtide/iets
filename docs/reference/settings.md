# Settings

Every `IETS_*` environment variable, every `iets.toml` setting, the
`--iets-option` flag, and how iets reads Nix's own `nix.conf`.

iets reads two settings files. `nix.conf` is Nix's, shared with the real
nix behind the classic fallback; `iets.toml` is iets's own, in TOML, and
no nix ever sees it.

iets reads every `IETS_*` variable, the `iets.toml` files and the
`--iets-option` flag once, at startup. A change to the environment during
a run has no effect.

## Environment variables

A variable with a **File** entry is also an `iets.toml` key, under the
kebab-case name given. The variable wins over the file: the file is the
configuration, the environment is the override. The perf-tuning knobs
(`IETS_SPIN`, `IETS_SPINNERS`, `IETS_HANDOFF`, `IETS_FANOUT`) and the
statistics knobs stay environment-only.

The **Type** column is the variable's. In the file the same setting has its
TOML type: `cache = false`, `no-fallback = true`, `workers = 4`,
`cache-dir = "/var/cache/iets"`.

A Boolean variable takes a nix.conf Boolean: `true`, `yes` or `1`, and
`false`, `no` or `0`. Any other text is an error that names the variable,
and so is a value of a number knob that is not a number.

A level knob (`IETS_STATS`, `IETS_PAR_STATS`) is off when unset, `2`
selects level 2, and any other value selects level 1.

The evaluation counters — thunks, calls, derivations, compile time, in an
`IETS_STATS` line and in the `--timings` derivation count — are per fiber.
A fiber adds its own to one run total whenever it parks or finishes its
task, so the printed numbers cover the helper fibers, and the derivation
count of a workload is the same at one worker and at four. Only a fiber
running at that instant is not in the sum yet.

Each worker owns an arena, so the arena figures are summed the same way, on
every fiber switch. Reporting one arena made memory look like it fell as
workers were added, when it rises: on nixos-desktop the sum is ~630 MB at
one worker and ~645 MB at four, because the string table is per arena and
the same text is interned once per worker. `rss_kb` is the process, not an
arena, and was always the whole run.

| Variable | File | Type | Default | Effect |
|:---|:---|:---|:---|:---|
| `IETS_WORKERS` | `workers` | usize ≥ 1 | cores, capped at 4 | Worker threads. `--workers N` overrides it. |
| `IETS_FIBER_STACK_MB` | `fiber-stack-mb` | usize (MiB) | 256 | Stack of every fiber. The reservation is `MAP_NORESERVE`, and workers × stack may not pass 64 GiB. |
| `IETS_SPIN` | | u32 | 2000 | Scans an idle searcher makes before it parks (one scan is 64 polls). |
| `IETS_SPINNERS` | | usize | workers / 4, at least 1 | Idle workers that may search for work at once. |
| `IETS_HANDOFF` | | usize | 8 | Queued tasks per awake worker that justify waking a sleeper. |
| `IETS_FANOUT` | | u32 bitmask | 7 | Strict-demand fan-out: 1 deep walks, 2 coercions, 4 list builtins. |
| `IETS_PAR_STATS` | | level | 0 | 1 prints the scheduler summary, 2 adds the sample timeline and the file table. |
| `IETS_STATS` | | level | 0 | 1 prints allocation counts and bytes per object kind, RSS and mimalloc's `mi_stats_print`; 2 adds the memory census — histograms, duplicates, objects never read — on a binary built with `--features stats2`. The hooks are compiled out otherwise. Each worker allocates from its own arena, so `arena_bytes`, `arena_mapped` and the per-kind table are the sum over every arena, folded on each fiber switch. Setting this is what turns the fold on, at ~0.1% of instructions. `store_adds` counts the source trees and `toFile` texts the evaluation turned into a store path; `IETS_EVENTS=store` names each one. |
| `IETS_EVENTS` | | group list | | The parts of the run to watch, comma-separated, or `all` for every cold group: `store`, `fetch`, `cache`, … A name that is not a group is an error. `docs/reference/events.md` has the table and the schema. |
| `IETS_EVENTS_TO` | | path | `-` | Where those records go. `-` is stderr, a unix socket is connected to, any other path is appended to. It acts only when `IETS_EVENTS` names a group. |
| `IETS_PROFILE` | | `eval` or `alloc` | | Collects a sampled flame graph of the evaluation: `eval` weighs a sample by the time it stood for, `alloc` by the arena bytes since the sample before it. Any other word is an error. A profile is read per fiber, so it pins the run to one worker and refuses an explicit `--workers` or `workers` that is not 1 (`docs/reference/profile.md`). |
| `IETS_PROFILE_TO` | | path | `-` | Where the folded lines go. `-` is stderr, any other path is written. It acts only with `IETS_PROFILE`. |
| `IETS_PROFILE_HZ` | | number | 99 | Samples a second the tick thread asks for. |
| `IETS_CACHE` | `cache` | Boolean | on | The tracing cache and the IR cache. Same as `--no-cache`. |
| `IETS_CACHE_DIR` | `cache-dir` | path | `$XDG_CACHE_HOME/iets` | Cache root. The three stores live in its `trace/`, `ir/` and `inputs/` subdirectories, so the directory you name is never itself a removal target. A store subdirectory that already exists, holds something, and is not iets's is refused with an error, not wiped. `--cache-dir DIR` overrides it. |
| `NIX_SSL_CERT_FILE`, `SSL_CERT_FILE` | | path | unset | CA bundle for the fetcher and `grpc://`. The first one set wins, even when empty, as in Nix. An unreadable file, or one with no certificate, is an error. Unset: the platform store, else the Mozilla roots compiled into iets. |
| `IETS_IR_STATS` | | set = on | off | One line per compiled file on stderr. |
| `IETS_IR_MIN` | | usize (bytes) | 1024 | Sources smaller than this compile faster than their cache entry loads, so they are not cached. |
| `IETS_PROVENANCE` | `provenance` | Boolean | on | Rewrites store paths and positions back to local trees (`docs/explanation/provenance.md`). |
| `IETS_NIX_BIN_DIR` | `nix-bin-dir` | path | unset | Where the real Nix binaries live. Unset searches `PATH`, skipping every iets. |
| `IETS_NO_FALLBACK` | `no-fallback` | Boolean | off | A classic command fails instead of running the real Nix (`docs/reference/classic.md`). |
| `IETS_FETCHING` | | set = on | off | Set on the Nix subprocess that fetches a flake input, so a recursive fetch is an error, not a loop. |
| `IETS_OFFLINE` | | set = on | off | Any download is an error. A flake whose inputs the input store already holds still evaluates. |
| `IETS_SESSION_ID` | | string | a new UUIDv7 | The `sid` of every JSON log record (`docs/reference/errors.md`). `eval-jobs` sets it on its workers. A settings error carries it too. |
| | `fix-hashes` | `ask`, `always`, `never` | `ask` | See **fix-hashes** below. |

`IETS_BUILD_ID` is not a setting: it is fixed when iets is built, and it
keys the caches to the binary.

### The measurement scripts

The statistics knobs are read by these; each one is run from the
repository root, in the dev shell, with `NP` the nixpkgs tree to measure
against.

| Script | What it does |
|:---|:---|
| `bench/perf.nu` | wall, user, sys and peak RSS for one binary |
| `bench/ab.nu` | the same, comparing two binaries; `--runs N`, `--skip-outpaths` |
| `bench/nar.nu` | the NAR writer, reader and hashes; `--nix` adds the real-nix reference rows |
| `bench/eval-jobs.nu [rounds]` | `iets eval-jobs` against `nix-eval-jobs` on `nixos/release-small.nix`, 4 workers, cold and warm cache runs interleaved; `--no-parity` skips the `--force-recurse` job-set comparison |
| `bench/cache-growth.nu` | evaluates `bench/workloads/nixos-desktop.nix` at two nixpkgs revisions and reports what the second adds to each cache |

The two standard workloads:

```
iets eval --impure --strict --no-tracing-cache --workers 1 -I nixpkgs=$NP bench/workloads/nixos-desktop.nix
iets eval-jobs --impure --no-instantiate --workers 1 --chunks 8 --chunk 0   # ci/eval/outpaths.nix
```

Both workloads reach nixpkgs through `<nixpkgs>`, which a pure evaluation
has not got, so they are run with `--impure`. That is what they always
measured: the native default was impure until the project root arrived
(`docs/explanation/flakes-and-sources.md`).

### fix-hashes

`ask` asks `[y/n/a]` for each fix when stdin and stderr are terminals and
the log format is not `internal-json`; `a` takes this fix and the rest of
the run. Elsewhere `ask` only prints the hint. `always` fixes without
asking, for CI and agents. `never` never fixes.

With a fix, `iets build` rewrites the hash of a fixed-output build that
failed on it and builds again. The old hash must appear once, in any
spelling, in the local files evaluation parsed; store paths are never
edited. A derivation with inputs is built as a resolved copy of itself,
whose mismatch the daemon reports as a line of its own; that one is fixed
too. It stops when a hash it wrote mismatches again (a build that is not
reproducible).

Only the daemon reports a mismatch, and only about a fixed-output
derivation of the closure the run is building, whose output hash is the
hash the message names. A build log, a `builtins.trace` and every other
text that holds the same words are ignored: a derivation must not be able
to make iets rewrite a hash in a file of the user. Each fix is a
`hash-fixed` record
(`docs/reference/errors.md`). After a mismatch, one more evaluation
without the tracing cache lists the files. Files read with `readFile` or
`importJSON` are not searched.

## `--iets-option`

`--iets-option NAME VALUE` sets any `iets.toml` setting for one run. Every
iets command and every classic name accepts it, and it never reaches the
real nix: it leaves the argv before a classic fallback execs.

VALUE is a TOML value when it reads as a boolean, an integer, a quoted
string or an array, and the text itself otherwise: `--iets-option workers
4`, `--iets-option fix-hashes always` and `--iets-option builder
ssh-ng://buildhost` need no quotes. Text for `source-map` is a
whitespace-separated list.

A command that starts a second iets passes the pairs on: `eval-jobs`
puts them on the argv of every worker process, so a setting the collector
was given holds in the workers too.

# `iets.toml`

The format is TOML: the
settings are top-level keys with the names below, and `[lint]` is a table
(`docs/reference/lints.md`).

```toml
builder = "ssh-ng://buildhost"
workers = 4
no-fallback = true
fix-hashes = "never"
source-map = ["/nix/store/…-source=/home/me/nixpkgs"]

[lint]
path-copy = "warn"
```

The keys are flat because there are thirteen of them and because the names
stay the ones `--iets-option` takes. A value has its
TOML type: a switch is `true` or `false`, a number is an integer, a path or
a URL is a string, `source-map` is an array of strings. A wrong type is an
error naming the key and the file; `workers = "4"` is not read as 4.

Nothing but iets reads this file, so a user can send iets's builds
somewhere without touching `nix.conf`, which the real nix behind the
classic fallback also reads.

There is no machine file. iets is a client: where it builds and which
token it uses are the user's. A machine file held a path into one user's
home directory for every account, root included
(`docs/explanation/history.md`).

## Files and order

Later wins.

1. `$XDG_CONFIG_HOME/iets/iets.toml`, default
   `~/.config/iets/iets.toml`: the user.
2. `$IETS_CONFIG`, treated as one more file's text, so it is TOML too:
   `IETS_CONFIG='builder = "daemon"'`.
3. The `--iets-option NAME VALUE` pairs from the command line.
4. The `IETS_*` variable of a setting that has one.

A later source **replaces** a key. `source-map` is the exception: its
entries **add up** across the sources, lowest first, so a command maps a
checkout without repeating what the file maps. There is no way to drop an
entry set lower down; a stale one costs a warning that its directory is
gone.

A file that is missing is skipped; one that is there and cannot be read is
an error. An **unknown key is an error** naming the file: this file is
iets's, so a name it does not know is a mistake, not a setting for
somebody else.

A `[lint]` table in the user file sets lint levels for every
evaluation, below the project's (`docs/reference/lints.md`). `$IETS_CONFIG`
and `--iets-option` refuse it: `IETS_LINT` and `--lint` are their
spelling.

## The project file, and why it holds only `[lint]`

A second `iets.toml` is the project's: the nearest one found by walking up
from the entry file or the flake directory (`docs/reference/lints.md`). **The
walk stops at the project root** (`docs/explanation/flakes-and-sources.md`),
and it looks in that directory before it stops. It must not leave
the project: a `/tmp/iets.toml`, which every user may write, would otherwise
set a lint level for every evaluation below it. The
user file is trusted, because whoever can write it already controls the
account. The project file is not: it arrives with
whatever repository was cloned, and running `iets` in a clone must not be
a way to run its author's code or to send data to its author. So the
project file is an allow-list of one entry, the `[lint]` table, which only
selects diagnostics. Any other key in it is an **error** that names the key
and the two files where it may be set. It is not a warning: a repository
that tries to set `nix-bin-dir` is worth stopping on.

| Key | What a repository could do with it |
|:---|:---|
| `nix-bin-dir` | name the binary the classic fallback execs: arbitrary code |
| `builder` | receive the closure, sources and all, and hand back outputs of its choice |
| `pure` | `false` gives the evaluation of a clone `getEnv`, a read of any absolute path and a fetch with no hash: read a private key, then post it as the path of a URL |
| `fix-hashes` | `always` rewrites source files and accepts whatever hash the network served, with no question asked |
| `cache-dir` | point the caches at a directory it ships, so a planted entry answers for an evaluation; or write the caches anywhere |
| `source-map` | make errors, provenance and lint roots name local directories outside the clone |
| `fetch-max-size` | lift the cap that stops a decompression bomb |
| `no-fallback` | `false` brings back the real nix on a machine whose admin turned it off |
| `shell-gc-root`, `build-gc-root` | pin closures on the disk, or let them go: the disk is the user's to budget |
| `workers`, `fiber-stack-mb` | take every core, or reserve address space until the run fails |
| `cache`, `provenance` | nothing worse than a slow run or plainer output, and nothing a project needs: the default is no |

A project file that is one of the two settings files (an evaluation started
below `~/.config/iets`) is read as the settings file it is.

## Before 2026-09

These files were `iets.conf`, in nix.conf's line syntax. That file is not read
any more. On NixOS the module below writes `iets.toml`, and the old file
leaves `/etc` at the same switch, so nothing checks for it.

## Keys

Every key in the **File** column above, with the type and default given
there, plus six keys that have no environment variable:

| Key | Type | Default |
|:---|:---|:---|
| `builder` | one URL, or a list of them, or a list of `{url, systems, features}` tables: `daemon`, `unix://PATH`, `ssh-ng://[user@]host`, `grpc://host:port`; not used by the shell commands (`docs/reference/classic.md`) | the nix.conf `store` setting (itself `auto`) |
| `source-map` | array of `"STOREPATH=LOCALDIR"` strings | empty |
| `fetch-max-size` | integer (bytes) | 17179869184 (16 GiB) |
| `shell-gc-root` | bool | `true` |
| `build-gc-root` | bool | `true` |
| `pure` | bool | `true` |

`pure` is `pure-eval` for the native commands, and `false` brings the old
default back for every run. The classic names do not read it: each one
keeps Nix's own rule, so one command line answers the same whichever
binary `PATH` found. `--impure` is the per-run form. **`pure` is a user
setting alone**: see the project file below.

`shell-gc-root` is the switch on the roots `nix-shell` and `nix develop`
keep for themselves: on, a shell entry links what it realised in
`~/.cache/iets/roots`, so the next `nix collect-garbage` leaves it and the
next entry rebuilds nothing (`docs/explanation/caching.md`). Off, no root is
made and no link is touched, which is the switch to take on a small disk: a
dev shell's closure is large, and a root nobody asked for still pins it for
the 30 days of the roots TTL.

`build-gc-root` is the same switch for the outputs of a build: `iets
build` roots every path it prints, because the caller reads those paths and
uses them after the run, and `nix-build --no-out-link` and `nix build
--no-link` keep a root in place of the symlink they were told not to make.
The second one is a divergence from Nix, which lets that output go
(`docs/reference/divergences.md`), so `build-gc-root = false` is how to get
Nix's behaviour back.

Neither switch touches the roots a caller names: `nix-instantiate
--add-root`, `nix-build -o`, `eval-jobs --gc-roots-dir`, `plumbing build
--gc-root`, `print-dev-env --gc-root` and `iets build --fetch-to`. One
root per thing.

`source-map` and `builder` are the lists. `fetch-max-size` caps
the decompressed bytes of one flake input download, together with what a
sparse member of the archive expands to; a stream that
decompresses over 1000 times its size past its first 16 MiB is refused
too.

iets has no local store of its own, so `auto` and an empty value both
mean the local daemon.

`builder` is where derivations are realised. Every command takes it:
`iets build --builder` and `iets print-dev-env --builder` win over it,
and so does a classic `--store` or `--option store`. `nix-build`, `nix build`, `nix-shell` and `nix develop`
have no flag of their own and use it as it stands. Registration is not
affected: drvs and sources for `nix-instantiate --add-root`, its GC roots
and the `nix-build`/`nix build` out-links always go to the local daemon,
and a remote `builder`'s outputs are copied back first
(`docs/reference/classic.md`).

`builder` may name several places. Each one declares what it accepts, and
a derivation goes to the first that accepts it. Order is the policy;
there is no cost model.

```toml
builder = [
  { url = "daemon", systems = ["x86_64-linux"] },
  { url = "ssh-ng://buildhost", systems = ["aarch64-linux"], features = ["kvm"] },
]
```

An undeclared `systems` or `features` accepts anything, so one entry that
declares neither takes every derivation, which is what a single `builder`
has always done: the store itself refuses what it cannot build, and it is
the only one that knows. A derivation no place accepts is an error that
names every place and what it declared, before anything is uploaded.

`--builder` carries one string, so it separates places with `;`
(`--builder 'daemon; ssh-ng://buildhost'`), as nix.conf's `builders` separates
machines. A URL's own query string uses `&`, never `;`. A classic name
addresses one store, as the tool it stands in for does, so it takes the
first place and warns about the rest.

# `nix.conf`

iets reads Nix's settings files as Nix 2.34.8 does:
`globals.cc:loadConfFile` for discovery, `configuration.cc:parseConfigFiles`
for the line syntax.

## Files and order

iets applies these in order. Later wins.

1. `$NIX_CONF_DIR/nix.conf`, default `/etc/nix/nix.conf`.
2. The user files, lowest precedence first. `loadConfFile` walks the list
   backwards (`globals.cc:143`), so the **first** entry of the list below
   has the highest precedence:
   - `$NIX_USER_CONF_FILES`, colon-separated, replaces the whole list;
     else
   - `$NIX_CONFIG_HOME/nix.conf` or `$XDG_CONFIG_HOME/nix/nix.conf`
     (default `~/.config/nix/nix.conf`), then
   - `<dir>/nix/nix.conf` for each `$XDG_CONFIG_DIRS` entry (default
     `/etc/xdg`).
3. `$NIX_CONFIG`, treated as one more file's text.
4. The `--option name value` pairs from the command line.

A file that is missing or unreadable is skipped, not an error.

## Line syntax

- `#` starts a comment; the rest of the line is dropped.
- `name = value`. The value is the rest of the line, with runs of
  whitespace collapsed to one space.
- `include path` and `!include path`, relative to the including file. A
  missing `include` is an error (`file '...' included from '...' not
  found`); a missing `!include` is silent.
- `extra-<name> = value` appends to a list setting. For a setting that is
  not a list, the pair is kept under its literal name, as Nix keeps it in
  `unknownSettings`.
- An unknown setting is kept. Nix warns
  about it, and so does iets, with the same text (**What is applied**,
  class 5).

## Values

A Boolean is `true`/`yes`/`1` or `false`/`no`/`0`. A list is
whitespace-separated. The defaults are Nix's: `store` is `auto`, `system`
is the build target, `max-jobs` is `auto` (the CPU count) and `cores` is
`0` (all).

`$NIX_PATH`, even an empty one, replaces the `nix-path` setting of every
file, as `configuration.cc:192` does.

Nix parses a value where it is set, so a value it refuses stops the run
with exit 1. iets gives the same text at the same place:

| setting | values | text |
| --- | --- | --- |
| a Boolean | `true`/`yes`/`1`, `false`/`no`/`0` | `Boolean setting 'NAME' has invalid value 'V'` |
| `sandbox` | those, and `relaxed` | `option 'sandbox' has invalid value 'V'` |
| `max-jobs` | an integer, and `auto` | `configuration setting 'max-jobs' should be 'auto' or an integer` |
| an integer | digits, with a `K`, `M`, `G` or `T` suffix for 2^10, 2^20, 2^30 or 2^40 | `setting 'NAME' has invalid value 'V'` |

Nix holds some integer settings in 32 bits, so a value above 4294967295
is an error for them:

- `build-poll-interval`
- `cores`
- `download-attempts`
- `download-speed`
- `eval-attrset-update-layer-rhs-threshold`
- `eval-profiler-frequency`
- `id-count`
- `max-call-depth`
- `max-jobs`
- `max-substitution-jobs`
- `narinfo-cache-meta-ttl`
- `narinfo-cache-negative-ttl`
- `narinfo-cache-positive-ttl`
- `start-id`
- `tarball-ttl`

## What is applied

Nix has about 130 settings. For iets each one is in one of five classes.

**1. iets reads it.**

| Setting | Effect |
|:---|:---|
| `store` | the last fallback for `builder` |
| `system` | `builtins.currentSystem` under the classic names; the `system=` default of a `grpc://` URL. iets's own commands use the system iets was built for |
| `pure-eval` | see **`pure-eval`** below |
| `nix-path` | the search path after `-I`, when `NIX_PATH` is not in the environment. `--option nix-path` replaces `NIX_PATH`, and `--option extra-nix-path` adds to it, as in Nix (`configuration.cc:192`). Nix's channel defaults are not applied |
| `show-trace` | the same as `--show-trace` |
| `json-log-path` | `docs/reference/errors.md` |
| `access-tokens`, `connect-timeout`, `stalled-download-timeout` | the flake input fetcher. The timeouts default to 15 s and 300 s; 0 is no limit, as in `filetransfer.hh`. `--option` reaches the fetcher too: the session keeps the settings of the first load |
| `bash-prompt`, `bash-prompt-prefix`, `bash-prompt-suffix` | `nix develop` |
| `experimental-features` | every feature is always on (`docs/reference/divergences.md`); `parse-toml-timestamps` needs the flag |

An old name is its setting: `build-cores` is `cores`, `binary-caches` is
`substituters`, and so on for the 23 aliases of Nix 2.34.8.

**2. It goes to the daemon.** `max-jobs`, `cores`, `keep-going`,
`keep-failed`, `substitute`, `fallback` and `max-silent-time` go as the
fields of the daemon's `SetOptions` operation. The daemon takes these
fields from its client, so the last two must be the client's values: a client that sends 0
turns the administrator's `max-silent-time` off.

Every other setting that acts in the store goes into the override map of
`SetOptions`, as `remote-store.cc:setOptions` sends it: `substituters`,
`trusted-public-keys`, `sandbox`, `timeout`, `builders`, `post-build-hook`
and the rest. Only a value from a user file, from `$NIX_CONFIG` or from
`--option` goes there. The system `nix.conf` does not, because the daemon
reads that file itself (`globals.cc:loadConfFile`, `resetOverridden`). The
daemon applies an override from a trusted user, and a short list of them
from any user; for the rest it prints `warning: ignoring the
client-specified setting 'NAME'`, as it does for a Nix client. `store` is
not sent: it names the daemon. A list setting that only got `extra-` values
goes as `extra-NAME`, which the daemon appends to its own value.

The classic names do this. iets's own commands (`iets
eval`, `build`, `eval-jobs`, `print-dev-env`, `diff`) send
Nix's defaults and no overrides. There, a store setting given with
`--option` is one warning:

```
warning: setting 'sandbox' is ignored: iets's own commands send a daemon no settings; the classic names do
```

A `grpc://` builder has no `SetOptions`. For it the classic names give the
same warning, with the reason `a grpc:// builder
takes no settings; its URL has 'max-builds'`. `store` and `system` still
apply.

**3. It holds by construction.** No message.

| Setting | Why it holds |
|:---|:---|
| `allow-import-from-derivation`, `trace-import-from-derivation` | iets never builds during evaluation. An import from derivation is an error in iets's own commands, and a fallback under a classic name, where the real nix applies the setting. |
| `eval-cache` | iets has no Nix evaluation cache. Its own caches check every input they read, and `--no-cache` turns them off. |
| `warn-dirty`, `allow-dirty-locks`, `commit-lock-file-summary` | iets prints no dirty-tree warning and writes no lock file. |
| `flake-registry`, `use-registries` | iets has no registry. A reference that needs one falls back, or is an error in iets's own commands. |
| `nix-shell-shebang-arguments-relative-to-script`, `nix-shell-always-looks-for-shell-nix` | a `#!` script falls back, and so does `nix-shell` when the second one is `false`. |
| `eval-profile-file`, `eval-profiler-frequency` | they only act with `eval-profiler`, which is in class 4; `IETS_PROFILE_TO` and `IETS_PROFILE_HZ` are the knobs that act. |
| `eval-attrset-update-layer-rhs-threshold`, `preallocate-contents`, `use-case-hack` | they tune Nix's own data structures and file writes. No result changes. |

**4. iets has nothing for it.** It counts when the value
is not Nix's default, from any file or from `--option`. Then
`nix-instantiate`, `nix-build`, `nix-shell` and the `nix` subcommands fall
back to the real nix, which applies it (`iets: falling back to nix-build:
the setting 'restrict-eval' is not implemented`). Two settings are no reason
to fall back and give the warning below instead: `accept-flake-config`,
because a flake seldom has a `nixConfig`, and `log-lines`, which changes no
result. The commands that cannot fall back — iets's own and
`nix-eval-jobs` — give one warning per setting and process, with the
reason:

```
warning: setting 'restrict-eval' is ignored: iets has no restricted mode, so every path and URI stays readable
```

| Setting | Nix's default | The reason in the warning |
|:---|:---|:---|
| `restrict-eval`, `allowed-uris` | `false`, empty | iets has no restricted mode, so every path and URI stays readable |
| `eval-system` | empty | iets does not read it; `builtins.currentSystem` is `system`, or the system iets was built for |
| `max-call-depth` | 10000 | the call depth limit is fixed at 10000 |
| `accept-flake-config` | `false` | iets never applies a flake's `nixConfig` (`docs/reference/divergences.md`) |
| `allow-dirty` | `true` | iets evaluates a dirty Git tree as it is |
| `impure-env` | empty | `builtins.getEnv` reads the process environment alone |
| `abort-on-warn` | `false` | `builtins.warn` never stops an evaluation |
| `trace-verbose` | `false` | `builtins.traceVerbose` never prints |
| `trace-function-calls` | `false` | iets has no function-call trace |
| `eval-profiler` | `disabled` | iets has an evaluation profiler, under `IETS_PROFILE` (`docs/reference/profile.md`). This setting is not wired to it: it arrives through nix.conf, which no evaluator-wide knob reads yet. |
| `debugger-on-trace`, `debugger-on-warn`, `ignore-try` | `false` | iets has no debugger |
| `allow-unsafe-native-code-during-evaluation` | `false` | iets has no `builtins.exec` and no `builtins.importNative` |
| `plugin-files` | empty | iets loads no plugins |
| `lint-absolute-path-literals`, `lint-short-path-literals`, `lint-url-literals`, `warn-short-path-literals` | `ignore`, `false` | iets does not read it; `iets eval` has the `--lint` flags |
| `log-lines` | 25 | iets prints the last 200 lines of a failed build |
| `tarball-ttl`, `trust-tarballs-from-git-forges` | 3600, `true` | iets's fetcher has no such switch |

The record has the code `ignored-setting` (`docs/reference/errors.md`).
Nix prints nothing for these, so the warning is a divergence on stderr
(`docs/reference/divergences.md`).

**5. Nix does not know it.** `warning: unknown setting 'NAME'`, Nix's own
text (configuration.cc:`warnUnknownSettings`), once per name, with the code
`unknown-setting`. The value is kept, and it never reaches a daemon: the override map takes the settings of Nix
2.34.8 only. So a `nix.conf` written for a fork of Nix, with settings of
its own, gives these warnings and changes no result. Such a setting as a
flag (`--NAME`) is an unknown flag: a usage error, or a fallback under a
classic name.

Two limits. A setting of the system `nix.conf` that is both a client and a
store setting (`netrc-file`, `ssl-cert-file`, `http2`, the `download-*`
group) acts in the daemon only: iets's own fetcher does not read it. Use
`NIX_SSL_CERT_FILE` for the CA bundle. The
fetcher still retries a transient HTTP status the way Nix does, with
Nix's defaults: five attempts, from 250 ms, doubled each time, and from a minute for a 429. A
404, a 410 and an authorisation failure are final. And
`extra-platforms`, `system-features` and the other store settings change
what the *daemon* builds, not what iets evaluates.

# `pure-eval`

`pure-eval` as Nix 2.34.8 has it (`eval-settings.hh`, `eval.cc`,
`primops.cc`, `fetchTree.cc`, `flake-primops.cc`). An evaluation in this
mode depends only on its locked inputs. The answers and the error
messages are Nix's.

## When it is on

The same rules as Nix, per command:

| Command | Default | Changed by |
|:---|:---|:---|
| `nix-instantiate`, `nix-build`, `nix-shell` | nix.conf `pure-eval`, else off | `--pure-eval`, `--no-pure-eval`, `--impure`, `--option pure-eval`; the last one wins |
| `nix eval`, `nix build`, `nix develop`, `nix print-dev-env` | on (main.cc); nix.conf does not change it | the same flags. `-f`/`--file` turns it off, and is a usage error when `pure-eval` was **overridden** — set by a flag, a user `nix.conf`, `$NIX_CONFIG` or `--option`, whatever the value; the system `$NIX_CONF_DIR/nix.conf` does not count, because `loadConfFile` clears the bit after it (installables.cc:460). A flag that turns purity off takes `-f` instead of refusing it. iets falls back on the error |
| `nix-eval-jobs` | `--flake`: on. Otherwise nix.conf, else off | `--impure` beats `--flake`; without `--flake`, `--pure-eval` and `--option pure-eval` |
| the native commands (`iets eval`, `iets build`, …) | **on**, and the `pure` key in the user `iets.toml` changes it | `--pure-eval`, `--no-pure-eval`, `--impure`, `--option pure-eval`; `eval-jobs --flake` as nix-eval-jobs. nix.conf is read by `eval-jobs` only |

`nix-shell --impure` is the shell's environment flag (nix-build.cc takes
it), not `pure-eval`.

## What it changes

- `builtins.currentSystem` and `builtins.currentTime` do not exist
  (`impureOnly` constants): `builtins ? currentSystem` is false and
  `__currentSystem` is an undefined variable.
- `builtins.getEnv` returns `""`.
- The search path is empty, `-I` and `NIX_PATH` included, so
  `builtins.nixPath` is `[ ]` and `<x>` fails with `cannot look up '<x>'
  in pure evaluation mode`. `<nix/fetchurl.nix>` still works.
- A file is readable only under a store path this evaluation allowed: one
  it copied (`"${./x}"` of an allowed path, `builtins.path`,
  `filterSource`), a `toFile` text, the flake's own tree and its inputs,
  or a tree a fetch produced (`fetchTree`, `fetchTarball`). Everything
  else is `access to absolute path '...' is forbidden in pure evaluation
  mode (use '--impure' to override)`: `import` (the file on the command
  line too), `readFile`, `readDir`, `readFileType`, `hashFile`,
  `findFile` entries, string coercion and `builtins.path` of a local
  path. `pathExists` answers `false` instead. A `.drv` this evaluation
  wrote is not readable either, but `import drv.drvPath` works: Nix reads
  it through the store, not the accessor.
- `builtins.storePath` is refused.
- `fetchTarball` needs a `sha256`; `fetchTree` refuses an unlocked input
  (no `narHash` for `path` and `tarball`, no `rev` for the rest);
  `getFlake` refuses an unlocked reference, and prints `at «none»:0`
  because Nix does.

`builtins.path` with a `sha256` whose store path already exists allows it
without reading the source, as `allowAndSetStorePathString` does.

## What stays unsupported

Classic mode falls back to the real nix on these, so the answer is still
Nix's:

- an unlocked `git` or `hg` input: Nix names it with query parameters
  (`?shallow=1`) that iets does not reproduce;
- `getFlake` on a reference that may be locked, one with a `rev` or a
  `narHash`;
- `restrict-eval`, which is a separate setting: it has no effect, and
  gives one `ignored-setting` warning (**What is applied**, class 4).

## Caches

`pure-eval` is part of the tracing-cache key. The IR cache gives the
right answer in both modes without it (`docs/explanation/caching.md`).
