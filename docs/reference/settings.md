# Settings

Where iets takes its settings from: `IETS_*` environment variables,
`iets.toml`, the `--iets-option` flag, and Nix's own `nix.conf`.

iets reads two settings files: `nix.conf` is Nix's, and the real nix behind
the classic fallback reads it too. `iets.toml` is iets's own, in TOML, and
no nix ever sees it. iets reads every `IETS_*` variable, the `iets.toml`
files and the `--iets-option` flag once, at startup.

## Environment variables

A variable with a **File** entry is also an `iets.toml` key, under the
kebab-case name given. The variable wins over the file: the file is your
configuration, the environment is the override. The tuning knobs
(`IETS_SPIN`, `IETS_SPINNERS`, `IETS_HANDOFF`, `IETS_FANOUT`) and the
statistics knobs exist only as variables.

In the file, a setting has its TOML type: `cache = false`, `workers = 4`,
`cache-dir = "/var/cache/iets"`.

A Boolean variable takes a nix.conf Boolean: `true`, `yes` or `1`, and
`false`, `no` or `0`. Any other text is an error that names the variable.
So is a number knob whose value is not a number.

A level knob (`IETS_STATS`, `IETS_PAR_STATS`) is off when unset. `2`
selects level 2, and any other value selects level 1. The counters it
prints cover every worker.

| Variable | File | Type | Default | Effect |
|:---|:---|:---|:---|:---|
| `IETS_WORKERS` | `workers` | usize ≥ 1 | cores, capped at 4 | Worker threads. `--workers N` overrides it. |
| `IETS_FIBER_STACK_MB` | `fiber-stack-mb` | usize (MiB) | 256 | Stack of every fiber. The reservation is `MAP_NORESERVE`, and workers × stack may not pass 64 GiB. |
| `NIX_STORE_DIR` | | path | `/nix/store`, or `%PROGRAMDATA%\nix\store` on Windows | The store directory, which is an input of every store path hash. `NIX_STORE` is the older name. It must be absolute, and a relative value is ignored with a warning. On Windows, `/nix/store` gives the same store paths as Linux. |
| `IETS_SPIN` | | u32 | 2000 | Scans an idle searcher makes before it parks (one scan is 64 polls). |
| `IETS_SPINNERS` | | usize | workers / 4, at least 1 | Idle workers that may search for work at once. |
| `IETS_HANDOFF` | | usize | 8 | Queued tasks per awake worker that justify waking a sleeper. |
| `IETS_FANOUT` | | u32 bitmask | 7 | Strict-demand fan-out: 1 deep walks, 2 coercions, 4 list builtins. |
| `IETS_PAR_STATS` | | level | 0 | 1 prints the scheduler summary, 2 adds the sample timeline and the file table. |
| `IETS_STATS` | | level | 0 | 1 prints allocation counts and bytes per object kind, RSS and mimalloc's `mi_stats_print`; 2 adds the memory census — histograms, duplicates, objects never read — on a binary built with `--features stats2`. The hooks are compiled out otherwise. The counters are the sum over every worker's arena. Setting this turns the fold on, at about 0.1 % of instructions. |
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
| `IETS_NIX_BIN_DIR` | `nix-bin-dir` | path | unset | Where the real Nix binaries live. Unset: the first one on `PATH` that is not iets, where an iets classic mode command stands for the Nix it was built with (`docs/reference/classic.md`, **Fallback rules**). |
| `IETS_NO_FALLBACK` | `no-fallback` | Boolean | off | A classic command fails instead of running the real Nix (`docs/reference/classic.md`). |
| `IETS_FETCHING` | | set = on | off | Set on the Nix subprocess that fetches a flake input, so a recursive fetch is an error. |
| `IETS_OFFLINE` | | set = on | off | Any download is an error. A flake whose inputs the input store already holds still evaluates. |
| `IETS_COMPLETE_MS` | | u64 (ms) | 5000 | How long a completion that evaluates (`FLAKEREF#fragment`, `-A`) may take before it gives no candidates. `0` waits. A value that is not a number keeps the default. |
| `IETS_PROGRESS` | | Boolean | on | The status line on a terminal and the `⏵` lines on a pipe (`docs/reference/commands.md`). Off draws neither, so stderr does not depend on how long a run takes. The log lines and the `internal-json` records stay. |
| `IETS_SESSION_ID` | | string | a new UUIDv7 | The `sid` of every JSON log record (`docs/reference/errors.md`). `eval-jobs` sets it on its workers. A settings error carries it too. |
| | `fix-hashes` | `ask`, `always`, `never` | `ask` | See **fix-hashes**. |

