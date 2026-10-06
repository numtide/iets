#!/usr/bin/env nu
# Runs nix-eval-jobs and iets eval-jobs with every supported flag
# combination on the small jobsets in tests/evaljobs/ and diffs the sorted
# output: `jq -S` per line, sorted by attrPath, error text replaced by
# `true`. Also compares the exit codes and, with --gc-roots-dir, the roots.
#
# usage: scripts/evaljobs-flags.nu          # NEJ=~/tmp/nej/bin/nix-eval-jobs
use lib.nu [ROOT diff-head iets-bin isolate nixpkgs require-real-nix run-in show]

# `jq -S` per line, error text replaced by true, sorted.
def norm []: string -> list<string> {
  let out = ($in | ^jq -S -c 'del(.error) + (if has("error") then {error: true} else {} end)')
  $out | lines | sort
}

def --wrapped nej [ctx: record, ...args: string] {
  ^($ctx.nej) --extra-experimental-features "nix-command flakes" ...$args
}

def gc-roots [dir: string]: nothing -> list<string> { ls $dir | get name | path basename | sort }

# iets evaluates a file purely and nix-eval-jobs follows nix.conf
# (docs/reference/settings.md), so the iets side of a file case gets
# `--impure` and the two stay comparable. `--flake` means pure in both.
def impure-for [args: list<string>]: nothing -> list<string> {
  if ($args | any {|a| $a in ["--flake" "--impure"] }) { [] } else { ["--impure"] }
}

# check NAME ARGS: both tools get ARGS; GCROOTS in ARGS is replaced by
# a directory per tool whose entries are compared too.
def check [ctx: record, name: string, args: list<string>]: nothing -> bool {
  let work = $ctx.work
  let nargs = ($args | each { str replace -a GCROOTS $"($work)/($name).nej-gc" })
  let iargs = ($args | each { str replace -a GCROOTS $"($work)/($name).iets-gc" })
  mkdir $"($work)/($name).nej-gc" $"($work)/($name).iets-gc"
  let stdin = (open --raw ($env.STDIN? | default "/dev/null"))
  let n = ($stdin | nej $ctx ...$nargs | complete)
  let i = ($stdin | ^($ctx.iets) eval-jobs ...(impure-for $args) ...$iargs | complete)
  let nn = ($n.stdout | norm)
  let ii = ($i.stdout | norm)
  let nr = (gc-roots $"($work)/($name).nej-gc")
  let ir = (gc-roots $"($work)/($name).iets-gc")
  if $n.exit_code == $i.exit_code and $nn == $ii and $nr == $ir {
    print $"ok   ($name) \(($nn | length) lines, exit ($n.exit_code)\)"
    true
  } else {
    print $"FAIL ($name) \(exit nej=($n.exit_code) iets=($i.exit_code)\)"
    $nn | str join "\n" | save -f $"($work)/($name).nej.norm"
    $ii | str join "\n" | save -f $"($work)/($name).iets.norm"
    $nr | str join "\n" | save -f $"($work)/($name).nej.roots"
    $ir | str join "\n" | save -f $"($work)/($name).iets.roots"
    diff-head $"($work)/($name).nej.norm" $"($work)/($name).iets.norm" ($env.SHOW? | default 6 | into int) | show
    diff-head $"($work)/($name).nej.roots" $"($work)/($name).iets.roots" 4 | show
    $i.stderr | lines | last 3 | show
    false
  }
}

# chunks NAME ARGS: iets in two chunks against one nix-eval-jobs run.
def chunks [ctx: record, name: string, args: list<string>]: nothing -> bool {
  let work = $ctx.work
  let n = (nej $ctx ...$args | complete)
  let i0 = (^($ctx.iets) eval-jobs ...(impure-for $args) --chunks 2 --chunk 0 ...$args | complete)
  let i = if $i0.exit_code == 0 {
    let i1 = (^($ctx.iets) eval-jobs ...(impure-for $args) --chunks 2 --chunk 1 ...$args | complete)
    { stdout: ($i0.stdout + $i1.stdout), exit_code: $i1.exit_code }
  } else {
    $i0
  }
  let nn = ($n.stdout | norm)
  let ii = ($i.stdout | norm)
  if $n.exit_code == $i.exit_code and $nn == $ii {
    print $"ok   ($name) \(($nn | length) lines\)"
    true
  } else {
    print $"FAIL ($name) \(exit nej=($n.exit_code) iets=($i.exit_code)\)"
    $nn | str join "\n" | save -f $"($work)/($name).nej.norm"
    $ii | str join "\n" | save -f $"($work)/($name).iets.norm"
    diff-head $"($work)/($name).nej.norm" $"($work)/($name).iets.norm" 6 | show
    false
  }
}

