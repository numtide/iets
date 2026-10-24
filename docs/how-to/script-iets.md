# Drive iets from a script or an agent

Scraping a build log for the word "error" is no way to run a pipeline. iets
prints JSON, exits with codes you can branch on, and can write everything it
says on stderr as records a program can read. This page shows each of those,
and the plumbing operations you can pipe together.

The example files come from other pages. `hello.nix` and `greeting.nix` are
in [Get started with iets](../tutorial/getting-started.md), `demo.nix` is in
[Replace your Nix tools with iets](replace-nix-tools.md), and `src.nix` is in
[Fix a wrong fixed-output hash](fix-hashes.md). All but `demo.nix` read
`<nixpkgs>` or `builtins.currentSystem`, so their commands add `--impure`.

The record shapes are in
[Errors, exit codes and log records](../reference/errors.md), and the contract
of every plumbing operation is in
[Plumbing operations](../reference/plumbing.md).

## JSON output

`--json` is on `eval`, `build`, `diff`, `search`, `prefetch`,
`print-dev-env`, `parse` and `cache paths`:

```sh
iets eval --impure --strict --json -I nixpkgs=$nixpkgs hello.nix
```

```json
{"drv":"/nix/store/2bki…-hello-2.12.3.drv","name":"hello-2.12.3"}
```

```sh
iets diff --json -A a -A b demo.nix
```

```json
{"a":"/nix/store/623k…-demo.drv","b":"/nix/store/sx7z…-demo.drv",
 "changes":[{"a":"\"-c\"\n\"echo one > $out\"\n","b":"\"-c\"\n\"echo two > $out\"\n","key":"","what":"args"},
            {"a":"hello","b":"goodbye","key":"GREETING","what":"env"}],
 "equal":false}
```

`iets eval-jobs` writes JSON lines by default, one object per job.

## Exit codes

Save this as `dep.nix`. It depends on a derivation that always fails:

```nix
let
  bad = derivation {
    name = "bad-dep"; system = "x86_64-linux"; builder = "/bin/sh";
    args = [ "-c" "exit 1" ];
  };
in derivation {
  name = "needs-bad"; system = "x86_64-linux"; builder = "/bin/sh";
  args = [ "-c" "echo ${bad} > $out" ];
}
```

```sh
iets build --builder daemon dep.nix; echo $?
```

```
100
```

The code is 0 for success, 1 for any error, 2 for a usage error of a native
command, and 100, 101, 102 or 104 when a build failed, timed out, had a hash
mismatch or wasn't deterministic. Several failures combine their bits, so a
timeout next to a failed build gives 101. Real Nix gives these codes only
when it builds in its own store. As a client of a daemon it exits 1, and
iets reads the status of each build so you get the code either way. A build
that fails on a builder that reports only a generic failure (status 9) exits
1.

Three commands differ. `iets diff` follows `diff(1)`: 0 for equal, 1 for
different, 2 for anything else. Classic mode exits 1 on an error, as Nix
does. `iets eval-jobs` exits 0 when only jobs fail, and their errors are in
the job lines. [Errors, exit codes and log records](../reference/errors.md)
has the full table.

## Log records

`--log-format internal-json` turns every stderr line into `@nix ` followed by
one JSON object. It's the format of Nix's own JSON logger, so
`nix-output-monitor` reads it unchanged:

```sh
iets eval --log-format internal-json -E '{a=1;}.b'
```

```
@nix {"action":"start","id":10831246095548416,"level":4,"parent":0,"sid":"01a1225c-a53a-7873-8635-0fc0dd34ee39","text":"evaluating the expression","type":0}
@nix {"action":"msg","code":"missing-attr","column":1,"file":"«string»:1:1","level":0,"line":1,"msg":"error: attribute 'b' missing\n       at «string»:1:1","raw_msg":"attribute 'b' missing","sid":"01a1225c-a53a-7873-8635-0fc0dd34ee39"}
@nix {"action":"stop","id":10831246095548416,"sid":"01a1225c-a53a-7873-8635-0fc0dd34ee39"}
```

`start` and `stop` bracket an activity, as in Nix. The `msg` record is the
error.

These keys exist only in iets, and they're the ones to build on:

- `code`: a stable error code. Branch on it, since the message text changes.
- `hint`: what to do about it, there only when iets is sure. A hash
  mismatch names the hash the build got. A `flake.lock` entry without a
  `narHash` names the input and the `nix flake update` that locks it.
- `sid`: one session id on every record of one run. Set it yourself with
  `IETS_SESSION_ID` to match a run with your own logs.

`raw_msg` is the message alone, and `msg` is the whole text iets would have
printed. `level` is 0 for an error and 1 for a warning. With `--show-trace`
you also get a `trace` array, innermost first.

## Log destinations

Say you want the records in a file but still want to watch the run.
`json-log-path` writes every record to a file or a unix socket, without the
`@nix ` prefix, whatever `--log-format` says. Stderr stays as it was:

```sh
iets eval --json-log-path ./log.json -E '{a=1;}.b'
cat log.json
```

```json
{"action":"start","id":10831301930123264,"level":4,"parent":0,"sid":"01a1225c-a547-7d12-82bf-d706defe431e","text":"evaluating the expression","type":0}
{"action":"msg","code":"missing-attr","column":1,"file":"«string»:1:1","level":0,"line":1,"msg":"error: attribute 'b' missing\n       at «string»:1:1","raw_msg":"attribute 'b' missing","sid":"01a1225c-a547-7d12-82bf-d706defe431e"}
{"action":"stop","id":10831301930123264,"sid":"01a1225c-a547-7d12-82bf-d706defe431e"}
```

`--option json-log-path PATH` and the `nix.conf` key of the same name do the
same. iets appends to a file and creates it with mode 0644. If the path can't
be opened, you get one `error (ignored)` line on stderr and the run goes on.
As in Nix, two processes that write one file can mix their records.

## Non-interactive runs

Without a terminal, two behaviors change. Set them yourself instead of
trusting detection. The first is `fix-hashes`:

```sh
iets --iets-option fix-hashes always build --impure --builder daemon src.nix
```

It defaults to `ask`, which asks only on a terminal. Anywhere else, a failing
hash is reported and nothing is fixed. `always` fixes without asking
([Fix a wrong fixed-output hash](fix-hashes.md)).

The second is color in `iets search`. Nix writes its escape sequences to
stdout even when stdout isn't a terminal, and iets does the same, byte for
byte. Use `--json` when a program reads the output.

## Plumbing chains

`iets plumbing` is the layer under the everyday commands. Each operation
takes JSON and gives JSON, with nothing else on stdout. The operations are
`resolve`, `plan`, `missing`, `build` and `fetch`.

An operation that takes roots accepts `--roots -` and reads the stdout of the
one before it, so you can pipe them:

```sh
iets plumbing resolve --impure greeting.nix
```

```json
{"attrPaths":[],"roots":[{"attr":null,"drv":"/nix/store/6jq7…-greeting.drv","output":null}]}
```

```sh
iets plumbing resolve --impure greeting.nix | iets plumbing plan --roots -
```

```json
{"drvs":["/nix/store/6jq7…-greeting.drv"],"sources":[],"storeDrvs":1}
```

```sh
iets plumbing resolve --impure greeting.nix | iets plumbing build --builder daemon --roots -
```

```json
{"outcomes":[{"attr":"greeting.drv","drv":"/nix/store/6jq7…-greeting.drv",
              "logTail":[],
              "outputs":{"out":"/nix/store/hl2d…-greeting"},
              "reason":null,"success":true}]}
```

`missing` takes the same pipe and prints what `iets build --dry-run --json`
prints. Add `--check-cache-status` for a per-root verdict.

When you pipe, the next operation reads the closure from `/nix/store` by
default. That works only for a derivation that's already there. To hand over
one that isn't, dump it:

```sh
iets plumbing resolve --dump-drvs dd demo.nix -A a
```

```json
{"attrPaths":[],"drvDir":"dd","roots":[{"attr":"a","drv":"/nix/store/623k…-demo.drv","output":null}]}
```

The `drvDir` key travels with the roots, so the next operation reads the
derivations from that directory. `--drvs DIR` sets it by hand.

`iets plumbing --help` lists every operation, and
`iets help reference/plumbing` prints the reference page.