`IETS_BUILD_ID` is not a setting. It is fixed when iets is built, and it
ties the caches to the binary. `ARCHITECTURE.md`, under "Benchmarks", lists the
benchmark scripts that read the statistics knobs.

### fix-hashes

- `ask` asks `[y/n/a]` for each fix when stdin and stderr are terminals and
  the log format is not `internal-json`. `a` takes this fix and every fix
  after it in the run. Anywhere else, `ask` behaves like `never`.
- `always` fixes without asking, for CI and agents.
- `never` never fixes.

When it may fix, `iets build` rewrites the hash of a fixed-output build
that failed on it, and builds again. The old hash must appear once, in any
spelling, in the local files the evaluation parsed. Store paths and files
read with `readFile` or `importJSON` are never edited. iets stops when a
hash it wrote mismatches again, because then the build is not reproducible.

Only a mismatch the daemon reports counts, and only for a fixed-output
derivation of the closure the run is building. A build log, a
`builtins.trace` or any other text with the same words is ignored, so a
derivation can't make iets rewrite a hash in your files. Each fix is a
`hash-fixed` record (`docs/reference/errors.md`), and
`docs/how-to/fix-hashes.md` walks through one.

## `--iets-option`

`--iets-option NAME VALUE` sets any `iets.toml` setting for one run. Every
iets command takes it, in classic mode too, and it never reaches the
real nix: iets removes it from the argv before a classic fallback runs.

VALUE is a TOML value when it reads as a boolean, an integer, a quoted
string or an array, and plain text otherwise. So these need no quotes:

```sh
iets build --iets-option workers 4 .#hello
iets build --iets-option fix-hashes always .#hello
iets build --iets-option builder ssh-ng://buildhost .#hello
```

Text for `source-map` is a whitespace-separated list. `eval-jobs` passes the
pairs on to every worker process.

## `iets.toml`

The settings are top-level keys with the names on this page, and `[lint]` is
a table (`docs/reference/lints.md`).

```toml
builder = "ssh-ng://buildhost"
workers = 4
no-fallback = true
fix-hashes = "never"
source-map = ["/nix/store/…-source=/home/me/nixpkgs"]

[lint]
path-copy = "warn"
```

The keys are flat, because they keep the names `--iets-option` takes. A
value has its TOML type: a switch is `true` or `false`, a number is an
integer, a path or a URL is a string, and `source-map` is an array of
strings. A wrong type is an error that names the key and the file:
`workers = "4"` is not read as 4.

Because only iets reads this file, you can send iets's builds somewhere else
without touching `nix.conf`. There is no machine-wide file: where iets builds
and which token it uses belong to each user (`docs/explanation/history.md`
says why). On NixOS, the NixOS module writes `iets.toml`.

### `iets.toml` files and order

Each source overrides the ones before it.

1. `$XDG_CONFIG_HOME/iets/iets.toml`, by default
   `~/.config/iets/iets.toml`: yours.
2. `$IETS_CONFIG`, read as the text of one more file, so it is TOML too:
   `IETS_CONFIG='builder = "daemon"'`.
3. The `--iets-option NAME VALUE` pairs from the command line.
4. The `IETS_*` variable of a setting that has one.

A later source **replaces** a key. `source-map` is the exception: its
entries **add up** across the sources, lowest first, so a command can map a
checkout without repeating what the file maps. You cannot drop an entry
set lower down. A stale one costs a warning that its directory is gone.

A missing file is skipped. A file that is there but cannot be read is an
error. An **unknown key is an error** that names the file: this file is
iets's own, so a name it does not know is a mistake.

A `[lint]` table in your user file sets lint levels for every evaluation,
below the project's (`docs/reference/lints.md`). `$IETS_CONFIG` and
`--iets-option` refuse it; use `IETS_LINT` and `--lint` there.

### The project file

A project can have its own `iets.toml`: the nearest one found by walking up
from the entry file or the flake directory (`docs/reference/lints.md`).
**The walk stops at the project root**
(`docs/explanation/flakes-and-sources.md`), after it looks in that
directory. It must not leave the project: otherwise a `/tmp/iets.toml`,
which every user may write, would set lint levels for every evaluation
below it.

