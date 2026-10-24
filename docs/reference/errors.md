# Errors, exit codes and log records

When something goes wrong, iets tells you in text and a program in JSON.
This page has the error codes, the exit codes, the
`--log-format internal-json` records, the activity records and the
`json-log-path` setting.

This is a missing attribute as a program sees it with
`--log-format internal-json`, without the evaluation activity records
around it:

```json
{"action":"msg","code":"missing-attr","column":12,"file":"«string»:1:12",
 "level":0,"line":1,"msg":"error: attribute 'b' missing\n       at «string»:1:12",
 "raw_msg":"attribute 'b' missing","sid":"019e037b-f603-7443-8071-cfd8585ec8c6"}
```

Without the flag, you get Nix's plain text on stderr. Either way, iets sets
the exit code. Programs read the record and the exit code, and agents read
the `hint` when there is one. A library host chooses its own sink, or none,
on its `Session` (`docs/reference/library.md`).

## Error codes

Each error has a stable kebab-case `code`, which comes from the error's
kind. The Kind column is what a library host matches on: a variant of
`eval::ErrorKind` for evaluation errors, of `error::Error` for the rest
(`docs/reference/library.md`), or the daemon's build status for builds. A
dash means the code has no kind of its own.

| Code | Kind | What went wrong |
| --- | --- | --- |
| `eval` | `ErrorKind::Eval` | an evaluation error with no more specific kind |
| `type` | `ErrorKind::Type` | a value of the wrong type |
| `throw` | `ErrorKind::Throw` | `builtins.throw` |
| `assert` | `ErrorKind::Assert` | a failed `assert` |
| `abort` | `ErrorKind::Abort` | `builtins.abort` |
| `undefined-var` | `ErrorKind::UndefinedVar` | a variable that is not in scope |
| `missing-attr` | `ErrorKind::MissingAttr` | an attribute that is not there |
| `infinite-recursion` | `ErrorKind::InfiniteRecursion` | a value that depends on itself |
| `stack-overflow` | `ErrorKind::StackOverflow` | the evaluator ran out of stack |
| `parse` | `ErrorKind::Parse` | a syntax error |
| `lint` | `ErrorKind::Lint` | a lint at level `error`; at `warn`, a level-1 record |
| `hash-mismatch` | `ErrorKind::HashMismatch`, build status | a fetched tree or a fixed-output build whose hash is not the one given |
| `hash-fixed` | — | a level-3 record: the hash `fix-hashes` wrote, at the file, line and column it changed, with the byte span and the new text |
| `ignored-flag` | — | a level-1 record: a flag that has no effect, and why (`docs/reference/divergences.md`) |
| `ignored-setting` | — | a level-1 record: a Nix setting that has no effect, and why (`docs/reference/settings.md`) |
| `unknown-setting` | — | a level-1 record: a setting Nix 2.34.8 does not know either, with Nix's text |
| `unknown-lint` | — | a level-1 record: a lint name in an `iets.toml` or in `IETS_LINT` that the registry does not have (`docs/reference/lints.md`) |
| `cache-invalidated` | — | a level-3 record: what changed under a tracing cache entry, so the run evaluates again |
| `io` | `ErrorKind::Io`, `Error::Io` | a file that cannot be read or written |
| `unsupported` | `ErrorKind::Unsupported`, `Error::Unsupported` | a feature iets does not have; classic mode falls back to Nix |
| `cancelled` | `ErrorKind::Cancelled` | a parallel helper was stopped; never reaches the user |
| `usage` | `Error::Usage` | a bad command line |
| `config` | `Error::Config` | a bad `nix.conf` or iets setting |
| `store` | `Error::Store` | a bad store path, derivation or hash |
| `flake` | `Error::Flake` | a bad flake reference or lock file |
| `protocol` | `Error::Protocol` | bad data from another process (daemon, worker) |
| `cache` | `Error::Cache` | the tracing cache |
| `plan` | `Error::Plan` | a derivation closure that cannot be built |
| `no-results` | — | `iets search`: no package matched every regex (`docs/reference/commands.md`) |
| `no-output` | — | `iets build`: a root put no store path on stdout: nothing to build, no build result, or no such output (`docs/reference/commands.md`) |
| `build-failed` | build status | a build that failed |

