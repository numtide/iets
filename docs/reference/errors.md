# Errors, exit codes and log records

The stable error codes, the exit codes, the `--log-format internal-json`
record shape, the activity records and the `json-log-path` setting.

Every error goes through one reporter (`src/log.rs`). It prints Nix's
plain text, or with `--log-format internal-json` one `@nix` JSON record.
It also sets the exit code. The `iets` command sends it to stderr; a
library host sets its own sink, or none, on its `Session`
(docs/reference/library.md). Humans read the text. Programs read the
record and the exit code. Agents read the `hint`.

## Error codes

Each error has a stable kebab-case `code`. The code comes from the error
kind: `ErrorKind` in `src/eval/error.rs` for evaluation errors, `Error`
in `src/error.rs` for the rest, and the daemon's build status for builds.

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
| `exec` | `ErrorKind::Exec` | a program `builtins.exec` ran exited non-zero, died of a signal or did not start |
| `hash-fixed` | `fix-hashes` | a level-3 record: the hash `fix-hashes` wrote, at the file, line and column it changed, with the byte span and the new text |
| `ignored-flag` | `src/ignored.rs` | a level-1 record: a flag that has no effect, and why (docs/reference/divergences.md) |
| `ignored-setting` | `src/config.rs` | a level-1 record: a Nix setting that has no effect, and why (docs/reference/settings.md) |
| `unknown-setting` | `src/config.rs` | a level-1 record: a setting Nix 2.34.8 does not know either, with Nix's text |
| `unknown-lint` | `src/lint/mod.rs` | a level-1 record: a lint name in an `iets.toml` or in `IETS_LINT` that the registry does not have (docs/reference/lints.md) |
| `cache-invalidated` | `src/trace/run.rs` | a level-3 record: what changed under a tracing cache entry, so the run evaluates again |
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
| `no-results` | `iets search` | no package matched every regex (docs/reference/commands.md) |
| `no-output` | `iets build` | a root put no store path on stdout: nothing to build, no build result, or no such output (docs/reference/commands.md) |
| `build-failed` | build status | a build that failed |

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success. |
| 1 | Any error. Also a build failure that has no code of its own. |
| 2 | A usage error of a native command (`iets eval --bogus`). Under a classic name the code is 1, as in Nix. |
| 1, 2 | `iets diff` alone follows `diff(1)`: 0 the two are equal, 1 they differ, 2 a usage error or any other failure (docs/reference/commands.md). |
| 100 | A build failed (`PermanentFailure`, `InputRejected`, `DependencyFailed`). |
| 101 | A build timed out. |
| 102 | A fixed-output build had a hash mismatch. |
| 104 | A build was not deterministic. |

The build codes follow `ExitStatusFlags::failingExitStatus` in Nix's
`src/libstore/build-result.cc`. The value is `0b1100000` plus one bit
per kind of failure: 4 build failed, 1 timeout, 2 hash mismatch, 8 not
deterministic. Several failures combine their bits: a timeout and a
failed build give 101. `iets build` uses them. Nix computes them only
when it builds in its own store; as a client of a daemon it exits 1,
because the worker protocol does not carry the exit status. iets is
always a client, but it reads the status of each `BuildResult`. A root
whose dependency failed reports `DependencyFailed`, so it exits 100, and
102 as well when the daemon reported a hash mismatch in the same run,
because Nix's flags combine over all its goals. The
classic names (`nix-build`, `nix build`, `nix-shell`) exit 1, as Nix
does through a daemon. `iets eval-jobs` exits 0 when only jobs fail;
their errors are in the job lines.

A NAR hash mismatch of a fetched tree has code `hash-mismatch` but exits
1, as the same error does in Nix.

## `--log-format internal-json`

The native commands (`eval`, `build`, `diff`, `eval-jobs`,
`print-dev-env`) and the classic names take `--log-format FORMAT` and
`--show-trace`. `internal-json` is the format; `raw`, `raw-with-logs`,
`bar` and `bar-with-logs` give the plain text. Any other value is a usage
error with Nix's message.