Your user file is trusted, because whoever can write it already controls
your account. A project file is not. It arrives with whatever repository
you cloned, and running `iets` in a clone must never run its author's code
or send your data to its author. So a project file may hold one thing, the
`[lint]` table, which only selects diagnostics. Any other key is an
**error** that names the key and the two files where you may set it. It is
not a warning, because a repository that tries to set `nix-bin-dir` is
worth stopping for.

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

A project file that is also one of the two settings files (an evaluation
started below `~/.config/iets`) is read as the settings file it is.

### Keys

The keys are those in the **File** column, with the type and default
given there, and these, which have no environment variable:

| Key | Type | Default |
|:---|:---|:---|
| `builder` | one URL, or a list of them, or a list of `{url, systems, features}` tables: `daemon`, `unix://PATH`, `ssh-ng://[user@]host`, `grpc://host:port`; not used by the shell commands (`docs/reference/classic.md`) | the nix.conf `store` setting (itself `auto`) |
| `source-map` | array of `"STOREPATH=LOCALDIR"` strings | empty |
| `fetch-max-size` | integer (bytes) | 17179869184 (16 GiB) |
| `shell-gc-root` | bool | `true` |
| `build-gc-root` | bool | `true` |
| `pure` | bool | `true` |

**`pure`** is `pure-eval` for the native commands. Set it to `false` to
make every run impure. Classic mode doesn't read it: each command keeps Nix's own
rule, so a command line gives the same answer whichever binary `PATH` found.
`--impure` is the per-run form. `pure` is a user setting only (see the
project file).

**`shell-gc-root`** controls the roots `nix-shell` and `nix develop` keep
for themselves. When it is on, a shell links what it realised in
`~/.cache/iets/roots`, so the next `nix collect-garbage` keeps it
(`docs/explanation/caching.md`). Turn it off on a small disk: a dev shell's
closure is large, and the root pins it for the 30 days of the roots TTL.

**`build-gc-root`** is the same switch for build outputs. `iets build` roots
every path it prints, and `nix-build --no-out-link` and `nix build --no-link`
keep a root in place of the symlink they were told not to make. Nix lets
the output go (`docs/reference/divergences.md`), so `build-gc-root = false`
gives you Nix's behavior back.

Neither switch touches the roots you name yourself: `nix-instantiate
--add-root`, `nix-build -o`, `eval-jobs --gc-roots-dir`, `plumbing build
--gc-root`, `print-dev-env --gc-root` and `iets build --fetch-to`.

**`fetch-max-size`** caps the decompressed bytes of one flake input
download, together with what a sparse member of the archive expands to. A
stream that decompresses to more than 1000 times its size, past its first
16 MiB, is refused too.

**`builder`** is where derivations are built. iets has no local store of
its own, so `auto` and an empty value both mean the local daemon.
`iets build --builder`, `iets print-dev-env --builder`, a classic `--store`
and `--option store` win over it. Registration always goes to the local
daemon (`docs/reference/classic.md`, "Where the builds go").

`builder` may name several places. Each one says what it accepts, and a
derivation goes to the first place that accepts it. The order is the
policy; there is no cost model.

```toml
builder = [
  { url = "daemon", systems = ["x86_64-linux"] },
  { url = "ssh-ng://buildhost", systems = ["aarch64-linux"], features = ["kvm"] },
]
```

A place that leaves out `systems` or `features` accepts anything, as a
single `builder` does: only the store knows what it can't build. A
derivation that no place accepts is an error that names every place and what
it accepts, before anything is uploaded.

`--builder` takes one string, so it separates places with `;`
(`--builder 'daemon; ssh-ng://buildhost'`), as nix.conf's `builders`
separates machines. A URL's own query string uses `&`, never `;`. A
classic mode command talks to one store, like the tool it stands in for, so it
takes the first place and warns about the rest.

## `nix.conf`

iets reads Nix's settings files as Nix 2.34.8 does:
`globals.cc:loadConfFile` for finding them,
`configuration.cc:parseConfigFiles` for the line syntax.

### `nix.conf` files and order

iets applies these in order, and each overrides the ones before it.