## Hints

You get a `hint` only when iets is sure of the fix:

- A hash mismatch of a fixed-output build names the hash the build got.
- A NAR hash mismatch of a fetched tree names the hash of the tree and,
  for a `fetchTree` call, the position of its `narHash` attribute.
- A usage error repeats the text's own `did you mean` and `--help` lines.
- A `flake.lock` entry without a `narHash` names the input by its path
  from the root (`a/nixpkgs`) and the `nix flake update` that locks it
  again.
- A `git` input whose tree is not the one the lock names has code
  `hash-mismatch` (Nix reports a NAR hash mismatch). The hint names the
  usual causes: a `.gitattributes` filter such as git-lfs, `export-subst`,
  or a submodule.
- A file in the working tree of a git flake that git does not track gives
  Nix's error (`Path 'F' in the repository "R" is not tracked by Git.`),
  and the hint is the `git -C "R" add "F"` that makes it visible.
- A `fetchTree` call on an input with no `narHash` says to pass one, or
  to run `nix flake update`.
- `iets diff --closure` on an output that is not in the store names the
  `iets build` that makes it.
- `iets search` with no results says to use fewer or shorter regexes.
- `fetchMercurial` names what works instead: a `fetchTree` with a
  `narHash`, an hg flake input, or classic mode, which falls back to Nix.

The text on stderr shows the hint only for a usage error and an
`unsupported` error. For every other error, the hint is only in the
record's `hint` field.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success. |
| 1 | Any error. Also a build failure that has no code of its own. |
| 2 | A usage error, such as `iets eval --bogus`. Classic mode uses 1, as Nix does. |
| 1, 2 | `iets diff` alone follows `diff(1)`: 0 the two are equal, 1 they differ, 2 a usage error or any other failure (`docs/reference/commands.md`). |
| 100 | A build failed (`PermanentFailure`, `InputRejected`, `DependencyFailed`). |
| 101 | A build timed out. |
| 102 | A fixed-output build had a hash mismatch. |
| 104 | A build was not deterministic. |

`iets build` uses Nix's build codes: `0b1100000` plus one bit per kind of
failure (4 build failed, 1 timeout, 2 hash mismatch, 8 not deterministic),
from `ExitStatusFlags::failingExitStatus` in Nix's
`src/libstore/build-result.cc`. Several failures add up their bits over the
whole run, so a timeout and a failed build give 101.

Nix itself exits 1 when it builds through a daemon, because the worker
protocol doesn't carry the exit status. iets reads the status of each
`BuildResult`, so it gives you the real code. Classic mode
(`nix-build`, `nix build`, `nix-shell`) exit 1, as Nix does.

Some cases differ:

- `iets eval-jobs` exits 0 when only jobs fail. Their errors are in the job
  lines.
- A NAR hash mismatch of a fetched tree has code `hash-mismatch` but exits
  1, as the same error does in Nix.

## `--log-format internal-json`

The native commands (`eval`, `build`, `diff`, `eval-jobs`,
`print-dev-env`) and classic mode take `--log-format FORMAT` and
`--show-trace`. `internal-json` gives you records. `raw`, `raw-with-logs`,
`bar` and `bar-with-logs` give the plain text. Any other value is a usage
error with Nix's message.

With `internal-json`, every stderr line is `@nix ` and one JSON object, as
Nix's `JSONLogger` writes it. The keys are sorted, as Nix sorts them. An
error record is Nix's, plus `code` and, when iets knows the fix, `hint`.
The record at the top of this page is one.

| Key | Value |
| --- | --- |
| `action` | `"msg"` |
| `level` | 0 for an error, 1 for a warning |
| `msg` | the whole text iets prints without `--log-format` |
| `raw_msg` | the message alone |
| `line`, `column` | where the error is, or `null` |
| `file` | `FILE:LINE:COL`, as Nix fills it, or `null` |
| `code` | the error code (iets only) |
| `hint` | what to do about it (iets only, only when known) |
| `start`, `end` | the place in the file as byte offsets from 0; `end` is the offset after the last byte (iets only, only when known) |
| `replacement` | the text that replaces the bytes `start` to `end - 1` (iets only, only when the record has an edit) |
| `sid` | the session: one id for every record of one run (iets only) |
| `trace` | with `--show-trace` only: `[{raw_msg, line, column, file}]`, innermost first |