Every stderr line is then `@nix ` and one JSON object, the format of
`JSONLogger` in Nix's `src/libutil/logging.cc`. The keys are sorted, as
Nix sorts them. An error is `logEI`'s record plus `code` and, when iets
knows the fix, `hint`:

```json
{"action":"msg","code":"missing-attr","column":12,"file":"«string»:1:12",
 "level":0,"line":1,"msg":"error: attribute 'b' missing\n       at «string»:1:12",
 "raw_msg":"attribute 'b' missing"}
```

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

A lint warning is a level-1 record with code `lint`; its `raw_msg` starts
with `[NAME]`. A failed build is a record with code `build-failed` or
`hash-mismatch`: `raw_msg` is the daemon's message, and the position is
where the derivation was defined.

A tracing cache entry that no longer holds is a level-3 record with code
`cache-invalidated`. The text is `re-evaluating: RAW`, and `raw_msg` names
the one observation that changed:

```json
{"action":"msg","code":"cache-invalidated","column":null,"file":null,"level":3,
 "line":null,"msg":"re-evaluating: /home/u/repo/flake.nix changed",
 "raw_msg":"/home/u/repo/flake.nix changed"}
```

The level is 3, not 1: nothing went wrong. An edit invalidates the entry of
every command that read the file, so the line is part of a normal run, as
`hash-fixed` is. Under a classic name the record goes to `json-log-path`
only and stderr stays as Nix leaves it (`docs/reference/divergences.md`).
Only a real invalidation prints it — an entry existed, was
validated, and something had changed. A run with no entry prints nothing,
because it invalidated nothing.

The `raw_msg` is one observation of docs/explanation/caching.md's table, and
says what about it changed: `PATH changed` (contents), `PATH is gone`,
`PATH cannot be read`, `PATH is now a directory, not a regular file`,
`PATH now exists`, `directory PATH has different entries`, `source tree PATH
changed`, `source tree PATH has another modification time` (a `path:` input
with the same bytes and another `lastModified`),
`PATH resolves to a different path`, `environment variable $NAME
changed` (or `is now set`, `is no longer set`), `the SCM working tree in DIR
changed` (SCM is the name of the source control system, `git`),
``the output of `CMD` in DIR changed``, or `input store entry NAME
is gone`. The read set has one order, and the lowest-indexed observation
that fails is the one reported, so one change always names the same thing.

The file or flake a command names is in the read set like every other file
the run reads, and not in the key (docs/explanation/caching.md), so editing
it is an ordinary invalidation and the line names it.

Any other line is `{"action":"msg","level":N,"msg":TEXT}`: level 0 for
`error:` and `trace:` lines, 1 for warnings, 3 for the rest (build
progress, summaries, classic fallback notes). The lines that `--timings`
and `IETS_STATS` add stay plain text.

### Activity records

A command that talks to a store also puts out Nix's activity records,
the ones `nix-output-monitor` (`nom --json`) reads. These are `startActivity`,
`stopActivity` and `result` of `JSONLogger` in `src/libutil/logging.cc`:

```json
{"action":"start","fields":["/nix/store/…-hello.drv","",1,1],"id":…,"level":3,"parent":0,"text":"building '/nix/store/…-hello.drv'","type":105}
{"action":"result","fields":["hello"],"id":…,"type":101}
{"action":"stop","id":…}
```

The type numbers are `ActivityType` and `ResultType` of
`src/libutil/include/nix/util/logging.hh`: activities 100 copy path, 101
file transfer, 102 realise, 103 copy paths, 104 builds, 105 build, 108
substitute, 109 query path info; results 101 build log line, 104 set
phase, 105 progress, 106 set expected.

- **From the daemon.** A worker-protocol daemon (`daemon`, `unix://`,
  `ssh-ng://`) sends its activities during a build or a substitution.
  iets forwards each one, as `worker-protocol-connection.cc:processStderr`
  does. Each forwarded activity gets an id of this process, and its
  `parent` follows.