1. `$NIX_CONF_DIR/nix.conf`, by default `/etc/nix/nix.conf`.
2. The user files. Nix loads this list from the end (`globals.cc:143`), so
   the **first** entry has the highest precedence:
   - `$NIX_USER_CONF_FILES`, colon-separated, replaces the whole list;
     else
   - `$NIX_CONFIG_HOME/nix.conf` or `$XDG_CONFIG_HOME/nix/nix.conf`
     (by default `~/.config/nix/nix.conf`), then
   - `<dir>/nix/nix.conf` for each `$XDG_CONFIG_DIRS` entry (by default
     `/etc/xdg`).
3. `$NIX_CONFIG`, read as the text of one more file.
4. The `--option name value` pairs from the command line.

A file that is missing or unreadable is skipped.

### Line syntax

The syntax is Nix's: `#` comments, `name = value`, `include path` (an error
when missing) and `!include path` (silent when missing), and
`extra-<name> = value` to append to a list setting. An unknown setting is
kept, and iets warns about it with Nix's text (**How each setting is
handled**, class 5).

### Values

A Boolean is `true`/`yes`/`1` or `false`/`no`/`0`. A list is
whitespace-separated. The defaults are Nix's: `store` is `auto`, `system`
is the build target, `max-jobs` is `auto` (the CPU count) and `cores` is
`0` (all).

`$NIX_PATH`, even an empty one, replaces the `nix-path` setting of every
file, as in Nix (`configuration.cc:192`).

Nix checks a value where it is set, so a value it refuses stops the run
with exit 1. iets gives the same text at the same place:

| setting | values | text |
| --- | --- | --- |
| a Boolean | `true`/`yes`/`1`, `false`/`no`/`0` | `Boolean setting 'NAME' has invalid value 'V'` |
| `sandbox` | those, and `relaxed` | `option 'sandbox' has invalid value 'V'` |
| `max-jobs` | an integer, and `auto` | `configuration setting 'max-jobs' should be 'auto' or an integer` |
| an integer | digits, with a `K`, `M`, `G` or `T` suffix for 2^10, 2^20, 2^30 or 2^40 | `setting 'NAME' has invalid value 'V'` |

Nix holds some integer settings in 32 bits (`cores`, `max-jobs`,
`max-call-depth`, `download-attempts`, the `narinfo-cache-*` TTLs and a few
more), so a value above 4294967295 is an error for them.

### How each setting is handled

Each Nix setting falls in a class for iets. Other pages refer to the numbers.

**1. iets reads it.**

| Setting | Effect |
|:---|:---|
| `store` | the last fallback for `builder` |
| `system` | `builtins.currentSystem` in classic mode; the `system=` default of a `grpc://` URL. iets's own commands use the system iets was built for |
| `pure-eval` | see **`pure-eval`** |
| `nix-path` | the search path after `-I`, when `NIX_PATH` is not in the environment. `--option nix-path` replaces `NIX_PATH`, and `--option extra-nix-path` adds to it, as in Nix (`configuration.cc:192`). Nix's channel defaults are not applied |
| `show-trace` | the same as `--show-trace` |
| `json-log-path` | `docs/reference/errors.md` |
| `access-tokens`, `connect-timeout`, `stalled-download-timeout` | the flake input fetcher. The timeouts default to 15 s and 300 s; 0 is no limit, as in `filetransfer.hh`. `--option` reaches the fetcher too: the session keeps the settings of the first load |
| `bash-prompt`, `bash-prompt-prefix`, `bash-prompt-suffix` | `nix develop` |
| `experimental-features` | in classic mode, whether `builtins` has `fetchTree`, `fetchClosure` and `outputOf`, as in Nix; iets's own commands have every feature (`docs/reference/divergences.md`). `parse-toml-timestamps` needs the flag everywhere |

An old name is its setting: `build-cores` is `cores`, `binary-caches` is
`substituters`, and so on for every alias in Nix 2.34.8.

**2. It goes to the daemon.** `max-jobs`, `cores`, `keep-going`,
`keep-failed`, `substitute`, `fallback` and `max-silent-time` go as the
fields of the daemon's `SetOptions` request. The daemon takes these fields
from its client, so the last two must be the client's values: a client that
sends 0 turns the administrator's `max-silent-time` off.