# expect-fail NAME ARGS: both tools must exit non-zero.
def expect-fail [ctx: record, name: string, args: list<string>]: nothing -> bool {
  let nrc = (nej $ctx ...$args | complete | get exit_code)
  let irc = (^($ctx.iets) eval-jobs ...(impure-for $args) ...$args | complete | get exit_code)
  if $nrc != 0 and $irc != 0 {
    print $"ok   ($name) \(both fail\)"
    true
  } else {
    print $"FAIL ($name) \(exit nej=($nrc) iets=($irc)\)"
    false
  }
}

# session: every internal-json record of one run, the workers' `trace:`
# lines and the main process's summary, carries one `sid`; IETS_SESSION_ID
# sets it (docs/reference/errors.md). iets only: nix-eval-jobs has no such field.
def session [ctx: record]: nothing -> bool {
  let e = '{ a = builtins.trace "a" (derivation { name = "a"; system = "x"; builder = ":"; }); b = builtins.trace "b" (derivation { name = "b"; system = "x"; builder = ":"; }); }'
  let sids = {|| ^($ctx.iets) eval-jobs --workers 2 --no-instantiate --log-format internal-json --expr $e | complete | get stderr | lines | where ($it | str starts-with "@nix ") | each { str substring 5.. | from json | get -o sid } }
  let own = (do $sids)
  let given = (with-env {IETS_SESSION_ID: "run-42"} { do $sids })
  let ok = ($own | length) == 3 and ($own | uniq | length) == 1 and ($own.0 =~ '^[0-9a-f]{8}-[0-9a-f]{4}-7') and ($given | uniq) == ["run-42"]
  if $ok { print "ok   session (3 records, one sid)" } else { print $"FAIL session: own ($own | to json -r), given ($given | to json -r)" }
  $ok
}

# ignored: `--verbose` and `--debug` change nothing, so each is one warning
# with the reason, from the main process only; `--max-jobs` holds, since
# nothing is built, and `--quiet` works (docs/reference/divergences.md).
def ignored [ctx: record]: nothing -> bool {
  let r = (^($ctx.iets) eval-jobs --workers 2 --verbose --debug --verbose --max-jobs 1 --quiet --no-instantiate ci.nix | complete)
  let warned = ($r.stderr | lines | where ($it | str starts-with "warning: "))
  let want = ["--verbose" "--debug"] | each {|f| $"warning: '($f)' is ignored: iets has one log level, the one Nix starts at" }
  let ok = $r.exit_code == 0 and $warned == $want
  if $ok { print "ok   ignored (one warning per flag, with the reason)" } else { print $"FAIL ignored: ($r.stderr)" }
  $ok
}

# options: `--iets-option` leaves the argv before any command sees it, so
# the collector must put it back on the argv of every worker process.
# `cache-dir` shows it: only a worker that read the setting makes the
# directory. iets only: nix-eval-jobs has no such flag.
def options [ctx: record]: nothing -> bool {
  let dir = $"($ctx.work)/opt-cache"
  let r = (^($ctx.iets) eval-jobs --iets-option cache-dir $dir --workers 2 --no-instantiate ci.nix | complete)
  let ok = $r.exit_code == 0 and ($dir | path exists)
  if $ok { print "ok   options (--iets-option reaches the workers)" } else { print $"FAIL options: exit ($r.exit_code), ($dir) ($dir | path exists): ($r.stderr)" }
  $ok
}