- **iets's own uploads.** The derivations and sources iets sends with
  `AddMultipleToStore`, and a remote builder's outputs copied back to the
  local daemon, are one `copying N paths` activity (type 103) with
  progress results and one `copying path` activity (type 100) per path,
  as `nix copy` shows them (`store-api.cc:copyPaths`).
- **`grpc://`.** There is no daemon stream, so iets makes the records of
  `build/worker.cc` itself. They are the realise, builds and copy-paths
  activities around a batch, and one `building 'DRV' on 'URL'` activity
  per derivation. The build's log lines and phases are its results. The
  builds activity gets progress results `[done, expected, running,
  failed]`.

An id is Nix's form: the pid in the high 32 bits and a counter below.
Records go out only under `internal-json` or with a `json-log-path`.
Under `internal-json` the build renderer leaves the builds to these
records: it puts out no start, phase, log or verdict lines. The failed
build's error record, notes and daemon messages outside a build still go
out. The text output does not change.

## A finding and its edit

Two records name a place to edit. They have one shape: the keys of a
message, plus `start`, `end` and `replacement`.

| Record | `start` and `end` | `replacement` |
|---|---|---|
| a lint finding, code `lint`, at level 1 (`warn`) or 0 (`error`) | the site of the finding | the text that corrects the finding; only `url-literal` and `short-path-literal` have one |
| `hash-fixed`, at level 3 | the hash in the file, before the edit | the hash that `fix-hashes` wrote |

A lint finding is a proposal: iets does not change the file. A
`hash-fixed` record is a report: iets made the edit.

```json
{"action":"msg","code":"lint","column":17,"end":30,
 "file":"/repo/default.nix:1:17","level":1,"line":1,
 "msg":"warning: [short-path-literal] relative path literal 'data/hello.txt' should be prefixed with '.' for clarity: './data/hello.txt'\n  at /repo/default.nix:1:17",
 "raw_msg":"[short-path-literal] relative path literal 'data/hello.txt' should be prefixed with '.' for clarity: './data/hello.txt'",
 "replacement":"./data/hello.txt","start":16}
```

The offsets are those of `iets parse --json` (`docs/reference/syntax-tree.md`).
Thus a program finds the node of a finding in the tree of the file.

The evaluator keeps the position where a node starts. It does not keep
the end. For the first finding in a file, iets reads the file again and
parses it. Then the span is that of the smallest node that starts at the
site and has a kind that the lint reports:

| Lint | Kind of node |
|---|---|
| `absolute-path-literal`, `short-path-literal` | `path` |
| `url-literal` | `string` |
| `eq-type-mismatch`, `eq-function` | `binary` with `==` or `!=` |
| `or-non-attrs` | `select` with a `default` |
| `impure-builtin` | `apply`; if there is none, `select`; if there is none, `var` |
| `unhashed-fetch`, `ifd`, `read-through-copy` | `apply` |
| `rev-count` | a `name` of an `inherit`; if there is none, `apply` (`builtins.getAttr`); if there is none, `select` |
| `unknown-allow` | no node: the word in the comment |
| all other lints | a `bind` or a `name`; if there is none, a `string` with an interpolation, a `binary` with `+`, or an `apply` |

Limits:

- A finding in an expression that is not in a file (`-E`, `--arg`) has
  no `start` and no `end`.
- Two nodes of the same kind can start at the same offset: `(a == b) ==
  c`, or `"${a}" + b` for a lint that reports a copy. The record then has
  the span of the smaller node.
- A site without a node of those kinds has no `start` and no `end`.
- A finding of `rev-count` has no site when the read is not a selection
  (`docs/reference/lints.md`, "The tree that a value comes from"). The
  record then has no `start` and no `end`, and `file`, `line` and `column`
  are null. `raw_msg` names the tree.