Every other setting that acts in the store goes into the override map of
`SetOptions`, as Nix's `remote-store.cc:setOptions` sends it:
`substituters`, `trusted-public-keys`, `sandbox`, `timeout`, `builders`,
`post-build-hook` and the rest. Only a value from a user file, from
`$NIX_CONFIG` or from `--option` goes there. The system `nix.conf` does
not, because the daemon reads that file itself. The daemon applies an
override from a trusted user, and a short list of them from any user. For
the rest it prints `warning: ignoring the client-specified setting 'NAME'`,
as it does for a Nix client. `store` is not sent: it names the daemon. A
list setting that only got `extra-` values goes as `extra-NAME`, which the
daemon appends to its own value.

Classic mode does this. iets's own commands (`iets eval`, `build`,
`eval-jobs`, `print-dev-env`, `diff`) send Nix's defaults and no
overrides. There, a store setting given with `--option` gives one warning:

```
warning: setting 'sandbox' is ignored: iets's own commands send a daemon no settings; classic mode does
```

A `grpc://` builder has no `SetOptions`. For it, classic mode gives
the same warning, with the reason `a grpc:// builder takes no settings;
its URL has 'max-builds'`. `store` and `system` still apply.

**3. It holds by construction.** iets says nothing.

| Setting | Why it holds |
|:---|:---|
| `allow-import-from-derivation`, `trace-import-from-derivation` | iets never builds during evaluation. It reads a derivation output that is already in the store only when `allow-import-from-derivation` is on and `trace-import-from-derivation` is off. In other cases an import from derivation is an error in iets's own commands, and a fallback in classic mode, where the real nix applies the setting. |
| `eval-cache` | iets has no Nix evaluation cache. Its own caches check every input they read, and `--no-cache` turns them off. |
| `warn-dirty`, `allow-dirty-locks`, `commit-lock-file-summary` | iets prints no dirty-tree warning and writes no lock file. |
| `flake-registry`, `use-registries` | iets has no registry. A reference that needs one falls back, or is an error in iets's own commands. |
| `nix-shell-shebang-arguments-relative-to-script`, `nix-shell-always-looks-for-shell-nix` | a `#!` script falls back, and so does `nix-shell` when the second one is `false`. |
| `eval-profile-file`, `eval-profiler-frequency` | they only act with `eval-profiler`, which is in class 4; `IETS_PROFILE_TO` and `IETS_PROFILE_HZ` are the knobs that act. |
| `eval-attrset-update-layer-rhs-threshold`, `preallocate-contents`, `use-case-hack` | they tune Nix's own data structures and file writes. No result changes. |

**4. iets has nothing for it.** This counts only when the value is not
Nix's default, from any file or from `--option`. Then `nix-instantiate`,
`nix-build`, `nix-shell` and the `nix` subcommands fall back to the real
nix, which applies it (`iets: falling back to nix-build: the setting
'restrict-eval' is not implemented`). Two settings give a warning instead
of a fallback: `accept-flake-config`, because a flake seldom has a
`nixConfig`, and `log-lines`, which changes no result. The commands that
cannot fall back, iets's own and `nix-eval-jobs`, give one warning per
setting and process, with the reason:

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
| `eval-profiler` | `disabled` | iets has an evaluation profiler, under `IETS_PROFILE` (`docs/reference/profile.md`). This setting is not wired to it: it arrives through nix.conf, which no evaluator-wide knob reads. |
| `debugger-on-trace`, `debugger-on-warn`, `ignore-try` | `false` | iets has no debugger |
| `allow-unsafe-native-code-during-evaluation` | `false` | iets has no `builtins.exec` and no `builtins.importNative` |
| `plugin-files` | empty | iets loads no plugins |
| `lint-absolute-path-literals`, `lint-short-path-literals`, `lint-url-literals`, `warn-short-path-literals` | `ignore`, `false` | iets does not read it; `iets eval` has the `--lint` flags |
| `log-lines` | 25 | iets prints the last 200 lines of a failed build |
| `tarball-ttl`, `trust-tarballs-from-git-forges` | 3600, `true` | iets's fetcher has no such switch |

The record has the code `ignored-setting` (`docs/reference/errors.md`).
Nix prints nothing for these, so the warning is a difference on stderr
(`docs/reference/divergences.md`).

**5. Nix does not know it either.** You get `warning: unknown setting 'NAME'`,
Nix's own text, once per name, with the code `unknown-setting`. The value is
kept, but it never reaches a daemon: the override map takes only the settings
of Nix 2.34.8. So a `nix.conf` written for a fork of Nix, with settings of its
own, gives these warnings and changes no result. As a flag, such a setting
(`--NAME`) is an unknown flag: a usage error, or a fallback under a classic
name.