A lint warning is a level-1 record with code `lint`. Its `raw_msg` starts
with `[NAME]`. A failed build is a record with code `build-failed` or
`hash-mismatch`. Its `raw_msg` is the daemon's message, and the position is
where the derivation was defined.

Any other line is `{"action":"msg","level":N,"msg":TEXT}`: level 0 for
`error:` and `trace:` lines, 1 for warnings, 3 for the rest (build
progress, summaries, classic fallback notes). The lines that `--timings`
and `IETS_STATS` add stay plain text.

### Stale cache entries

When a tracing cache entry is out of date, you get a level-3 record with
code `cache-invalidated`. The text is `re-evaluating: RAW`, and `raw_msg`
names the one thing that changed:

```json
{"action":"msg","code":"cache-invalidated","column":null,"file":null,"level":3,
 "line":null,"msg":"re-evaluating: /home/u/repo/flake.nix changed",
 "raw_msg":"/home/u/repo/flake.nix changed"}
```

The level is 3 because nothing went wrong: an edit invalidates the entry of
every command that read the file. You see the record only when an entry
existed and something had changed. In classic mode it goes to
`json-log-path` only (`docs/reference/divergences.md`).

`raw_msg` names one observation from the table in
`docs/explanation/caching.md`: `PATH changed`, `PATH is gone`,
`directory PATH has different entries`, `environment variable $NAME
changed`, `the SCM working tree in DIR changed` and so on. The same change
always gives the same line.

### Activity records

iets also writes Nix's activity records, the ones `nix-output-monitor`
(`nom --json`) reads:

```json
{"action":"start","fields":["/nix/store/…-hello.drv","",1,1],"id":…,"level":3,"parent":0,"sid":…,"text":"building '/nix/store/…-hello.drv'","type":105}
{"action":"result","fields":["hello"],"id":…,"sid":…,"type":101}
{"action":"stop","id":…,"sid":…}
```

The type numbers are Nix's `ActivityType` and `ResultType`
(`src/libutil/include/nix/util/logging.hh`):

- activities: 0 unknown, 100 copy path, 101 file transfer, 102 realise,
  103 copy paths, 104 builds, 105 build, 108 substitute, 109 query path
  info;
- results: 101 build log line, 104 set phase, 105 progress, 106 set
  expected.

A worker-protocol daemon sends its activities during a build, and iets
passes each one on with an id of this process. iets's own uploads are one
`copying N paths` activity (type 103) with a `copying path` activity (type
100) per path, as `nix copy` shows them. With `grpc://` there is no daemon
stream, so iets writes the records itself. `iets eval` and `iets build` wrap
the evaluation in one type 0 activity at level 4, `evaluating 'FILE' -A
ATTR`. `docs/reference/divergences.md` lists where they differ from Nix.

An id has Nix's form: the pid in the high 32 bits and a counter below.
These records go out only under `internal-json` or with a `json-log-path`,
and the text output doesn't change. Under `internal-json` the build renderer
leaves the builds to them, so there are no start, phase, log or verdict
lines. The failed build's error record, notes and daemon messages outside a
build still go out.

## `sid`: the session

Every record carries a `sid`, activity records too, on stderr and in the
`json-log-path` file. Use it to group the records of one run. Nix 2.34.8 has
no such field. By default each run makes a new UUIDv7, for example
`019e037b-f603-7443-8071-cfd8585ec8c6`, which sorts by start time.
`IETS_SESSION_ID=ID` sets it, and `eval-jobs` passes its own id to its
workers.

## A finding and its edit

A lint finding and a `hash-fixed` record point at a place to edit. Both
have the keys of a message, plus `start`, `end` and `replacement`.

| Record | `start` and `end` | `replacement` |
|---|---|---|
| a lint finding, code `lint`, at level 1 (`warn`) or 0 (`error`) | the site of the finding | the text that corrects the finding; only `url-literal` and `short-path-literal` have one |
| `hash-fixed`, at level 3 | the hash in the file, before the edit | the hash that `fix-hashes` wrote |

A lint finding is a proposal: iets does not change your file. A
`hash-fixed` record is a report: iets made the edit.