## The build plan: `iets build --dry-run`

`iets build --dry-run` registers the closure, as `--no-build` does, and
asks the daemon what a build would do (`QueryMissing`,
`remote-store.cc:queryMissing`). It builds nothing. It prints what
`nix build --dry-run` prints (`printMissing` in `src/libmain/shared.cc`),
at level 0:

```
these 2 derivations will be built:
  /nix/store/…-dep.drv
  /nix/store/…-top.drv
this path will be fetched (1.5 MiB download, 5.9 MiB unpacked):
  /nix/store/…-figlet-2.2.5
don't know how to build these paths:
  /nix/store/…
```

The derivations come references first. The fetched paths are sorted by
name. The sizes are Nix's `renderSize`: at least KiB, one decimal. A
`grpc://` builder has no `QueryMissing`, so `--dry-run` is a usage error
there.

With `--json`, stdout also gets the plan as one JSON object. This is an
iets extension: `nix build --dry-run --json` prints the installables'
derived paths, not the plan.

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

The names are the fields of Nix's `MissingPaths`.

`iets plumbing missing` prints the same object, and
`iets plumbing build` the outcomes of the same dispatch, from derivation
paths instead of an installable (docs/reference/plumbing.md). Both, and
`iets build`, go through one call, `api::build::Dispatch`.

### `sid`: the session

Every record carries `sid`, activity records included, on stderr and in the
`json-log-path` file. It lets a program that reads several streams group
the records of one run.
This is an iets extension: Nix 2.34.8 has no such field. The key and the
default value have the shape Determinate Nix uses (`sid`, a UUIDv7).

- By default each session makes a new UUIDv7, for example
  `019e037b-f603-7443-8071-cfd8585ec8c6`. It starts with the time in
  milliseconds, so ids sort by start time.
- `IETS_SESSION_ID=ID` sets it. Use this to give several invocations one
  session.
- `eval-jobs` starts its worker processes with `IETS_SESSION_ID` set to its
  own id, so the workers' records have the id of the run that started them.

## `json-log-path`

The `json-log-path` setting (nix.conf, `--option json-log-path PATH` or
`--json-log-path PATH`) names a file or a unix socket. Every record above,
activity records too, also goes there, without the `@nix ` prefix, whatever `--log-format` says;
stderr does not change. A file is opened for append and created with mode
0644. A path that does not open gives one `error (ignored): opening log
file` line, and the run goes on. This is `applyJSONLogger` in Nix's
`src/libutil/logging.cc`. As in Nix, two processes that write the same file
can interleave their records.

### Hints

A hint is there only when it is certain:

- A hash mismatch of a fixed-output build names the hash the build got.
- A NAR hash mismatch of a fetched tree names the hash of the tree and,
  for a `fetchTree` call, the position of its `narHash` attribute.
- A usage error repeats the text's own `did you mean` and `--help` lines.
- A `flake.lock` entry without a `narHash` names the input by its path
  from the root (`a/nixpkgs`) and the `nix flake update` that locks it
  again.
- A `git` input whose tree is not the one the lock names (nix reports a
  NAR hash mismatch, fetchers.cc:`Input::checkLocks`) has code
  `hash-mismatch`, and the hint names the usual causes: a `.gitattributes`
  filter such as git-lfs, `export-subst`, or a submodule.
- A file that is in the working tree of a git flake but that git does not
  track is Nix's error (git.cc:`makeNotAllowedError`, `Path 'F' in the
  repository "R" is not tracked by Git.`), and the hint is the
  `git -C "R" add "F"` that makes it visible.
- A missing `builtins.exec`, `exec` or `__exec` (code `missing-attr` or
  `undefined-var`) when the setting is off names it: `builtins.exec needs
  '--option allow-unsafe-native-code-during-evaluation true'`. The text
  prints it on the line after the error, where Nix prints its own
  suggestion.

Any other missing attribute or undefined variable has no hint: iets has no
did-you-mean suggestion for them yet.