Some settings are out of reach:

- A setting of the system `nix.conf` that is both a client and a store
  setting (`netrc-file`, `ssl-cert-file`, `http2`, the `download-*`
  group) acts in the daemon only. iets's own fetcher does not read it. Use
  `NIX_SSL_CERT_FILE` for the CA bundle. The fetcher retries a
  transient HTTP status as Nix does, with Nix's defaults: five attempts,
  from 250 ms, doubled each time, and from a minute for a 429. A 404, a
  410 and an authorisation failure are final.
- `extra-platforms`, `system-features` and the other store settings change
  what the *daemon* builds.

## `pure-eval`

`pure-eval` works as in Nix 2.34.8: the evaluation depends only on its
locked inputs, and the answers and error messages are Nix's.

### When it is on

The rules are Nix's, per command:

| Command | Default | Changed by |
|:---|:---|:---|
| `nix-instantiate`, `nix-build`, `nix-shell` | nix.conf `pure-eval`, else off | `--pure-eval`, `--no-pure-eval`, `--impure`, `--option pure-eval`; the last one wins |
| `nix eval`, `nix build`, `nix develop`, `nix print-dev-env` | on (main.cc); nix.conf does not change it | the same flags. `-f`/`--file` turns it off, and is a usage error when `pure-eval` was **overridden** — set by a flag, a user `nix.conf`, `$NIX_CONFIG` or `--option`, whatever the value; the system `$NIX_CONF_DIR/nix.conf` does not count, because `loadConfFile` clears the bit after it (installables.cc:460). A flag that turns purity off takes `-f` instead of refusing it. iets falls back on the error |
| `nix-eval-jobs` | `--flake`: on. Otherwise nix.conf, else off | `--impure` beats `--flake`; without `--flake`, `--pure-eval` and `--option pure-eval` |
| the native commands (`iets eval`, `iets build`, …) | **on**, and the `pure` key in the user `iets.toml` changes it | `--pure-eval`, `--no-pure-eval`, `--impure`, `--option pure-eval`; `eval-jobs --flake` as nix-eval-jobs. nix.conf is read by `eval-jobs` only |

`nix-shell --impure` is the shell's environment flag and has nothing to do
with `pure-eval`.

### What it changes

- `builtins.currentSystem` and `builtins.currentTime` do not exist:
  `builtins ? currentSystem` is false, and `__currentSystem` is an
  undefined variable.
- `builtins.getEnv` returns `""`.
- The search path is empty, `-I` and `NIX_PATH` included, so
  `builtins.nixPath` is `[ ]` and `<x>` fails with `cannot look up '<x>'
  in pure evaluation mode`. `<nix/fetchurl.nix>` still works.
- A file is readable only under a store path this evaluation allowed: one it
  copied (`"${./x}"` of an allowed path, `builtins.path`, `filterSource`), a
  `toFile` text, the flake's own tree and its inputs, or what a fetch
  produced (`fetchTree`, `fetchTarball`, `fetchurl`). Everything else is
  `access to absolute path '...' is forbidden in pure evaluation mode (use
  '--impure' to override)`: `import` (the file on the command line too), `readFile`,
  `readDir`, `readFileType`, `hashFile`, `findFile` entries, string coercion
  and `builtins.path` of a local path. `pathExists` answers `false` instead. A
  `.drv` this evaluation wrote is not readable either, but `import
  drv.drvPath` works: Nix reads it through the store.
- `builtins.storePath` is refused.
- `fetchTarball` and `fetchurl` need a `sha256`. `fetchTree` refuses an
  unlocked input (no `narHash` for `path` and `tarball`, no `rev` for the rest).
  `getFlake` refuses an unlocked reference, and prints `at «none»:0`
  because Nix does.

### What stays unsupported

Classic mode falls back to the real nix on these, so the answer is Nix's:

- an unlocked `git` or `hg` input: Nix names it with query parameters
  (`?shallow=1`) that iets does not reproduce;
- `getFlake` on a reference that may be locked, one with a `rev` or a
  `narHash`;
- `restrict-eval`, which is a separate setting. It has no effect, and gives
  one `ignored-setting` warning (**How each setting is handled**, class 4).

### Caches

`pure-eval` is part of the tracing-cache key. The IR cache gives the right
answer in both modes without it (`docs/explanation/caching.md`).