```json
{"action":"msg","code":"lint","column":17,"end":30,
 "file":"/repo/default.nix:1:17","level":1,"line":1,
 "msg":"warning: [short-path-literal] relative path literal 'data/hello.txt' should be prefixed with '.' for clarity: './data/hello.txt'\n  at /repo/default.nix:1:17",
 "raw_msg":"[short-path-literal] relative path literal 'data/hello.txt' should be prefixed with '.' for clarity: './data/hello.txt'",
 "replacement":"./data/hello.txt","start":16}
```

The offsets are the ones `iets parse --json` gives
(`docs/reference/syntax-tree.md`), so you can find the node of a finding in
the tree of the file. The span is the smallest node that starts at the
site and has the kind the lint reports:

| Lint | Kind of node |
|---|---|
| `absolute-path-literal`, `short-path-literal` | `path` |
| `url-literal` | `string` |
| `eq-type-mismatch`, `eq-function` | `binary` with `==` or `!=` |
| `or-non-attrs` | `select` with a `default` |
| `impure-builtin` | `apply`; if there is none, `select`; if there is none, `var` |
| `unhashed-fetch`, `ifd`, `ifd-local-output`, `read-through-copy` | `apply` |
| `rev-count` | a `name` of an `inherit`; if there is none, `apply` (`builtins.getAttr`); if there is none, `select` |
| `unknown-allow` | no node: the word in the comment |
| all other lints | a `bind` or a `name`; if there is none, a `string` with an interpolation, a `binary` with `+`, or an `apply` |

Some findings have no span:

- A finding in an expression that is not in a file (`-E`, `--arg`) has
  no `start` and no `end`.
- A site with no node of those kinds has no `start` and no `end`.
- A finding of `rev-count` has no site when the read is not a selection
  (`docs/reference/lints.md`, "The tree that a value comes from"). The
  record then has no `start` and no `end`, and `file`, `line` and `column`
  are null. `raw_msg` names the tree.

Two nodes of the same kind can start at the same offset: `(a == b) == c`,
or `"${a}" + b` for a lint that reports a copy. The record then has the
span of the smaller node.

## The build plan: `iets build --dry-run`

`iets build --dry-run` registers the closure like `--no-build`, asks the
daemon what a build would do, and builds nothing. It prints what
`nix build --dry-run` prints, at level 0:

```
these 2 derivations will be built:
  /nix/store/…-dep.drv
  /nix/store/…-top.drv
this path will be fetched (1.5 MiB download, 5.9 MiB unpacked):
  /nix/store/…-figlet-2.2.5
don't know how to build these paths:
  /nix/store/…
```

The derivations come references first, and the fetched paths are sorted by
name. A `grpc://` builder cannot answer the question, so `--dry-run` is a
usage error there.

With `--json`, stdout also gets the plan as one JSON object, where
`nix build --dry-run --json` prints the installables' derived paths:

```json
{"downloadSize":1553696,"narSize":6180152,"unknown":[],
 "willBuild":["/nix/store/…-dep.drv","/nix/store/…-top.drv"],
 "willSubstitute":["/nix/store/…-figlet-2.2.5"]}
```

| Key | Value |
| --- | --- |
| `willBuild` | the derivations to build, in the order of the text |
| `willSubstitute` | the paths to fetch from a substituter, in the order of the text |
| `unknown` | the paths nothing can build or fetch |
| `downloadSize` | bytes to download for `willSubstitute` |
| `narSize` | bytes of those paths unpacked |

The names are the fields of Nix's `MissingPaths`. `iets plumbing missing`
prints the same object from derivation paths instead of an installable, and
`iets plumbing build` prints the outcomes of the same build
(`docs/reference/plumbing.md`).

## `json-log-path`

The `json-log-path` setting names a file or a unix socket. Set it in
nix.conf, with `--option json-log-path PATH`, or with
`--json-log-path PATH`. Every record on this page goes there too, activity
records included, without the `@nix ` prefix, whatever `--log-format`
says. stderr does not change.

A file is opened for append and created with mode 0644. If the path does
not open, you get one `error (ignored): opening log file` line, and the run
goes on. This is how Nix's `applyJSONLogger` behaves. As in Nix, two
processes that write the same file can interleave their records.