def run-all [ctx: record, np: string]: nothing -> int {
  # the fixture's lock, outside the flake: --reference-lock-file must read it
  let alt_lock = $"($ctx.work)/alt.lock"
  cp ./flake/flake.lock $alt_lock
  let cases = [
    [check basic [ci.nix]]
    [check meta [--meta ci.nix]]
    [check force-recurse [--meta --force-recurse ci.nix]]
    [check no-instantiate [--meta --no-instantiate --gc-roots-dir GCROOTS ci.nix]]
    [check input-drvs [--show-input-drvs ci.nix]]
    [check gc-roots [--gc-roots-dir GCROOTS --meta ci.nix]]
    [check apply [--apply 'drv: { the-name = drv.name; version = drv.version or null; outs = drv.outputs or [ "out" ]; }' ci.nix]]
    [check apply-error [--apply 'drv: drv.name' ci.nix]]
    [check select [--select 'jobs: { inherit (jobs) builtJob recurse; }' --meta ci.nix]]
    [check expr [--expr 'import ./ci.nix { system = "aarch64-linux"; }' --meta]]
    [check expr-E [-E '(import ./ci.nix { }).recurse' --meta]]
    [check argstr [--argstr system aarch64-linux ci.nix]]
    [check arg [--arg system '"i686-linux"' ci.nix]]
    [check arg-from-file [--arg-from-file system system.txt ci.nix]]
    [check arg-from-stdin [--arg-from-stdin system ci.nix] system.txt]
    [check include [--include $"nixpkgs=($np)" --expr '{ inherit ((import <nixpkgs> { }).hello) drvPath; }' --no-instantiate]]
    [check option [--option allow-import-from-derivation false --show-trace --quiet --max-jobs 1 --log-format raw ci.nix]]
    [check verbose [--verbose --debug ci.nix]]
    [check log-format-json [--log-format internal-json --show-trace ci.nix]]
    [check impure [--impure --expr '{ x = derivation { name = "x-${builtins.currentSystem}"; system = builtins.currentSystem; builder = ":"; }; }']]
    [check flake [--flake ./flake#hydraJobs --meta --gc-roots-dir GCROOTS]]
    [check flake-whole [--flake ./flake]]
    [check flake-select [--flake ./flake --select 'flake: flake.outputs.hydraJobs' --meta]]
    [check flake-select-fragment [--flake ./flake#hydraJobs --select 'outputs: { inherit (outputs) builtJob; }']]
    [check flake-legacy [--flake ./flake#legacyPackages.x86_64-linux.success --meta]]
    [check flake-reference-lock [--flake ./flake#hydraJobs --reference-lock-file $alt_lock --meta]]
    [check constituents [--constituents --gc-roots-dir GCROOTS --meta aggregates.nix]]
    [check constituents-flake [--constituents --flake ./flake#legacyPackages.x86_64-linux --meta]]
    [check constituents-select [--constituents --select 'j: { inherit (j) success; }' aggregates.nix]]
    [check ca [--extra-experimental-features ca-derivations --no-instantiate --meta ca.nix]]
    [check ca-workers [--extra-experimental-features ca-derivations --no-instantiate --workers 3 ca.nix]]
    [check cache-status [--check-cache-status --meta -I $"nixpkgs=($np)" cache.nix]]
    [check cache-status-constituents [--check-cache-status --constituents --gc-roots-dir GCROOTS aggregates.nix]]
    [check restart [--max-memory-size 1 --workers 2 --meta --gc-roots-dir GCROOTS ci.nix]]
    [check restart-1 [--max-memory-size 1 --workers 1 --meta ci.nix]]
    [check restart-big [--max-memory-size 100000 --workers 3 --meta --show-input-drvs ci.nix]]
    [check restart-constituents [--max-memory-size 1 --workers 2 --constituents --check-cache-status --gc-roots-dir GCROOTS aggregates.nix]]
    [check restart-flake [--max-memory-size 1 --workers 2 --flake ./flake#hydraJobs --apply 'd: { n = d.name; }']]
    [check restart-no-instantiate [--max-memory-size 1 --workers 2 --no-instantiate --meta ci.nix]]
    [check one-process [--max-memory-size 0 --force-recurse --meta --gc-roots-dir GCROOTS ci.nix]]
    [chunks chunks [--meta ci.nix]]
    [expect-fail no-instantiate-constituents [--no-instantiate --constituents aggregates.nix]]
    [expect-fail no-instantiate-cache [--no-instantiate --check-cache-status ci.nix]]
    [expect-fail no-instantiate-input-drvs [--no-instantiate --show-input-drvs ci.nix]]
    [expect-fail select-not-attrs [--select 'x: "str"' ci.nix]]
    [expect-fail no-expr []]
    [session session []]
    [ignored ignored []]
    [options options []]
  ]
  let res = ($cases | each {|c|
    # bare `false` and numbers in a list literal are bool/int values
    let args = ($c.2 | each { into string })
    let stdin = ($c | get -o 3)
    let extra = if $stdin == null { {} } else { {STDIN: $stdin} }
    with-env $extra {
      match $c.0 {
        "check" => (check $ctx $c.1 $args)
        "chunks" => (chunks $ctx $c.1 $args)
        "expect-fail" => (expect-fail $ctx $c.1 $args)
        "session" => (session $ctx)
        "ignored" => (ignored $ctx)
        "options" => (options $ctx)
      }
    }
  })
  let fail = ($res | where not $it | length)
  print $"passed ($res | where $it | length), failed ($fail)"
  if $fail == 0 { 0 } else { 1 }
}

def main [] {
  let nej = ($env.NEJ? | default $"($env.HOME)/tmp/nej/bin/nix-eval-jobs")
  require-real-nix $nej
  let ctx = {
    iets: (iets-bin)
    work: (isolate evaljobs-flags)
    nej: $nej
  }
  cd ($ROOT | path join tests/evaljobs)
  run-in $ctx.work { run-all $ctx (nixpkgs) }
}
